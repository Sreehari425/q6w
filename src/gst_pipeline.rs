//! GStreamer video pipeline.
//!
//! Two decoding strategies are tried in order:
//!  1. **Hardware (VAAPI)** — `vaapidecodebin` keeps frames in VRAM, uses  
//!     `vaapipostproc` to scale and convert before pushing to the CPU.
//!  2. **Software fallback** — `uridecodebin` with a `deep-element-added`  
//!     hook that clamps every interior `multiqueue` to 2 buffers so  
//!     decoded-frame RSS stays at ≈ 2 × frame_size (63 MB at 4K BGRA).
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
}

impl Pipeline {
    /// Build the decode pipeline for `path` at `width × height`.
    ///
    /// Modern GStreamer (1.22+) auto-selects VA-API hardware decoders
    /// (`vah264dec`, `vaav1dec`, etc.) through `decodebin3` inside
    /// `uridecodebin` — no need for the deprecated `vaapidecodebin`.
    /// We use a single pipeline; hardware vs software is fully automatic.
    pub fn new(path: &str, volume: f64, width: i32, height: i32, fps: Option<i32>) -> Self {
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
        if let Some(p) = Self::try_vaapi(&uri, volume, width, height, fps) {
            eprintln!("q6w: using VAAPI hardware decoder");
            return p;
        }
        eprintln!("q6w: VAAPI not available — using software decoder");
        Self::build_software(&uri, volume, width, height, fps)
    }

    // ── Hardware path (VAAPI) ─────────────────────────────────────────────────
    //
    // Pipeline:
    //   uridecodebin  →  queue(2)  →  vapostproc (scale + colorspace via GPU)
    //   →  videorate  →  capsfilter (BGRA WxH)  →  appsink
    //
    // We quick-check that `vapostproc` exists. If so, we build a pipeline that
    // converts color and scales directly on the GPU, avoiding the massive CPU
    // penalty of downloading a 4K 60fps frame to software memory for conversion.

