// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2025 Sreehari Anil <sreehari7102008@gmail.com>

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
#[command(name = "q6w", author, version = env!("FULL_VERSION"), about, long_about = None)]
struct Args {
    /// Path to the video file
    #[arg(short, long, value_name = "FILE", required_unless_present = "license")]
    file: Option<PathBuf>,

    /// Enable audio playback (disabled by default)
    #[arg(short, long)]
    audio: bool,

    /// Audio volume: 0.0 = mute, 1.0 = full
    #[arg(long, value_name = "VOLUME", default_value_t = 1.0)]
    volume: f32,

    /// Mute audio when any window is focused or maximized (requires --audio)
    #[arg(long)]
    mute_on_window: bool,

    /// Pause video when any window is focused or maximized
    #[arg(long)]
    pause_on_window: bool,

    /// Disable automatic pause when a window goes fullscreen
    #[arg(long)]
    no_pause_on_fullscreen: bool,

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

    /// Print license information and source code links, then exit.
    #[arg(long)]
    license: bool,
}

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

fn main() {
    let args = Args::parse();

    if args.license {
        println!(
            "q6w is licensed under the GNU Affero General Public License v3.0 (AGPL-3.0-only)."
        );
        println!();
        println!("Source code:");
        println!("  GitHub   : https://github.com/Sreehari425/q6w");
        println!("  Codeberg : https://codeberg.org/sreehari425/q6w (mirror)");
        std::process::exit(0);
    }

    let file = args
        .file
        .expect("--file is required when --license is not used");

    if !file.exists() {
        eprintln!("q6w: file not found: {}", file.display());
        std::process::exit(1);
    }

    let abs_path = file.canonicalize().unwrap_or_else(|_| file.clone());
    let path_str = abs_path.to_string_lossy().into_owned();
    let enable_audio = args.audio;
    let volume = args.volume.clamp(0.0, 1.0) as f64;

    let conn = Connection::connect_to_env()
        .expect("q6w: cannot connect to Wayland — is WAYLAND_DISPLAY set?");

    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).expect("q6w: Wayland registry init failed");

    let qh = queue.handle();
    let mut state = State::new();

    state.compositor = globals.bind(&qh, 4..=6, ()).ok();
    // wl_shm intentionally NOT bound: GPU rendering via wgpu/Vulkan eliminates
    // the need for ShmPool (~120 MB saved).
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

    // Created after configure to use exact monitor dimensions.
    // Zero-copy path: GstBuffer → write_texture → GPU → present
    let renderer = unsafe {
        GpuRenderer::new(
            display_ptr(&conn),
            surface_ptr(&state),
            state.buf_w as u32,
            state.buf_h as u32,
        )
        .expect("q6w: failed to create GPU renderer — check Vulkan drivers")
    };

    let pipeline = Pipeline::new(
        &path_str,
        enable_audio,
        volume,
        state.buf_w,
        state.buf_h,
        args.fps,
    );

    // Without VAAPI, hi-res decoding can saturate CPU and consume GB of RAM.
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

    let mut was_paused_fs = false;
    let mut was_paused_window = false;
    let mut was_muted = false;

    loop {
        pipeline.with_latest_frame(|data, _w, _h| renderer.upload_and_render(data));

        if pipeline.handle_bus() {
            break;
        }

        if !args.no_pause_on_fullscreen {
            if state.paused_for_fs != was_paused_fs {
                was_paused_fs = state.paused_for_fs;
                if was_paused_fs {
                    pipeline.pause();
                } else if !state.paused_for_windows {
                    pipeline.resume();
                }
            }
        }

        if args.pause_on_window {
            if state.paused_for_windows != was_paused_window {
                was_paused_window = state.paused_for_windows;
                if was_paused_window {
                    pipeline.pause();
                } else if !state.paused_for_fs || args.no_pause_on_fullscreen {
                    pipeline.resume();
                }
            }
        }

        // Handle audio muting when windows are focused/maximized (opt-in)
        if args.mute_on_window && enable_audio {
            if state.muted_for_windows != was_muted {
                was_muted = state.muted_for_windows;
                if was_muted {
                    pipeline.mute();
                } else {
                    pipeline.unmute();
                }
            }
        }

        conn.flush().ok();

        queue
            .dispatch_pending(&mut state)
            .expect("Wayland dispatch error");

        if !state.running {
            break;
        }

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
