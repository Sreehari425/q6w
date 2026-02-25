//! GStreamer video pipeline.
//!
//! Two decoding strategies are tried in order:
//!  1. **Hardware (VAAPI)** — `uridecodebin` auto-selects `vah264dec` etc.,
//!     then `vapostproc` scales and converts in VRAM before CPU readback.
//!  2. **Software fallback** — `uridecodebin` with CPU `videoscale` +
//!     `videoconvert`.  A `deep-element-added` hook clamps every interior
//!     queue to ≤ 20 MB so decoded-frame RSS stays low.
//!
//! Frame delivery is **zero-copy**: callers receive a `&[u8]` slice mapped
//! directly from the GstBuffer — no `Vec` is ever allocated.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

// ─── Public pipeline wrapper ─────────────────────────────────────────────────

pub struct Pipeline {
    pipeline: gst::Pipeline,
    appsink: gst_app::AppSink,
    bus: gst::Bus,
    /// `true` when VAAPI was unavailable and the software path is active.
    is_software: bool,
}

impl Pipeline {
    /// Build the decode pipeline for `path` at `width × height`.
    ///
    /// When `enable_audio` is `true`, an audio playback chain is added;
    /// otherwise audio pads from uridecodebin are sent to `fakesink`.
    pub fn new(
        path: &str,
        enable_audio: bool,
        volume: f64,
        width: i32,
        height: i32,
        fps: Option<i32>,
    ) -> Self {
        gst::init().expect(
            "q6w: GStreamer init failed — is GStreamer installed?\n\
             Arch: sudo pacman -S gstreamer gst-plugins-base gst-plugins-good \
             gst-plugins-bad",
        );

        let uri = if path.starts_with('/') {
            format!("file://{path}")
        } else {
            let cwd = std::env::current_dir().unwrap_or_default();
            format!("file://{}/{path}", cwd.display())
        };

        // Try hardware path, fall back to software
        if let Some(p) = Self::try_vaapi(&uri, enable_audio, volume, width, height, fps) {
            eprintln!("q6w: using VAAPI hardware decoder");
            return p;
        }

        // No VAAPI — warn and continue with software decode
        eprintln!("q6w: WARNING: VAAPI hardware decoding is not available.");
        eprintln!(
            "q6w:   Possible causes: missing VA-API driver, NVIDIA without nouveau/nvidia-vaapi-driver,"
        );
        eprintln!("q6w:   or unsupported GPU. Run `vainfo` to diagnose.");
        eprintln!("q6w:   Falling back to software decoding (higher CPU and RAM usage).");

        Self::build_software(&uri, enable_audio, volume, width, height, fps)
    }

    // ── Shared: install deep-element-added hook ──────────────────────────────
    //
    // Clamp every internal `multiqueue` to 2 buffers and every internal
    // `queue` to 20 MB.  Without this, decodebin3 defaults to buffering
    // 2 seconds of decoded 4K frames ≈ 3.8 GB RSS.

    fn install_queue_clamp(pipeline: &gst::Pipeline) {
        pipeline.connect("deep-element-added", false, |args| {
            let element: gst::Element = args[2].get().expect("deep-element-added arg");
            if let Some(name) = element.factory().map(|f| f.name()) {
                if name == "multiqueue" {
                    element.set_property("max-size-buffers", 2u32);
                    element.set_property("max-size-bytes", 0u32);
                    element.set_property("max-size-time", 0u64);
                } else if name == "queue" {
                    // 20 MB holds thousands of encoded NAL units (no decoder
                    // starvation) but only ~1 decoded 4K BGRA frame.
                    element.set_property("max-size-bytes", 20 * 1024 * 1024u32);
                    element.set_property("max-size-buffers", 0u32);
                    element.set_property("max-size-time", 0u64);
                }
            }
            None
        });
    }

    // ── Hardware path (VAAPI) ─────────────────────────────────────────────────
    //
    // Pipeline:
    //   uridecodebin  →  queue(2)  →  vapostproc (GPU scale + colorspace)
    //   →  videorate  →  capsfilter(BGRA WxH)  →  appsink