    fn try_vaapi(
        uri: &str,
        volume: f64,
        width: i32,
        height: i32,
        fps: Option<i32>,
    ) -> Option<Pipeline> {
        // Quick check for modern VA-API postprocessing plugin
        gst::ElementFactory::find("vapostproc")?;

        let pipeline = gst::Pipeline::default();

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

        // vapostproc does in-VRAM scale + BGRA conversion via VA-API.
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
            .drop(true) // Drop frames if the GPU render loop falls behind
            .sync(true) // Lock strictly to the presentation clock
            .build();

        let (aqueue, aconvert, aresample, vol, audiosink) = Self::make_audio_chain(volume)?;

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

        let vqueue_w = vqueue.downgrade();
        let aqueue_w = aqueue.downgrade();
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
            } else if name.starts_with("audio/") {
                if let Some(q) = aqueue_w.upgrade() {
                    let sink = q.static_pad("sink").unwrap();
                    if !sink.is_linked() {
                        pad.link(&sink).ok();
                    }
                }
            }
        });

        let bus = pipeline.bus().expect("no bus");
        Some(Pipeline {
            pipeline,
            appsink,
            bus,
        })
    }

    // ── Software path (uridecodebin) ──────────────────────────────────────────
    //
    // Pipeline:
    //   uridecodebin  →  [deep-element-added: cap internal multiqueue to 2]
    //   queue(2)  →  videoconvert  →  videoscale  →  capsfilter(BGRA WxH)
    //   →  appsink
    //
    // Without the deep-element-added hook, decodebin3's internal multiqueue
    // uses max-size-time=2s defaults which at 4K60 BGRA ≈ 3.8 GB RSS.

    fn build_software(
        uri: &str,
        volume: f64,
        width: i32,
        height: i32,
        fps: Option<i32>,
    ) -> Pipeline {
        let pipeline = gst::Pipeline::default();

        // Intercept every multiqueue that decodebin3 creates and clamp it —
        // without this, decodebin3 defaults to max-size-time=2s which at
        // 4K60 BGRA ≈ 3.8 GB RSS.
        // NOTE: only cap `multiqueue`, NOT `queue` — capping internal queues
        // can deadlock the pipeline.
        pipeline.connect("deep-element-added", false, |args| {
            let element: gst::Element = args[2].get().expect("deep-element-added arg");
            if element
                .factory()
                .map(|f| f.name() == "multiqueue")
                .unwrap_or(false)
            {
                element.set_property("max-size-buffers", 2u32);
                element.set_property("max-size-bytes", 0u32);
                element.set_property("max-size-time", 0u64);
            }
            None
        });

        let src = gst::ElementFactory::make("uridecodebin")
            .property("uri", uri)
            .property("buffer-size", 2i32 * 1024 * 1024)
            .build()
            .expect("uridecodebin not found");

        // Hard-cap our own queue to 2 frames, NO leaky!
        // We WANT it to stall the decoder when full — this backpressures the whole pipeline
        let vqueue = gst::ElementFactory::make("queue")
            .property("max-size-buffers", 2u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .build()
            .expect("queue not found");

        // Convert the decode format to something standard, then scale
        let convert = gst::ElementFactory::make("videoconvert")
            .build()
            .expect("videoconvert not found");

        let scale = gst::ElementFactory::make("videoscale")
            .property("add-borders", false)
            .build()
            .expect("videoscale not found");

        let rate = gst::ElementFactory::make("videorate")
            .property("drop-only", true)
            .build()
            .expect("videorate not found");

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
            .drop(true) // Drop frames if the GPU render loop falls behind
            .sync(true) // Lock strictly to the presentation clock
            .build();

        let (aqueue, aconvert, aresample, vol, audiosink) =
            Self::make_audio_chain(volume).expect("audio chain elements not found");

        pipeline
            .add_many([
                &src,
                &vqueue,
                &convert,
                &scale,
                &rate,
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
            &convert,
            &scale,
            &rate,
            &cfilter,
            appsink.upcast_ref(),
        ])
        .expect("failed to link video chain");
        gst::Element::link_many([&aqueue, &aconvert, &aresample, &vol, &audiosink])
            .expect("failed to link audio chain");

        let vqueue_w = vqueue.downgrade();
        let aqueue_w = aqueue.downgrade();
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
            } else if name.starts_with("audio/") {
                if let Some(q) = aqueue_w.upgrade() {
                    let sink = q.static_pad("sink").unwrap();
                    if !sink.is_linked() {
                        pad.link(&sink).ok();
                    }
                }
            }
        });

        let bus = pipeline.bus().expect("no bus");
        Pipeline {
            pipeline,
            appsink,
            bus,
        }
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
            .property("max-size-time", 1_000_000_000u64) // 1 second of audio buffer maximal
            .build()
            .ok()?;
        let aconvert = gst::ElementFactory::make("audioconvert").build().ok()?;
        let aresample = gst::ElementFactory::make("audioresample").build().ok()?;
        let vol = gst::ElementFactory::make("volume")
            .property("volume", volume.clamp(0.0, 1.0))
            .build()
            .ok()?;
        let audiosink = gst::ElementFactory::make("autoaudiosink")
            .property("sync", true) // Audio clock drives the master pipeline time
            .build()
            .ok()?;
        Some((aqueue, aconvert, aresample, vol, audiosink))
    }

    // ── Playback control ─────────────────────────────────────────────────────

    pub fn play(&self) {
        self.pipeline.set_state(gst::State::Playing).ok();
    }

    pub fn pause(&self) {
        self.pipeline.set_state(gst::State::Paused).ok();
    }

    pub fn resume(&self) {
        self.pipeline.set_state(gst::State::Playing).ok();
    }

    // ── Zero-copy frame access ───────────────────────────────────────────────

    /// Drain the appsink and process only the **latest** available frame.
    /// For a wallpaper we never need stale frames — only the freshest one.
    pub fn with_latest_frame<F: FnOnce(&[u8], i32, i32)>(&self, f: F) {
        // Pull all available samples, keep only the last one
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
                    // Full NULL → Playing reset is the most reliable loop
                    // for both uridecodebin and vaapidecodebin.  A bare
                    // flush-seek on a fully drained pipeline frequently
                    // hangs.
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
