//! "Sound only on the TV": mutes the PC's default output device while a cast sends sound.
//!
//! WASAPI loopback (`audio.rs`, and `nanocast_cast::sound` for Google Cast) taps the mix
//! before the endpoint mute is applied, so the TV keeps getting everything that plays while
//! the PC's own speakers stay quiet. The volume level is left alone: on some drivers it
//! scales the loopback too, the mute does not.
//!
//! Each call runs on a short-lived thread of its own, so the COM apartment of the caller
//! (the GUI thread is STA) never matters.

use anyhow::{anyhow, Context as _, Result};
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};

/// Mutes the default output device. Returns its endpoint id when this call muted it (give it
/// to [`unmute`] later), `None` when it was muted already (then there is nothing to undo).
pub fn mute_default() -> Result<Option<String>> {
    on_com_thread(|| unsafe {
        let device = enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole).context("default output device")?;
        let volume = endpoint_volume(&device)?;
        if volume.GetMute()?.as_bool() {
            return Ok(None);
        }
        let raw = device.GetId()?;
        let id = raw.to_string();
        CoTaskMemFree(Some(raw.0 as *const _));
        let id = id.context("endpoint id")?;
        volume.SetMute(true, std::ptr::null()).context("mute speakers")?;
        log::info!("PC speakers muted ({id})");
        Ok(Some(id))
    })
}

/// Unmutes the device [`mute_default`] muted (also after the default device changed). A
/// device that is gone (unplugged headset) is not an error.
pub fn unmute(id: &str) -> Result<()> {
    let id = id.to_owned();
    on_com_thread(move || unsafe {
        let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
        let device = match enumerator()?.GetDevice(PCWSTR(wide.as_ptr())) {
            Ok(d) => d,
            Err(e) => {
                log::info!("PC speakers {id} gone, nothing to unmute: {e}");
                return Ok(());
            }
        };
        endpoint_volume(&device)?.SetMute(false, std::ptr::null()).context("unmute speakers")?;
        log::info!("PC speakers unmuted ({id})");
        Ok(())
    })
}

unsafe fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("audio device enumerator") }
}

unsafe fn endpoint_volume(device: &IMMDevice) -> Result<IAudioEndpointVolume> {
    unsafe { device.Activate(CLSCTX_ALL, None).context("endpoint volume") }
}

fn on_com_thread<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    std::thread::Builder::new()
        .name("speaker".into())
        .spawn(move || {
            let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            let result = f();
            if init.is_ok() {
                unsafe { CoUninitialize() };
            }
            result
        })
        .context("spawn speaker thread")?
        .join()
        .map_err(|_| anyhow!("speaker thread panicked"))?
}