    fn try_vaapi(
        uri: &str,
        enable_audio: bool,
        volume: f64,
        width: i32,
        height: i32,
        fps: Option<i32>,
    ) -> Option<Pipeline> {
        gst::ElementFactory::find("vapostproc")?;

        let pipeline = gst::Pipeline::default();
        Self::install_queue_clamp(&pipeline);

        let src = gst::ElementFactory::make("uridecodebin")
            .property("uri", uri)
            .build()
            .ok()?;

        let vqueue = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 2u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .build()
            .ok()?;

        let postproc = gst::ElementFactory::make("vapostproc").build().ok()?;

        let rate = gst::ElementFactory::make("videorate")
            .property("drop-only", true)
            .build()
            .ok()?;

        let mut caps_builder = gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", width)
            .field("height", height);
        if let Some(f) = fps {
            caps_builder = caps_builder.field("framerate", gst::Fraction::new(f, 1));
        }
        let out_caps = caps_builder.build();

        let cfilter = gst::ElementFactory::make("capsfilter")
            .property("caps", &out_caps)
            .build()
            .ok()?;

        let appsink = gst_app::AppSink::builder()
            .max_buffers(2)
            .drop(true)
            .sync(true)
            .build();

        // Always attach a real audio sink so GStreamer has a clock provider.
        // Without -a (audio), volume is set to 0 — silent but clocked.
        let effective_volume = if enable_audio { volume } else { 0.0 };
        let (aqueue, aconvert, aresample, vol, audiosink) =
            Self::make_audio_chain(effective_volume)?;

        pipeline
            .add_many([
                &src,
                &vqueue,
                &postproc,
                &rate,
                &cfilter,
                appsink.upcast_ref::<gst::Element>(),
                &aqueue,
                &aconvert,
                &aresample,
                &vol,
                &audiosink,
            ])
            .ok()?;

        gst::Element::link_many([&vqueue, &postproc, &rate, &cfilter, appsink.upcast_ref()])
            .ok()?;
        gst::Element::link_many([&aqueue, &aconvert, &aresample, &vol, &audiosink]).ok()?;

        Self::wire_pads(&src, &vqueue, Some(&aqueue));

        let bus = pipeline.bus().expect("no bus");
        Some(Pipeline {
            pipeline,
            appsink,
            bus,
            is_software: false,
        })
    }

    // ── Software path (uridecodebin) ──────────────────────────────────────────
    //
    // Pipeline:
    //   uridecodebin  →  queue(2)  →  videoscale  →  videorate
    //   →  videoconvert  →  capsfilter(BGRA WxH)  →  appsink
    //
    // videoscale is placed BEFORE videoconvert so that scaling happens on the
    // smaller YUV frames (1.5 B/px) rather than on the 4× larger BGRA frames.

    fn build_software(
        uri: &str,
        enable_audio: bool,
        volume: f64,
        width: i32,
        height: i32,
        fps: Option<i32>,
    ) -> Pipeline {
        let pipeline = gst::Pipeline::default();
        Self::install_queue_clamp(&pipeline);

        let src = gst::ElementFactory::make("uridecodebin")
            .property("uri", uri)
            .property("buffer-size", 2i32 * 1024 * 1024)
            .build()
            .expect("uridecodebin not found");

        let vqueue = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 2u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .build()
            .expect("queue not found");

        let scale = gst::ElementFactory::make("videoscale")
            .property("add-borders", false)
            .build()
            .expect("videoscale not found");

        let rate = gst::ElementFactory::make("videorate")
            .property("drop-only", true)
            .build()
            .expect("videorate not found");

        let convert = gst::ElementFactory::make("videoconvert")
            .build()
            .expect("videoconvert not found");

        let mut caps_builder = gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", width)
            .field("height", height);
        if let Some(f) = fps {
            caps_builder = caps_builder.field("framerate", gst::Fraction::new(f, 1));
        }
        let out_caps = caps_builder.build();

        let cfilter = gst::ElementFactory::make("capsfilter")
            .property("caps", &out_caps)
            .build()
            .expect("capsfilter not found");

        let appsink = gst_app::AppSink::builder()
            .max_buffers(2)
            .drop(true)
            .sync(true)
            .build();

        // Always attach a real audio sink so GStreamer has a clock provider.
        // Without -a (audio), volume is set to 0 — silent but clocked.
        let effective_volume = if enable_audio { volume } else { 0.0 };
        let (aqueue, aconvert, aresample, vol, audiosink) =
            Self::make_audio_chain(effective_volume).expect("audio chain elements not found");

        pipeline
            .add_many([
                &src,
                &vqueue,
                &scale,
                &rate,
                &convert,
                &cfilter,
                appsink.upcast_ref::<gst::Element>(),
                &aqueue,
                &aconvert,
                &aresample,
                &vol,
                &audiosink,
            ])
            .expect("failed to add elements");

        gst::Element::link_many([
            &vqueue,
            &scale,
            &rate,
            &convert,
            &cfilter,
            appsink.upcast_ref(),
        ])
        .expect("failed to link video chain");
        gst::Element::link_many([&aqueue, &aconvert, &aresample, &vol, &audiosink])
            .expect("failed to link audio chain");

        Self::wire_pads(&src, &vqueue, Some(&aqueue));

        let bus = pipeline.bus().expect("no bus");
        Pipeline {
            pipeline,
            appsink,
            bus,
            is_software: true,
        }
    }

