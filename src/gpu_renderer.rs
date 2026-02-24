//! wgpu-based full-screen video renderer.
//!
//! Accepts raw BGRA pixels from the GStreamer appsink, uploads them directly
//! via `Queue::write_texture` (a single DMA-style write from the mapped
//! GstBuffer into a GPU staging buffer — no intermediate `Vec` allocation),
//! then renders them as a full-screen quad onto the Wayland swapchain surface.
//!
//! # Why no `to_vec()`?
//! `gst_pipeline::Pipeline::with_frame` gives us a `&[u8]` backed by a
//! read-only GstBuffer memory map.  `wgpu::Queue::write_texture` accepts any
//! `&[u8]`, so we pass the mapped slice straight through.  A `Vec::to_vec()`
//! copy is never made.
//!
//! # BGRA → display swizzle
//! GStreamer emits `video/x-raw,format=BGRA`.
//! We store the bytes in a `TextureFormat::Rgba8Unorm` texture, which means
//! the GPU sees `.r = B, .g = G, .b = R, .a = A` in memory order.
//! The fragment shader corrects this with a single `vec4(c.b, c.g, c.r, c.a)`
//! swizzle — no extra copy.

use std::ffi::c_void;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};

// ─── WGSL shader ─────────────────────────────────────────────────────────────

const SHADER_SRC: &str = r#"
// Six vertices for two triangles covering NDC space.
var<private> VERTS: array<vec2<f32>, 6> = array<vec2<f32>, 6>(
    vec2(-1.0, -1.0), vec2( 1.0, -1.0), vec2(-1.0,  1.0),
    vec2(-1.0,  1.0), vec2( 1.0, -1.0), vec2( 1.0,  1.0),
);

struct VO { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VO {
    let p = VERTS[vi];
    // Map [-1,1] NDC to [0,1] UV, flip Y so (0,0) is top-left
    return VO(vec4(p, 0.0, 1.0), vec2((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5));
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;

@fragment
fn fs(v: VO) -> @location(0) vec4<f32> {
    // Texture is Rgba8Unorm but stores BGRA bytes → swap B↔R
    let c = textureSample(tex, smp, v.uv);
    return vec4(c.b, c.g, c.r, c.a);
}
"#;

// ─── GpuRenderer ─────────────────────────────────────────────────────────────

pub struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    pipeline: wgpu::RenderPipeline,
    texture: wgpu::Texture,
    bind_grp: wgpu::BindGroup,
    width: u32,
    height: u32,
}

impl GpuRenderer {
    /// Create a wgpu renderer that presents onto the given Wayland surface.
    ///
    /// # Safety
    /// Both `display` and `surface` must remain valid for the entire lifetime
    /// of the returned `GpuRenderer`.  They are the raw C pointers to
    /// `wl_display` and `wl_surface` respectively.
    pub unsafe fn new(
        display: *mut c_void,
        surface: *mut c_void,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..Default::default()
        });

        // Create wgpu surface from raw Wayland handles
        let wgpu_surface = unsafe {
            let rdh = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                std::ptr::NonNull::new(display).expect("null wl_display"),
            ));
            let rwh = RawWindowHandle::Wayland(WaylandWindowHandle::new(
                std::ptr::NonNull::new(surface).expect("null wl_surface"),
            ));
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: rdh,
                raw_window_handle: rwh,
            })?
        };

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&wgpu_surface),
            force_fallback_adapter: false,
        }))
        .ok_or_else(|| anyhow::anyhow!("no wgpu adapter found"))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("q6w"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None, // no API call tracing
        ))?;

        // Pick surface format — MUST be non-sRGB so video bytes pass through
        // without double gamma correction.  Video content is already sRGB-encoded;
        // the Wayland compositor handles final display gamma.
        let caps = wgpu_surface.get_capabilities(&adapter);
        let fmt = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);

        wgpu_surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: fmt,
                width,
                height,
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                view_formats: vec![],
                desired_maximum_frame_latency: 1,
            },
        );

        // Frame texture: Rgba8Unorm — we upload BGRA bytes, shader swizzles
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let tex_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let tex_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let bind_grp = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&tex_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&tex_sampler),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let pl_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pl_layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit_pipeline"),
            layout: Some(&pl_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: fmt,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(GpuRenderer {
            device,
            queue,
            surface: wgpu_surface,
            pipeline,
            texture,
            bind_grp,
            width,
            height,
        })
    }

    /// Upload `bgra` pixels directly (zero-copy from the GstBuffer map) and
    /// render them onto the swapchain surface.
    ///
    /// `bgra` must be exactly `width * height * 4` bytes.
    pub fn upload_and_render(&self, bgra: &[u8]) {
        // Write pixels straight from the mapped GstBuffer into the GPU texture.
        // wgpu does a single staging-buffer write — no Vec allocation here.
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bgra,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("q6w: wgpu surface error: {e}");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame_enc"),
            });
        {
            let mut rpass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rpass.set_pipeline(&self.pipeline);
            rpass.set_bind_group(0, &self.bind_grp, &[]);
            rpass.draw(0..6, 0..1);
        }
        self.queue.submit([enc.finish()]);
        frame.present();
    }
}
