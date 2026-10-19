//! Full-screen presentation with wgpu: the decoder's I420 planes are uploaded as three R8
//! textures and converted to RGB (BT.709 limited range) in the fragment shader, letterboxed
//! to the window. Each picture is presented as soon as it arrives (PresentMode::AutoNoVsync,
//! frame latency 1), so no vsync queue builds up. A small text overlay (stats or the idle
//! screen) is drawn on top from an RGBA texture rendered by `font`.

use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use winit::window::Window;

use crate::decoder::Picture;
use crate::font::TextImage;

const SHADER: &str = r#"
struct Rect { r: vec4<f32> };  // x0, y0, x1, y1 in NDC

@group(0) @binding(0) var tex_a: texture_2d<f32>;
@group(0) @binding(1) var tex_b: texture_2d<f32>;
@group(0) @binding(2) var tex_c: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> rect: Rect;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    // Triangle strip: (0,0) (1,0) (0,1) (1,1) in uv space, v pointing down.
    let uv = vec2<f32>(f32(i & 1u), f32(i >> 1u));
    let x = mix(rect.r.x, rect.r.z, uv.x);
    let y = mix(rect.r.y, rect.r.w, uv.y);
    var o: VsOut;
    o.pos = vec4<f32>(x, y, 0.0, 1.0);
    o.uv = uv;
    return o;
}

@fragment
fn fs_yuv(in: VsOut) -> @location(0) vec4<f32> {
    let y = (textureSample(tex_a, samp, in.uv).r - 0.0627451) * 1.1643836;
    let u = textureSample(tex_b, samp, in.uv).r - 0.5;
    let v = textureSample(tex_c, samp, in.uv).r - 0.5;
    let rgb = vec3<f32>(y + 1.7927411 * v, y - 0.2132486 * u - 0.5329093 * v, y + 2.1124018 * u);
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

@fragment
fn fs_rgba(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex_a, samp, in.uv);
}
"#;

struct Planes {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind: wgpu::BindGroup,
}

struct Overlay {
    width: u32,
    height: u32,
    tex: wgpu::Texture,
    bind: wgpu::BindGroup,
}

/// Where the text box goes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    TopLeft,
    /// Small badge in the top-right corner (paused sign).
    TopRight,
    Center,
}