    // ── Shared: wire uridecodebin pads ───────────────────────────────────────

    fn wire_pads(
        src: &gst::Element,
        vqueue: &gst::Element,
        audio_sink_elem: Option<&gst::Element>,
    ) {
        let vqueue_w = vqueue.downgrade();
        let audio_w = audio_sink_elem.map(|e| e.downgrade());
        src.connect_pad_added(move |_, pad| {
            let Some(caps) = pad.current_caps() else {
                return;
            };
            let Some(s) = caps.structure(0) else { return };
            let name = s.name();
            if name.starts_with("video/") {
                if let Some(q) = vqueue_w.upgrade() {
                    let sink = q.static_pad("sink").unwrap();
                    if !sink.is_linked() {
                        pad.link(&sink).ok();
                    }
                }
            } else if name.starts_with("audio/")
                && let Some(ref w) = audio_w
                    && let Some(q) = w.upgrade() {
                        let sink = q.static_pad("sink").unwrap();
                        if !sink.is_linked() {
                            pad.link(&sink).ok();
                        }
                    }
        });
    }

    // ── Shared audio chain builder ────────────────────────────────────────────

    fn make_audio_chain(
        volume: f64,
    ) -> Option<(
        gst::Element,
        gst::Element,
        gst::Element,
        gst::Element,
        gst::Element,
    )> {
        let aqueue = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 1_000_000_000u64)
            .build()
            .ok()?;
        let aconvert = gst::ElementFactory::make("audioconvert").build().ok()?;
        let aresample = gst::ElementFactory::make("audioresample").build().ok()?;
        let vol = gst::ElementFactory::make("volume")
            .property("volume", volume.clamp(0.0, 1.0))
            .build()
            .ok()?;
        let audiosink = gst::ElementFactory::make("autoaudiosink")
            .property("sync", true)
            .build()
            .ok()?;
        Some((aqueue, aconvert, aresample, vol, audiosink))
    }

    // ── Playback control ─────────────────────────────────────────────────────

    /// Returns `true` if the pipeline is using the CPU software decoder.
    pub fn is_software_fallback(&self) -> bool {
        self.is_software
    }

    pub fn play(&self) {
        self.pipeline.set_state(gst::State::Playing).ok();
    }

    pub fn pause(&self) {
        if let Err(e) = self.pipeline.set_state(gst::State::Paused) {
            eprintln!("q6w: failed to pause pipeline: {e:?}");
        }
    }

    pub fn resume(&self) {
        if let Err(e) = self.pipeline.set_state(gst::State::Playing) {
            eprintln!("q6w: failed to resume pipeline: {e:?}");
        }
    }

    // ── Zero-copy frame access ───────────────────────────────────────────────

    /// Drain the appsink and process only the **latest** available frame.
    /// For a wallpaper we never need stale frames — only the freshest one.
    pub fn with_latest_frame<F: FnOnce(&[u8], i32, i32)>(&self, f: F) {
        let mut last = self.appsink.try_pull_sample(gst::ClockTime::ZERO);
        if last.is_none() {
            return;
        }
        while let Some(newer) = self.appsink.try_pull_sample(gst::ClockTime::ZERO) {
            last = Some(newer);
        }
        let sample = last.unwrap();
        let Some(buffer) = sample.buffer() else {
            return;
        };
        let Some(caps) = sample.caps() else { return };
        let Some(s) = caps.structure(0) else { return };
        let Ok(w) = s.get::<i32>("width") else { return };
        let Ok(h) = s.get::<i32>("height") else {
            return;
        };
        let Ok(map) = buffer.map_readable() else {
            return;
        };
        f(map.as_slice(), w, h);
    }

    // ── Bus monitoring ───────────────────────────────────────────────────────

    /// Drain pending bus messages.  Returns `true` on fatal error.
    pub fn handle_bus(&self) -> bool {
        while let Some(msg) = self.bus.pop() {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => {
                    self.pipeline.set_state(gst::State::Null).ok();
                    self.pipeline.set_state(gst::State::Playing).ok();
                }
                MessageView::Error(e) => {
                    eprintln!(
                        "q6w: GStreamer error: {}\n  debug: {}",
                        e.error(),
                        e.debug().unwrap_or_default(),
                    );
                    return true;
                }
                _ => {}
            }
        }
        false
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.pipeline.set_state(gst::State::Null).ok();
    }
}
