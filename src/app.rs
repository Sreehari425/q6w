// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2025 Sreehari Anil <sreehari7102008@gmail.com>

//! Wayland application state and all protocol `Dispatch` implementations.
//!
//! Nothing here touches C++ or Qt — pure Rust Wayland via `wayland-client`
//! and `wayland-protocols-wlr`.

use std::collections::HashMap;

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    backend::ObjectId,
    globals::GlobalListContents,
    protocol::{wl_compositor, wl_registry, wl_surface},
};
use wayland_protocols_wlr::{
    foreign_toplevel::v1::client::{
        zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
        zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
    },
    layer_shell::v1::client::{
        zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
        zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
    },
};

pub struct State {
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub layer_shell: Option<ZwlrLayerShellV1>,
    pub toplevel_mgr: Option<ZwlrForeignToplevelManagerV1>,

    pub surface: Option<wl_surface::WlSurface>,
    pub layer_surface: Option<ZwlrLayerSurfaceV1>,

    pub buf_w: i32,
    pub buf_h: i32,
    pub configured: bool,

    // Maps foreign-toplevel ObjectId → (was_fullscreen_active, was_active_or_maximized)
    toplevel_states: HashMap<ObjectId, (bool, bool)>,
    pub fullscreen_count: i32,
    pub paused_for_fs: bool,

    // Window activity tracking for audio muting
    pub active_or_maximized_count: i32,
    pub muted_for_windows: bool,

    pub running: bool,
}

impl State {
    pub fn new() -> Self {
        State {
            compositor: None,
            layer_shell: None,
            toplevel_mgr: None,
            surface: None,
            layer_surface: None,
            buf_w: 0,
            buf_h: 0,
            configured: false,
            toplevel_states: HashMap::new(),
            fullscreen_count: 0,
            paused_for_fs: false,
            active_or_maximized_count: 0,
            muted_for_windows: false,
            running: true,
        }
    }

    pub fn create_layer_surface(&mut self, qh: &QueueHandle<State>) -> bool {
        let compositor = match &self.compositor {
            Some(c) => c,
            None => {
                eprintln!("q6w: wl_compositor missing");
                return false;
            }
        };
        let layer_shell = match &self.layer_shell {
            Some(s) => s,
            None => {
                eprintln!(
                    "q6w: zwlr_layer_shell_v1 missing — compositor does not support it\n     (works on: Sway, Hyprland, river, labwc …)"
                );
                return false;
            }
        };

        let surface = compositor.create_surface(qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            None, // output: None = compositor picks
            zwlr_layer_shell_v1::Layer::Background,
            "wallpaper".to_owned(),
            qh,
            (),
        );

        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_size(0, 0);
        layer_surface.set_exclusive_zone(-1);
        layer_surface
            .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);

        surface.commit();

        self.surface = Some(surface);
        self.layer_surface = Some(layer_surface);
        true
    }

    fn on_fullscreen_enter(&mut self) {
        self.fullscreen_count += 1;
        if self.fullscreen_count == 1 && !self.paused_for_fs {
            self.paused_for_fs = true;
        }
    }

    fn on_fullscreen_leave(&mut self) {
        if self.fullscreen_count > 0 {
            self.fullscreen_count -= 1;
        }
        if self.fullscreen_count == 0 && self.paused_for_fs {
            self.paused_for_fs = false;
        }
    }

    fn on_window_active_enter(&mut self) {
        self.active_or_maximized_count += 1;
        if self.active_or_maximized_count == 1 && !self.muted_for_windows {
            self.muted_for_windows = true;
        }
    }

    fn on_window_active_leave(&mut self) {
        if self.active_or_maximized_count > 0 {
            self.active_or_maximized_count -= 1;
        }
        if self.active_or_maximized_count == 0 && self.muted_for_windows {
            self.muted_for_windows = false;
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // GlobalListContents is already maintained by the wayland-client
        // infrastructure before this callback fires.  Nothing to do.
    }
}

// ─── Dispatch: trivial globals (no meaningful events) ────────────────────────

impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _mgr: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { .. } => {}
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {}
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        0 => (ZwlrForeignToplevelHandleV1, ())
    ]);
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);

                let w = if width == 0 { 1920 } else { width as i32 };
                let h = if height == 0 { 1080 } else { height as i32 };

                if w != state.buf_w || h != state.buf_h {
                    state.buf_w = w;
                    state.buf_h = h;
                }

                if let Some(surf) = &state.surface {
                    surf.commit();
                }

                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                eprintln!("q6w: layer surface closed by compositor");
                state.running = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_client::Proxy;
        let id = handle.id();

        match event {
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw } => {
                // States are packed as little-endian u32 values in a byte array
                let is_fullscreen = raw.chunks_exact(4).any(|c| {
                    u32::from_ne_bytes(c.try_into().unwrap())
                        == zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32
                });
                let is_activated = raw.chunks_exact(4).any(|c| {
                    u32::from_ne_bytes(c.try_into().unwrap())
                        == zwlr_foreign_toplevel_handle_v1::State::Activated as u32
                });
                let is_maximized = raw.chunks_exact(4).any(|c| {
                    u32::from_ne_bytes(c.try_into().unwrap())
                        == zwlr_foreign_toplevel_handle_v1::State::Maximized as u32
                });
                let is_fs_active = is_fullscreen && is_activated;
                let is_active_or_max = is_activated || is_maximized;

                let (was_fs_active, was_active_or_max) = state
                    .toplevel_states
                    .insert(id, (is_fs_active, is_active_or_max))
                    .unwrap_or((false, false));

                if is_fs_active && !was_fs_active {
                    state.on_fullscreen_enter();
                }
                if !is_fs_active && was_fs_active {
                    state.on_fullscreen_leave();
                }

                if is_active_or_max && !was_active_or_max {
                    state.on_window_active_enter();
                }
                if !is_active_or_max && was_active_or_max {
                    state.on_window_active_leave();
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                if let Some((was_fs, was_active)) = state.toplevel_states.remove(&id) {
                    if was_fs {
                        state.on_fullscreen_leave();
                    }
                    if was_active {
                        state.on_window_active_leave();
                    }
                }
            }
            _ => {}
        }
    }
}