pub struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    layout: wgpu::BindGroupLayout,
    yuv_pipeline: wgpu::RenderPipeline,
    rgba_pipeline: wgpu::RenderPipeline,
    linear: wgpu::Sampler,
    nearest: wgpu::Sampler,
    dummy: wgpu::TextureView,
    video_rect: wgpu::Buffer,
    overlay_rect: wgpu::Buffer,
    planes: Option<Planes>,
    overlay: Option<Overlay>,
    pub adapter_info: String,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let (surface, adapter) = pick_adapter(&window)?;
        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?})", info.name, info.backend);
        log::info!("GPU: {adapter_info}");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("nanocast"),
            // Old iGPUs / Raspberry Pi GLES: ask only for the downlevel minimum.
            required_limits: wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            // Small allocator blocks: we only hold a few textures (DX12 default reserves ~256 MB).
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..Default::default()
        }))
        .context("creating GPU device")?;

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        // The shader outputs gamma-encoded RGB already: an sRGB surface would encode it twice.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| anyhow!("surface has no formats"))?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoNoVsync,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        log::info!("surface {format:?}, present modes {:?}", caps.present_modes);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nanocast"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = |fs: &str, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fs),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
                }),
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
                depth_stencil: None,
                multisample: Default::default(),
                multiview: None,
                cache: None,
            })
        };
        let yuv_pipeline = pipeline("fs_yuv", None);
        let rgba_pipeline = pipeline("fs_rgba", Some(wgpu::BlendState::ALPHA_BLENDING));
        let sampler = |filter| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: filter,
                min_filter: filter,
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                ..Default::default()
            })
        };
        let linear = sampler(wgpu::FilterMode::Linear);
        let nearest = sampler(wgpu::FilterMode::Nearest);
        let dummy = r8_texture(&device, 1, 1).create_view(&Default::default());
        let rect_buf = || {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let (video_rect, overlay_rect) = (rect_buf(), rect_buf());
        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            layout,
            yuv_pipeline,
            rgba_pipeline,
            linear,
            nearest,
            dummy,
            video_rect,
            overlay_rect,
            planes: None,
            overlay: None,
            adapter_info,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Uploads a decoded picture (recreating the textures when the size changes).
    pub fn upload(&mut self, pic: &Picture) {
        if self.planes.as_ref().is_none_or(|p| (p.width, p.height) != (pic.width, pic.height)) {
            let (cw, chh) = (pic.width.div_ceil(2), pic.height.div_ceil(2));
            let y = r8_texture(&self.device, pic.width, pic.height);
            let u = r8_texture(&self.device, cw, chh);
            let v = r8_texture(&self.device, cw, chh);
            let views = [&y, &u, &v].map(|t| t.create_view(&Default::default()));
            let bind = self.bind_group(&views[0], &views[1], &views[2], &self.linear, &self.video_rect);
            self.planes = Some(Planes { width: pic.width, height: pic.height, y, u, v, bind });
            log::info!("video {}x{}", pic.width, pic.height);
        }
        let p = self.planes.as_ref().unwrap();
        let (cw, chh) = (pic.width.div_ceil(2), pic.height.div_ceil(2));
        write_plane(&self.queue, &p.y, &pic.y, pic.stride_y, pic.width, pic.height);
        write_plane(&self.queue, &p.u, &pic.u, pic.stride_uv, cw, chh);
        write_plane(&self.queue, &p.v, &pic.v, pic.stride_uv, cw, chh);
    }

    /// Forgets the last picture (stream stopped): only the overlay is drawn until the next one.
    pub fn clear_video(&mut self) {
        self.planes = None;
    }

    pub fn set_overlay(&mut self, img: Option<&TextImage>) {
        let Some(img) = img else {
            self.overlay = None;
            return;
        };
        if self.overlay.as_ref().is_none_or(|o| (o.width, o.height) != (img.width, img.height)) {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("overlay"),
                size: wgpu::Extent3d { width: img.width, height: img.height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = tex.create_view(&Default::default());
            let bind = self.bind_group(&view, &self.dummy, &self.dummy, &self.nearest, &self.overlay_rect);
            self.overlay = Some(Overlay { width: img.width, height: img.height, tex, bind });
        }
        let o = self.overlay.as_ref().unwrap();
        self.queue.write_texture(
            o.tex.as_image_copy(),
            &img.rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(img.width * 4), rows_per_image: None },
            wgpu::Extent3d { width: img.width, height: img.height, depth_or_array_layers: 1 },
        );
    }

    /// Draws the current picture (letterboxed) and overlay, and presents immediately.
    pub fn render(&mut self, placement: Placement) {
        // Going full screen can change the size without a Resized event reaching us first.
        let s = self.window.inner_size();
        if (s.width, s.height) != (self.config.width, self.config.height) {
            self.resize(s.width, s.height);
        }
        let Some(frame) = self.acquire() else { return };
        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        if let Some(p) = &self.planes {
            let (sx, sy) = fit(p.width as f32 / p.height as f32, sw / sh);
            self.queue.write_buffer(&self.video_rect, 0, bytemuck::cast_slice(&[-sx, sy, sx, -sy]));
        }
        if let Some(o) = &self.overlay {
            let (w, h) = (o.width as f32 * 2.0 / sw, o.height as f32 * 2.0 / sh);
            let (x0, y0) = match placement {
                Placement::TopLeft => (-1.0 + 16.0 / sw, 1.0 - 16.0 / sh),
                Placement::TopRight => (1.0 - w - 16.0 / sw, 1.0 - 16.0 / sh),
                Placement::Center => (-w / 2.0, h / 2.0),
            };
            // Snap to whole pixels so the bitmap font stays crisp.
            let snap = |v: f32, n: f32| ((v + 1.0) * n / 2.0).round() * 2.0 / n - 1.0;
            let (x0, y0) = (snap(x0, sw), snap(y0, sh));
            self.queue.write_buffer(&self.overlay_rect, 0, bytemuck::cast_slice(&[x0, y0, x0 + w, y0 - h]));
        }
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if let Some(p) = &self.planes {
                pass.set_pipeline(&self.yuv_pipeline);
                pass.set_bind_group(0, &p.bind, &[]);
                pass.draw(0..4, 0..1);
            }
            if let Some(o) = &self.overlay {
                pass.set_pipeline(&self.rgba_pipeline);
                pass.set_bind_group(0, &o.bind, &[]);
                pass.draw(0..4, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
        self.window.pre_present_notify();
        frame.present();
    }

    /// Next swapchain image; reconfigures once when the swapchain is lost, outdated or
    /// suboptimal (Vulkan reports that after a full-screen switch).
    fn acquire(&mut self) -> Option<wgpu::SurfaceTexture> {
        for attempt in 0..2 {
            match self.surface.get_current_texture() {
                Ok(f) if f.suboptimal && attempt == 0 => drop(f),
                Ok(f) => return Some(f),
                Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) if attempt == 0 => {}
                Err(e) => {
                    log::warn!("surface: {e}");
                    return None;
                }
            }
            let s = self.window.inner_size();
            self.config.width = s.width.max(1);
            self.config.height = s.height.max(1);
            self.surface.configure(&self.device, &self.config);
        }
        None
    }

    fn bind_group(
        &self,
        a: &wgpu::TextureView,
        b: &wgpu::TextureView,
        c: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
        rect: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(a) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(b) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(c) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(sampler) },
                wgpu::BindGroupEntry { binding: 4, resource: rect.as_entire_binding() },
            ],
        })
    }
}

