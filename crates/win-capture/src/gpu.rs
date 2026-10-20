//! Zero-copy GPU path: the captured BGRA texture never leaves VRAM.
//!
//! Everything runs on windows-capture's own D3D11 device (made multithread protected), so
//! there is no cross-device sharing or keyed-mutex wait (that variant stalled 20-30 ms per
//! frame once the process was short of CPU, see docs/benchmark.md):
//! - capture thread: `TileHasher` hashes the frame in 64x64 tiles with a compute shader
//!   (unchanged screen -> nothing is sent); `Nv12Converter` converts the WGC texture straight
//!   to NV12 BT.709 limited range, scaled / letterboxed to the encoder canvas, with
//!   `ID3D11VideoProcessor`, into a small pool of NV12 textures;
//! - cast thread: the encoder MFT gets the same device through an `IMFDXGIDeviceManager`
//!   (`device_manager`) and each NV12 texture as a DXGI surface sample.
//!
//! A pool texture stays reserved while its `GpuFrame` lives (queued, being encoded, or
//! kept to be repeated); dropping the `GpuFrame` frees it. GPU work on one immediate context
//! runs in call order, so a texture freed after the encoder's output came out can be written
//! again right away.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use log::{debug, info};
use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};

use crate::convert::fit_rect;

/// NV12 textures: one being written, up to two queued / in the encoder, one kept for repeats.
const NV12_POOL: usize = 5;
/// Tile edge of `TileHasher` in pixels (8x8 threads, 8x8 pixels each).
const TILE: u32 = 64;

/// An NV12 picture (canvas size) in a pool texture; the texture is reserved until this drops.
pub struct GpuFrame {
    pub tex: ID3D11Texture2D,
    pub slot: usize,
    busy: Arc<[AtomicBool]>,
}

impl Drop for GpuFrame {
    fn drop(&mut self) {
        self.busy[self.slot].store(false, Ordering::Release);
    }
}

/// Adapter name of a device (for logs).
pub fn adapter_name(device: &ID3D11Device) -> String {
    unsafe {
        device
            .cast::<IDXGIDevice>()
            .and_then(|d| d.GetAdapter())
            .and_then(|a| a.GetDesc())
            .map(|d| String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string())
            .unwrap_or_default()
    }
}

/// Makes the device safe to use from several threads (capture, cast, the MFT's own).
pub fn protect(ctx: &ID3D11DeviceContext) -> Result<ID3D11Multithread> {
    let mt: ID3D11Multithread = ctx.cast().context("ID3D11Multithread")?;
    unsafe {
        let _ = mt.SetMultithreadProtected(true);
    }
    Ok(mt)
}

/// DXGI device manager around `device`, for `MFT_MESSAGE_SET_D3D_MANAGER`.
pub fn device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    unsafe {
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager).context("MFCreateDXGIDeviceManager")?;
        let manager = manager.ok_or_else(|| anyhow!("no DXGI device manager"))?;
        manager.ResetDevice(device, token).context("IMFDXGIDeviceManager::ResetDevice")?;
        Ok(manager)
    }
}

