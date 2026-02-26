# q6w

A scrappy little hobby project that does one thing: play a video as your Wayland desktop wallpaper.

I built this for myself because I wanted animated wallpapers on Hyprland and nothing
out there did it quite the way I wanted. It works on any compositor that supports
`zwlr_layer_shell_v1` (Sway, Hyprland, river, labwc, …).

> **Fair warning**: this is a personal project, not a polished product.
> It does what I need it to do, but it's far from feature-complete. See the
> [limitations](#limitations) section below for what's missing.

## How it works

| Layer      | What happens                                                                                                                                                |
| ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Decode** | GStreamer decodes video in background threads. VAAPI hardware decoding is used automatically when available; software fallback otherwise.                   |
| **Upload** | Decoded frames are mapped directly from the GstBuffer and written to a GPU texture via wgpu — no `Vec` allocation, no CPU-side copy.                        |
| **Render** | A full-screen quad is drawn onto a `wlr-layer-shell` surface using wgpu (Vulkan or OpenGL backend). A WGSL fragment shader handles the BGRA → RGBA swizzle. |

## Dependencies

- **Rust** (edition 2024)
- **GStreamer 1.x** + base/good/bad plugins
- **Wayland** compositor with `zwlr_layer_shell_v1`
- Vulkan or OpenGL-capable GPU (wgpu backend)

### Arch Linux

```sh
sudo pacman -S gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad
```

For VAAPI hardware decoding, also install:

```sh
# Arch
sudo pacman -S gst-plugin-va

```

## Building

```sh
cargo build --release
```

The binary is at `target/release/q6w`.

## Usage

```
q6w --file <VIDEO>
```

### Options

| Flag                  | Description                                  |
| --------------------- | -------------------------------------------- |
| `-f, --file <FILE>`   | Path to the video file                       |
| `-a, --audio`         | Enable audio playback (off by default)       |
| `--volume <VOLUME>`   | Audio volume, `0.0` – `1.0` (default: `1.0`) |
| `--fps <FPS>`         | Framerate limit (e.g. `30`)                  |
| `--no-fallback-guard` | Allow software decoding above 1080p          |
| `--license`           | Print license info and source code links     |
| `-V, --version`       | Print version                                |
| `-h, --help`          | Print help                                   |

### Examples

```sh
# basic — play a video as wallpaper
q6w --file ~/Videos/wallpaper.mp4

# with audio at half volume
q6w --file ~/Videos/wallpaper.mp4 --audio --volume 0.5

# cap at 30 fps to save power
q6w --file ~/Videos/wallpaper.mp4 --fps 30
```

## Project structure

```
src/
  main.rs          CLI entry point, Wayland connection setup
  app.rs           Wayland state & protocol Dispatch implementations
  gst_pipeline.rs  GStreamer decode pipeline (VAAPI → software fallback)
  gpu_renderer.rs  wgpu full-screen quad renderer (zero-copy upload)
```

## Limitations

This is a hobby project — it scratches my itch, but it doesn't try to be
everything. Here's what it _doesn't_ do (yet, or maybe ever):

- **No multi-monitor support**: it renders on whatever output the compositor gives it;
  you can't pick a specific screen or set different videos per monitor.
- **No playlist / shuffle**: one video, looped. That's it.
- **No runtime control**: no IPC, no socket, no D-Bus. To change the video, kill it
  and start a new one.
- **No image wallpapers**: video only. Use `swaybg` or similar for static images.
- **No X11**: Wayland only, and specifically compositors with `zwlr_layer_shell_v1`.
- **No graceful handling of GPU loss** — if your GPU resets, q6w will crash.
- **Software decoding above 1080p is blocked by default**: CPU and memory usage can
  get extreme. You can override this with `--no-fallback-guard`, but don't say I
  didn't warn you.

Pull requests are welcome if any of this bothers you enough to fix it.

## Performance

Tested on my single-monitor(1920x1200@60hz) Arch Linux setup with VAAPI enabled.

Sample file:

- 3840x2160 (4K UHD)
- 60 FPS
- H.264 (Main), ~140 Mbps
- ~700 MB, 43 seconds looped

Memory usage (RSS):

- 60 FPS: ~70–75 MB (stable after startup)
- 30 FPS cap: ~55–65 MB (stable)

Observed over ~1 hour runtime with no upward trend.

## License

This project is licensed under the [GNU Affero General Public License v3.0](LICENSE) (AGPL-3.0-only).

Source code:

- **GitHub**: <https://github.com/Sreehari425/q6w>
- **Codeberg** (mirror): <https://codeberg.org/sreehari425/q6w>

---
