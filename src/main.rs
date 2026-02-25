mod app;
mod gpu_renderer;
mod gst_pipeline;

use std::ffi::c_void;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use clap::Parser;
use wayland_client::{Connection, Proxy, globals::registry_queue_init};
use wayland_protocols_wlr::{
    foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
    layer_shell::v1::client::zwlr_layer_shell_v1::ZwlrLayerShellV1,
};

use app::State;
use gpu_renderer::GpuRenderer;
use gst_pipeline::Pipeline;

// ─── CLI ─────────────────────────────────────────────────────────────────────

/// q6w — GStreamer video wallpaper for Wayland
///
/// Plays a video file as the desktop background on any compositor that
/// implements zwlr_layer_shell_v1 (Sway, Hyprland, river, labwc, …).
///
/// Video decoding runs in GStreamer background threads.  Frames are
/// uploaded directly from the mapped GstBuffer to a GPU texture via wgpu
/// — no Vec allocation, no CPU copy.  VAAPI hardware decoding is used
/// automatically when available; software fallback otherwise.
#[derive(Parser, Debug)]
#[command(name = "q6w", author, version, about, long_about = None)]
struct Args {
    /// Path to the video file
    #[arg(short, long, value_name = "FILE")]
    file: PathBuf,

    /// Zero-based screen/output index (reserved — compositor picks output)
    #[arg(short, long, value_name = "N", default_value_t = 0)]
    screen: i32,

    /// Enable audio playback (disabled by default)
    #[arg(short, long)]
    audio: bool,

    /// Audio volume: 0.0 = mute, 1.0 = full
    #[arg(long, value_name = "VOLUME", default_value_t = 1.0)]
    volume: f32,

    /// Target framerate limit (e.g. 30). Drops frames to hit the limit.
    #[arg(long, value_name = "FPS")]
    fps: Option<i32>,

    /// Disable the software-fallback guard rail.
    ///
    /// By default, q6w refuses to software-decode videos larger than
    /// 1920×1080 without VAAPI, because CPU and memory usage can be
    /// extreme.  Pass this flag to allow it anyway.
    #[arg(long)]
    no_fallback_guard: bool,
}

// ─── Wayland raw-pointer extraction ─────────────────────────────────────────

/// Return the raw `wl_display *` C pointer.
///
/// On Linux, wayland-client uses the system (libwayland-client.so) backend
/// where every proxy is a `*mut wl_proxy`.  Adding `wayland-backend` with
/// the `client_system` feature exposes `ObjectId::as_ptr()`.
fn display_ptr(conn: &Connection) -> *mut c_void {
    conn.display().id().as_ptr().cast()
}