fn tex2d(device: &ID3D11Device, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D> {
    let mut tex = None;
    unsafe { device.CreateTexture2D(desc, None, Some(&mut tex)) }?;
    tex.ok_or_else(|| anyhow!("CreateTexture2D returned nothing"))
}

fn bgra_desc(w: u32, h: u32, bind: D3D11_BIND_FLAG) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

// ---------------------------------------------------------------------------------------
// Change detection
// ---------------------------------------------------------------------------------------

const HASH_CS: &str = r#"
Texture2D<float4> src : register(t0);
RWStructuredBuffer<uint> tiles : register(u0);
groupshared uint part[64];

[numthreads(8, 8, 1)]
void main(uint3 gid : SV_GroupID, uint3 tid : SV_GroupThreadID, uint gi : SV_GroupIndex)
{
    uint w, h;
    src.GetDimensions(w, h);
    uint2 base = gid.xy * 64 + tid.xy * 8;
    uint acc = 2166136261;
    [unroll] for (uint y = 0; y < 8; y++) {
        [unroll] for (uint x = 0; x < 8; x++) {
            // Out of range Load returns 0: the edge tiles hash consistently.
            uint3 c = (uint3)(src.Load(int3(base + uint2(x, y), 0)).rgb * 255.0 + 0.5);
            acc = (acc ^ (c.r | (c.g << 8) | (c.b << 16))) * 16777619;
        }
    }
    acc ^= gi * 0x9E3779B9u;
    acc ^= acc >> 16; acc *= 0x85EBCA6Bu; acc ^= acc >> 13; acc *= 0xC2B2AE35u; acc ^= acc >> 16;
    part[gi] = acc;
    GroupMemoryBarrierWithGroupSync();
    if (gi == 0) {
        uint t = 0;
        for (uint i = 0; i < 64; i++) t ^= part[i];
        tiles[gid.y * ((w + 63) / 64) + gid.x] = t;
    }
}
"#;

/// Change detection on the GPU: a 32-bit hash per 64x64 tile (an FNV chain per 8x8 block,
/// mixed per block), read back (~2 KB at 1080p) and compared with the previous frame.
pub struct TileHasher {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    mt: Option<ID3D11Multithread>,
    cs: ID3D11ComputeShader,
    size: (u32, u32),
    buf: Option<(ID3D11Buffer, ID3D11UnorderedAccessView, ID3D11Buffer)>,
    /// Private copy for frame textures that cannot be bound as a shader resource.
    scratch: Option<ID3D11Texture2D>,
    prev: Vec<u32>,
    cur: Vec<u32>,
}

impl TileHasher {
    pub fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext) -> Result<Self> {
        let mut blob = None;
        let mut errors = None;
        let r = unsafe {
            D3DCompile(
                HASH_CS.as_ptr() as _,
                HASH_CS.len(),
                PCSTR(c"tilehash".as_ptr() as _),
                None,
                None,
                PCSTR(c"main".as_ptr() as _),
                PCSTR(c"cs_5_0".as_ptr() as _),
                D3DCOMPILE_OPTIMIZATION_LEVEL3,
                0,
                &mut blob,
                Some(&mut errors),
            )
        };
        if let Err(e) = r {
            let msg = errors
                .map(|b| unsafe {
                    String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()))
                        .into_owned()
                })
                .unwrap_or_default();
            bail!("compiling the tile hash shader: {e} {msg}");
        }
        let blob = blob.ok_or_else(|| anyhow!("D3DCompile returned nothing"))?;
        let code = unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) };
        let mut cs = None;
        unsafe { device.CreateComputeShader(code, None, Some(&mut cs)) }.context("CreateComputeShader")?;
        Ok(Self {
            device: device.clone(),
            ctx: ctx.clone(),
            mt: ctx.cast().ok(),
            cs: cs.ok_or_else(|| anyhow!("CreateComputeShader returned nothing"))?,
            size: (0, 0),
            buf: None,
            scratch: None,
            prev: Vec::new(),
            cur: Vec::new(),
        })
    }

    fn tiles(w: u32, h: u32) -> (u32, u32) {
        (w.div_ceil(TILE), h.div_ceil(TILE))
    }

    fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        let (tx, ty) = Self::tiles(w, h);
        let mut desc = D3D11_BUFFER_DESC {
            ByteWidth: tx * ty * 4,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
            StructureByteStride: 4,
        };
        let (mut buf, mut uav, mut staging) = (None, None, None);
        unsafe {
            self.device.CreateBuffer(&desc, None, Some(&mut buf))?;
            let buf_ref = buf.as_ref().ok_or_else(|| anyhow!("CreateBuffer returned nothing"))?;
            self.device.CreateUnorderedAccessView(buf_ref, None, Some(&mut uav))?;
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            self.device.CreateBuffer(&desc, None, Some(&mut staging))?;
        }
        self.buf = Some((buf.unwrap(), uav.ok_or_else(|| anyhow!("no UAV"))?, staging.ok_or_else(|| anyhow!("no staging"))?));
        self.scratch = None;
        self.size = (w, h);
        self.prev.clear();
        Ok(())
    }

    /// Hashes `tex` (`w`x`h` BGRA); `true` when any tile differs from the previous call (or
    /// the size changed). Waits for the GPU (well under 1 ms of GPU work at 1080p).
    pub fn changed(&mut self, tex: &ID3D11Texture2D, w: u32, h: u32) -> Result<bool> {
        if self.size != (w, h) || self.buf.is_none() {
            self.resize(w, h)?;
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let src = if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 != 0 {
            tex.clone()
        } else {
            if self.scratch.is_none() {
                self.scratch = Some(tex2d(&self.device, &bgra_desc(w, h, D3D11_BIND_SHADER_RESOURCE))?);
            }
            let s = self.scratch.clone().unwrap();
            unsafe { self.ctx.CopyResource(&s, tex) };
            s
        };
        let (buf, uav, staging) = self.buf.as_ref().unwrap();
        let (tx, ty) = Self::tiles(w, h);
        unsafe {
            let mut srv = None;
            self.device.CreateShaderResourceView(&src, None, Some(&mut srv)).context("tile hash SRV")?;
            // The pipeline state below spans several calls: keep the encoder's threads out.
            if let Some(mt) = &self.mt {
                mt.Enter();
            }
            self.ctx.CSSetShader(&self.cs, None);
            self.ctx.CSSetShaderResources(0, Some(&[srv]));
            self.ctx.CSSetUnorderedAccessViews(0, 1, Some(&Some(uav.clone())), None);
            self.ctx.Dispatch(tx, ty, 1);
            self.ctx.CSSetShaderResources(0, Some(&[None]));
            self.ctx.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
            self.ctx.CSSetShader(None, None);
            self.ctx.CopyResource(staging, buf);
            if let Some(mt) = &self.mt {
                mt.Leave();
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).context("map tile hashes")?;
            let n = (tx * ty) as usize;
            self.cur.clear();
            self.cur.extend_from_slice(std::slice::from_raw_parts(mapped.pData as *const u32, n));
            self.ctx.Unmap(staging, 0);
        }
        let changed = self.cur != self.prev;
        std::mem::swap(&mut self.cur, &mut self.prev);
        Ok(changed)
    }
}

