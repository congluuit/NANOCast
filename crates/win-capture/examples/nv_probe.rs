//! Which hardware encoder MFT opens, on which GPU, with which input (PLAN.md Phase 1: the
//! NVIDIA MFT failed with E_UNEXPECTED on an Optimus laptop).
//!
//! For every DXGI adapter: the encoders `MFTEnum2` lists for that adapter
//! (`MFT_ENUM_ADAPTER_LUID`), each opened (a) with a D3D11 device manager on that adapter
//! (`MFT_MESSAGE_SET_D3D_MANAGER` before the media types) and fed NV12 textures, and
//! (b) without one, fed system memory. Plus the plain `MFTEnumEx` list the app uses.
//!
//!   cargo run --release -p nanocast-win --example nv_probe

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    probe::main()
}

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
mod probe {
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, Result};
    use nanocast_core::protocol::VideoCodec;
    use nanocast_win::gpu::GpuDevice;
    use nanocast_win::mf_encoder::{list_hw_encoders, list_hw_encoders_on, EncoderConfig, MfEncoder, MfRuntime};
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

    const W: u32 = 1920;
    const H: u32 = 1080;
    const FRAMES: usize = 30;

    fn cfg(d3d: Option<&GpuDevice>) -> EncoderConfig {
        EncoderConfig {
            codec: VideoCodec::H264,
            width: W,
            height: H,
            fps: 60,
            bitrate_bps: 20_000_000,
            gop_frames: 600,
            encoder_index: 0,
            d3d: d3d.map(|g| g.manager.clone()),
        }
    }

    /// Grey NV12 picture with a moving bright square.
    fn nv12(i: usize) -> Vec<u8> {
        let mut v = vec![128u8; (W * H * 3 / 2) as usize];
        let x0 = (i * 40) % (W as usize - 200);
        for y in 400..600 {
            v[y * W as usize + x0..y * W as usize + x0 + 200].fill(230);
        }
        v
    }

    fn texture(dev: &GpuDevice, data: &[u8]) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as _, SysMemPitch: W, SysMemSlicePitch: 0 };
        let mut tex = None;
        unsafe { dev.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }?;
        tex.ok_or_else(|| anyhow!("no texture"))
    }

    /// Feeds FRAMES frames; returns (frames out, keyframes, bytes, average encode ms).
    fn run(enc: &mut MfEncoder, dev: Option<&GpuDevice>) -> Result<(usize, usize, usize, f64)> {
        let pictures: Vec<Vec<u8>> = (0..4).map(nv12).collect();
        let textures = match dev {
            Some(d) if enc.gpu_input => pictures.iter().map(|p| texture(d, p)).collect::<Result<Vec<_>>>()?,
            _ => Vec::new(),
        };
        let (mut out, mut key, mut bytes, mut lat) = (0usize, 0usize, 0usize, 0u64);
        let t0 = Instant::now();
        for i in 0..FRAMES {
            enc.wait_need_input(|f| {
                out += 1;
                key += f.keyframe as usize;
                bytes += f.data.len();
                lat += f.encode_latency_us;
            })?;
            let pts = t0.elapsed().as_nanos() as i64 / 100;
            if textures.is_empty() {
                enc.submit(&pictures[i % 4], pts, Instant::now())?;
            } else {
                enc.submit_texture(&textures[i % 4], i % 4, pts, Instant::now())?;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        let until = Instant::now() + Duration::from_millis(500);
        while Instant::now() < until {
            enc.poll_output(|f| {
                out += 1;
                key += f.keyframe as usize;
                bytes += f.data.len();
                lat += f.encode_latency_us;
            })?;
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok((out, key, bytes, if out == 0 { 0.0 } else { lat as f64 / out as f64 / 1000.0 }))
    }

    fn report(label: &str, r: Result<(MfEncoder, (usize, usize, usize, f64))>) {
        match r {
            Ok((enc, (out, key, bytes, ms))) => println!(
                "    {label}: OK, {} input, {out}/{FRAMES} frames out ({key} key), {} KB, encode {ms:.1} ms",
                if enc.gpu_input { "texture" } else { "memory" },
                bytes / 1024
            ),
            Err(e) => println!("    {label}: FAILED {e:#}"),
        }
    }

    pub fn main() -> Result<()> {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
        // NANOCAST_AFFINITY=0xC: run on those logical CPUs only (a rough weak PC).
        if let Ok(mask) = std::env::var("NANOCAST_AFFINITY") {
            let mask = usize::from_str_radix(mask.trim_start_matches("0x"), 16)?;
            unsafe {
                windows::Win32::System::Threading::SetProcessAffinityMask(windows::Win32::System::Threading::GetCurrentProcess(), mask)?
            };
        }
        let _rt = MfRuntime::init()?;

        println!("MFTEnumEx (what the app lists):");
        for (name, _) in list_hw_encoders(VideoCodec::H264)? {
            println!("  {name}");
        }
        println!("  -> each opened with a device manager on the default adapter:");
        let default_dev = GpuDevice::new(None)?;
        for (name, act) in list_hw_encoders(VideoCodec::H264)? {
            let r = MfEncoder::from_activate(name.clone(), act, &cfg(Some(&default_dev)))
                .and_then(|mut e| run(&mut e, Some(&default_dev)).map(|x| (e, x)));
            report(&name, r);
        }

        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut i = 0;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
            i += 1;
            let desc = unsafe { adapter.GetDesc1()? };
            let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_string();
            let luid = ((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64;
            println!("\nAdapter {name} (LUID {luid:#x}, vendor {:#06x})", desc.VendorId);
            let encoders = match list_hw_encoders_on(VideoCodec::H264, luid) {
                Ok(e) => e,
                Err(e) => {
                    println!("  MFTEnum2: {e:#}");
                    continue;
                }
            };
            if encoders.is_empty() {
                println!("  no hardware H.264 encoder");
                continue;
            }
            let dev = match GpuDevice::new(Some(&adapter.cast::<IDXGIAdapter>()?)) {
                Ok(d) => Some(d),
                Err(e) => {
                    println!("  D3D11 video device: {e:#}");
                    None
                }
            };
            for (enc_name, _) in &encoders {
                println!("  {enc_name}");
                // A fresh activate per attempt: a shut down activate is not reused.
                let fresh = || -> Result<_> {
                    list_hw_encoders_on(VideoCodec::H264, luid)?
                        .into_iter()
                        .find(|(n, _)| n == enc_name)
                        .map(|(_, a)| a)
                        .ok_or_else(|| anyhow!("gone"))
                };
                if let Some(dev) = &dev {
                    let r = fresh().and_then(|a| MfEncoder::from_activate(enc_name.clone(), a, &cfg(Some(dev))))
                        .and_then(|mut e| run(&mut e, Some(dev)).map(|x| (e, x)));
                    report("D3D manager + textures", r);
                }
                let r = fresh().and_then(|a| MfEncoder::from_activate(enc_name.clone(), a, &cfg(None)))
                    .and_then(|mut e| run(&mut e, None).map(|x| (e, x)));
                report("system memory", r);
            }
        }
        Ok(())
    }
}