/// Return the raw `wl_surface *` C pointer for use with wgpu.
fn surface_ptr(state: &State) -> *mut c_void {
    state
        .surface
        .as_ref()
        .expect("surface not yet created")
        .id()
        .as_ptr()
        .cast()
}

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() {
    let args = Args::parse();

    if !args.file.exists() {
        eprintln!("q6w: file not found: {}", args.file.display());
        std::process::exit(1);
    }

    let abs_path = args
        .file
        .canonicalize()
        .unwrap_or_else(|_| args.file.clone());
    let path_str = abs_path.to_string_lossy().into_owned();
    let enable_audio = args.audio;
    let volume = args.volume.clamp(0.0, 1.0) as f64;

    // ── 1. Connect to the Wayland compositor ─────────────────────────────────
    let conn = Connection::connect_to_env()
        .expect("q6w: cannot connect to Wayland — is WAYLAND_DISPLAY set?");

    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).expect("q6w: Wayland registry init failed");

    let qh = queue.handle();
    let mut state = State::new();

    state.compositor = globals.bind(&qh, 4..=6, ()).ok();
    // wl_shm is intentionally NOT bound: the GPU path renders via wgpu/Vulkan
    // directly onto the wl_surface swapchain.  If state.shm stays None, the
    // configure handler in app.rs will skip ShmPool creation (~120 MB saved).
    state.layer_shell = globals.bind::<ZwlrLayerShellV1, _, _>(&qh, 1..=4, ()).ok();
    state.toplevel_mgr = globals
        .bind::<ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ())
        .ok();

    if state.compositor.is_none() {
        eprintln!("q6w: wl_compositor not found");
        std::process::exit(1);
    }
    if state.layer_shell.is_none() {
        eprintln!(
            "q6w: zwlr_layer_shell_v1 not found\n     Supported: Sway, Hyprland, river, labwc, …"
        );
        std::process::exit(1);
    }
    if state.toplevel_mgr.is_none() {
        eprintln!(
            "q6w: zwlr_foreign_toplevel_management_v1 not available — pause-on-fullscreen disabled"
        );
    }

    // ── 2. Create layer surface + wait for configure ──────────────────────────
    if !state.create_layer_surface(&qh) {
        std::process::exit(1);
    }

    queue
        .roundtrip(&mut state)
        .expect("Wayland roundtrip failed");

    if !state.configured {
        eprintln!("q6w: layer-surface configure event not received — aborting");
        std::process::exit(1);
    }

    // ── 3. Create wgpu GPU renderer ───────────────────────────────────────────
    //
    // Created AFTER configure so we have the exact monitor dimensions.
    // Frames go: GstBuffer map → queue.write_texture() → GPU → present
    // — no Vec, no memcpy, no SHM.
    let renderer = unsafe {
        GpuRenderer::new(
            display_ptr(&conn),
            surface_ptr(&state),
            state.buf_w as u32,
            state.buf_h as u32,
        )
        .expect("q6w: failed to create GPU renderer — check Vulkan drivers")
    };

    // ── 4. Start GStreamer pipeline ───────────────────────────────────────────
    let pipeline = Pipeline::new(
        &path_str,
        enable_audio,
        volume,
        state.buf_w,
        state.buf_h,
        args.fps,
    );

    // ── 4a. Software-fallback guard rail ──────────────────────────────────────
    //
    // Without VAAPI, decoding high-resolution video on the CPU can saturate
    // all cores and consume multiple GB of RAM.  Block by default; the user
    // can override with --no-fallback-guard.
    if pipeline.is_software_fallback() {
        let pixels = (state.buf_w as u64) * (state.buf_h as u64);
        let is_high_res = pixels > 1920 * 1080; // anything above Full HD

        if is_high_res && !args.no_fallback_guard {
            eprintln!();
            eprintln!(
                "q6w: Software decoding at {}×{} is not recommended.",
                state.buf_w, state.buf_h
            );
            eprintln!("q6w: Without VAAPI, high-resolution decode will cause excessive CPU");
            eprintln!("q6w: and memory usage. Consider downscaling the video or installing");
            eprintln!("q6w: the appropriate VA-API driver for your GPU.");
            eprintln!();
            eprintln!("q6w: To proceed anyway, re-run with --no-fallback-guard.");
            std::process::exit(1);
        }
    }

    pipeline.play();

    // ── 5. Main event loop ───────────────────────────────────────────────────
    //
    // pipeline.with_frame() maps the GstBuffer read-only and passes the slice
    // straight to renderer.upload_and_render() — zero intermediate copies.
    let mut was_paused = false;

    loop {
        // ── Pull the latest decoded frame → upload to GPU texture → present ──
        pipeline.with_latest_frame(|data, _w, _h| renderer.upload_and_render(data));

        // ── GStreamer bus: EOS → loop, errors → exit ─────────────────────────
        if pipeline.handle_bus() {
            break;
        }

        // ── Fullscreen detection: pause / resume ─────────────────────────────
        if state.paused_for_fs != was_paused {
            was_paused = state.paused_for_fs;
            if was_paused {
                pipeline.pause();
            } else {
                pipeline.resume();
            }
        }

        // ── Flush our Wayland writes ─────────────────────────────────────────
        conn.flush().ok();

        // ── Dispatch already-buffered Wayland events ─────────────────────────
        queue
            .dispatch_pending(&mut state)
            .expect("Wayland dispatch error");

        if !state.running {
            break;
        }

        // ── Wait up to 8 ms for new data on the Wayland socket ───────────────
        if let Some(guard) = queue.prepare_read() {
            let fd = guard.connection_fd().as_raw_fd();
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut pfd, 1, 8 /* ms */);
            }
            let _ = guard.read();
        }

        queue
            .dispatch_pending(&mut state)
            .expect("Wayland dispatch error");

        if !state.running {
            break;
        }
    }

    // Pipeline is dropped here → set_state(Null) via Drop impl
}