// ---------------------------------------------------------------------------------------
// BGRA -> NV12
// ---------------------------------------------------------------------------------------

struct Vp {
    src: (u32, u32),
    en: ID3D11VideoProcessorEnumerator,
    proc: ID3D11VideoProcessor,
    outs: Vec<ID3D11VideoProcessorOutputView>,
}

/// BGRA (any size) -> NV12 canvas with `ID3D11VideoProcessor`, on the device of the frames.
pub struct Nv12Converter {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_ctx: ID3D11VideoContext,
    canvas: (u32, u32),
    pool: Vec<ID3D11Texture2D>,
    busy: Arc<[AtomicBool]>,
    vp: Option<Vp>,
    /// Input view of the last source texture (WGC reuses its texture frame after frame).
    input: Option<(usize, ID3D11VideoProcessorInputView)>,
    /// Copy target for sources the video processor cannot read directly.
    scratch: Option<ID3D11Texture2D>,
    pub adapter: String,
}

impl Nv12Converter {
    /// Fails (-> CPU path) when the device has no video processor that turns BGRA into NV12.
    pub fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext, canvas: (u32, u32)) -> Result<Self> {
        let video_device: ID3D11VideoDevice = device.cast().context("ID3D11VideoDevice")?;
        let video_ctx: ID3D11VideoContext = ctx.cast().context("ID3D11VideoContext")?;
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: canvas.0,
            Height: canvas.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_VIDEO_ENCODER).0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut pool = Vec::new();
        for _ in 0..NV12_POOL {
            let tex = match tex2d(device, &desc) {
                Ok(t) => t,
                Err(_) if desc.BindFlags != D3D11_BIND_RENDER_TARGET.0 as u32 => {
                    desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
                    tex2d(device, &desc).context("NV12 texture")?
                }
                Err(e) => return Err(e).context("NV12 texture"),
            };
            pool.push(tex);
        }
        let conv = Self {
            device: device.clone(),
            ctx: ctx.clone(),
            video_device,
            video_ctx,
            canvas,
            pool,
            busy: (0..NV12_POOL).map(|_| AtomicBool::new(false)).collect(),
            vp: None,
            input: None,
            scratch: None,
            adapter: adapter_name(device),
        };
        unsafe {
            let en = conv.video_device.CreateVideoProcessorEnumerator(&conv.content_desc(canvas)).context("CreateVideoProcessorEnumerator")?;
            let input = en.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)?;
            let output = en.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)?;
            if input & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
                || output & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
            {
                bail!("video processor cannot convert BGRA to NV12");
            }
        }
        info!("[gpu] BGRA->NV12 on the GPU ({}), canvas {}x{}", conv.adapter, canvas.0, canvas.1);
        Ok(conv)
    }

    fn content_desc(&self, src: (u32, u32)) -> D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: src.0,
            InputHeight: src.1,
            OutputFrameRate: rate,
            OutputWidth: self.canvas.0,
            OutputHeight: self.canvas.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        }
    }

    fn ensure_vp(&mut self, src: (u32, u32)) -> Result<()> {
        if self.vp.as_ref().is_some_and(|v| v.src == src) {
            return Ok(());
        }
        self.vp = None;
        self.input = None;
        self.scratch = None;
        unsafe {
            let en = self.video_device.CreateVideoProcessorEnumerator(&self.content_desc(src))?;
            let proc = self.video_device.CreateVideoProcessor(&en, 0)?;
            let mut outs = Vec::new();
            for tex in &self.pool {
                let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
                };
                let mut view = None;
                self.video_device.CreateVideoProcessorOutputView(tex, &en, &desc, Some(&mut view))?;
                outs.push(view.ok_or_else(|| anyhow!("no output view"))?);
            }
            let vc = &self.video_ctx;
            vc.VideoProcessorSetStreamFrameFormat(&proc, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vc.VideoProcessorSetStreamAutoProcessingMode(&proc, 0, false);
            let (x, y, w, h) = fit_rect(src.0, src.1, self.canvas.0, self.canvas.1);
            let src_rect = RECT { left: 0, top: 0, right: src.0 as i32, bottom: src.1 as i32 };
            let dst_rect = RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 };
            let target = RECT { left: 0, top: 0, right: self.canvas.0 as i32, bottom: self.canvas.1 as i32 };
            vc.VideoProcessorSetStreamSourceRect(&proc, 0, true, Some(&src_rect));
            vc.VideoProcessorSetStreamDestRect(&proc, 0, true, Some(&dst_rect));
            vc.VideoProcessorSetOutputTargetRect(&proc, true, Some(&target));
            // Bars: black in RGB terms (the processor turns it into Y=16, UV=128).
            let black =
                D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
            vc.VideoProcessorSetOutputBackgroundColor(&proc, false, &black);
            // Full-range sRGB in, BT.709 limited range out (same as the CPU converter).
            if let Ok(vc1) = vc.cast::<ID3D11VideoContext1>() {
                vc1.VideoProcessorSetStreamColorSpace1(&proc, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                vc1.VideoProcessorSetOutputColorSpace1(&proc, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            } else {
                // Bitfield: Usage(bit 0), RGB_Range(1: 0 = full), YCbCr_Matrix(2: 1 = BT.709),
                // xvYCC(3), Nominal_Range(4-5: 1 = 16-235).
                let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
                vc.VideoProcessorSetStreamColorSpace(&proc, 0, &input);
                vc.VideoProcessorSetOutputColorSpace(&proc, &output);
            }
            debug!("[gpu] video processor {}x{} -> {}x{} at ({x},{y}) {w}x{h}", src.0, src.1, self.canvas.0, self.canvas.1);
            self.vp = Some(Vp { src, en, proc, outs });
        }
        Ok(())
    }

    fn input_view(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let key = src.as_raw() as usize;
        if let Some((k, view)) = &self.input {
            if *k == key {
                return Ok(view.clone());
            }
        }
        let en = &self.vp.as_ref().unwrap().en;
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut view = None;
        unsafe { self.video_device.CreateVideoProcessorInputView(src, en, &desc, Some(&mut view)) }
            .context("CreateVideoProcessorInputView")?;
        let view = view.ok_or_else(|| anyhow!("no input view"))?;
        self.input = Some((key, view.clone()));
        Ok(view)
    }

    /// Converts `src` (`w`x`h` BGRA) into a free pool texture. `None` when all of them are
    /// still reserved (the frame is dropped).
    pub fn convert(&mut self, src: &ID3D11Texture2D, w: u32, h: u32) -> Result<Option<GpuFrame>> {
        let Some(slot) = self.busy.iter().position(|b| !b.load(Ordering::Acquire)) else { return Ok(None) };
        self.ensure_vp((w, h))?;
        let view = match self.scratch.clone() {
            Some(s) => {
                unsafe { self.ctx.CopyResource(&s, src) };
                self.input_view(&s)?
            }
            None => match self.input_view(src) {
                Ok(v) => v,
                Err(e) => {
                    // Not readable by the video processor as is: go through a copy from now on.
                    debug!("[gpu] direct input view refused ({e:#}); copying each frame");
                    let s = tex2d(&self.device, &bgra_desc(w, h, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE))?;
                    unsafe { self.ctx.CopyResource(&s, src) };
                    self.scratch = Some(s.clone());
                    self.input_view(&s)?
                }
            },
        };
        let vp = self.vp.as_ref().unwrap();
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(view)),
            ..Default::default()
        }];
        let r = unsafe { self.video_ctx.VideoProcessorBlt(&vp.proc, &vp.outs[slot], 0, &streams) };
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        r.context("VideoProcessorBlt")?;
        self.busy[slot].store(true, Ordering::Release);
        Ok(Some(GpuFrame { tex: self.pool[slot].clone(), slot, busy: self.busy.clone() }))
    }
}