/// On Windows DX12 first (flip-model swapchain that composes well with DWM full screen;
/// Vulkan full screen could not even be screen-captured on the dev machine), then any
/// backend. `WGPU_BACKEND=vulkan|gl|dx12` overrides. Integrated GPU preferred: it is enough
/// for a textured quad and keeps a laptop's discrete GPU asleep.
fn pick_adapter(window: &Arc<Window>) -> Result<(wgpu::Surface<'static>, wgpu::Adapter)> {
    let env = wgpu::Backends::from_env();
    let mut tries = Vec::new();
    match env {
        Some(b) => tries.push(b),
        None => {
            if cfg!(windows) {
                tries.push(wgpu::Backends::DX12);
            }
            tries.push(wgpu::Backends::all());
        }
    }
    let mut last_err = anyhow!("no GPU backend");
    for backends in tries {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends, ..wgpu::InstanceDescriptor::from_env_or_default() });
        let surface = instance.create_surface(window.clone()).context("creating surface")?;
        match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        })) {
            Ok(a) => return Ok((surface, a)),
            Err(e) => {
                log::info!("no adapter for {backends:?}: {e}");
                last_err = anyhow!("no GPU adapter (wgpu needs DX12, Vulkan or OpenGL ES 3): {e}");
            }
        }
    }
    Err(last_err)
}

/// Half-extents in NDC of a picture with aspect `pic` letterboxed into a surface with aspect `surf`.
pub fn fit(pic: f32, surf: f32) -> (f32, f32) {
    if pic > surf {
        (1.0, surf / pic)
    } else {
        (pic / surf, 1.0)
    }
}

fn r8_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_plane(queue: &wgpu::Queue, tex: &wgpu::Texture, data: &[u8], stride: u32, width: u32, height: u32) {
    queue.write_texture(
        tex.as_image_copy(),
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: None },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn letterbox() {
        // 16:9 on 16:9 fills, on 4:3 bars top and bottom, 4:3 on 16:9 bars left and right.
        assert_eq!(fit(16.0 / 9.0, 16.0 / 9.0), (1.0, 1.0));
        let (x, y) = fit(16.0 / 9.0, 4.0 / 3.0);
        assert!(x == 1.0 && (y - 0.75).abs() < 1e-6);
        let (x, y) = fit(4.0 / 3.0, 16.0 / 9.0);
        assert!((x - 0.75).abs() < 1e-6 && y == 1.0);
    }
}