// ---------------------------------------------------------------------------------------
// Standalone device (probes, tests)
// ---------------------------------------------------------------------------------------

/// A D3D11 device of our own with video support, multithread protected, plus its manager.
pub struct GpuDevice {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    pub manager: IMFDXGIDeviceManager,
    pub adapter: String,
}

impl GpuDevice {
    /// Device on `adapter` (`None`: the default adapter).
    pub fn new(adapter: Option<&IDXGIAdapter>) -> Result<Self> {
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let (mut dev, mut ctx) = (None, None);
        unsafe {
            D3D11CreateDevice(
                adapter,
                driver,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
        }
        .context("D3D11CreateDevice (video support)")?;
        let (device, ctx): (ID3D11Device, ID3D11DeviceContext) = (dev.unwrap(), ctx.unwrap());
        protect(&ctx)?;
        let manager = device_manager(&device)?;
        let adapter = adapter_name(&device);
        Ok(Self { device, ctx, manager, adapter })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads an NV12 texture back: (Y plane, UV plane), tightly packed.
    fn read_nv12(dev: &GpuDevice, tex: &ID3D11Texture2D, w: u32, h: u32) -> (Vec<u8>, Vec<u8>) {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let staging = tex2d(&dev.device, &desc).unwrap();
        let (mut y, mut uv) = (Vec::new(), Vec::new());
        unsafe {
            dev.ctx.CopyResource(&staging, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            dev.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
            let base = m.pData as *const u8;
            let pitch = m.RowPitch as usize;
            for row in 0..h as usize {
                y.extend_from_slice(std::slice::from_raw_parts(base.add(row * pitch), w as usize));
            }
            for row in 0..h as usize / 2 {
                uv.extend_from_slice(std::slice::from_raw_parts(base.add((h as usize + row) * pitch), w as usize));
            }
            dev.ctx.Unmap(&staging, 0);
        }
        (y, uv)
    }

    #[test]
    fn video_processor_matches_cpu_levels_and_letterbox() {
        let Ok(dev) = GpuDevice::new(None) else {
            eprintln!("no D3D11 video device; skipped");
            return;
        };
        let Ok(mut conv) = Nv12Converter::new(&dev.device, &dev.ctx, (64, 36)) else {
            eprintln!("no BGRA->NV12 video processor; skipped");
            return;
        };
        // White 4:3 source into a 16:9 canvas: bars left and right.
        let (sw, sh) = (40u32, 30u32);
        let white = vec![255u8; (sw * sh * 4) as usize];
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: white.as_ptr() as _, SysMemPitch: sw * 4, SysMemSlicePitch: 0 };
        let mut src = None;
        unsafe { dev.device.CreateTexture2D(&bgra_desc(sw, sh, D3D11_BIND_SHADER_RESOURCE), Some(&init), Some(&mut src)) }.unwrap();
        let f = conv.convert(&src.unwrap(), sw, sh).unwrap().expect("a free texture");
        let (y, uv) = read_nv12(&dev, &f.tex, 64, 36);
        let (x, _, w, _) = fit_rect(sw, sh, 64, 36);
        let row = &y[64 * 18..64 * 19];
        assert!(row[0].abs_diff(16) <= 2, "bar Y {}", row[0]);
        assert!(row[(x + w / 2) as usize].abs_diff(235) <= 2, "white Y {}", row[(x + w / 2) as usize]);
        assert!(uv[0].abs_diff(128) <= 2 && uv[1].abs_diff(128) <= 2, "bar UV {} {}", uv[0], uv[1]);
        // The texture stays reserved while the frame lives.
        let slot = f.slot;
        assert!(conv.busy[slot].load(Ordering::Acquire));
        drop(f);
        assert!(!conv.busy[slot].load(Ordering::Acquire));
    }
}
