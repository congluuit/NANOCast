//! Small settings file, read at start and written whenever something changes:
//! `%APPDATA%\NANOCast\config.json` on Windows (`~/Library/Application Support/NANOCast/`
//! on macOS). Unknown or missing fields fall back to defaults, so old files keep working.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use nanocast_core::protocol::VideoCodec;
use serde::{Deserialize, Serialize};

use crate::i18n::Lang;

/// The TV of the last successful cast.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LastTv {
    pub name: String,
    pub addr: Ipv4Addr,
    pub port: u16,
    pub ctrl_port: u16,
    /// Typed in by hand (re-added to the list at start, mDNS may never see it).
    #[serde(default)]
    pub manual: bool,
    /// The 4-digit code the TV accepted last time.
    #[serde(default)]
    pub pin: Option<String>,
}

/// Where the Windows sender turns BGRA into NV12 (`nanocast_win::session::ConvertPath`).
/// Stored on every OS so one file works everywhere; only Windows uses it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConvertPref {
    Gpu,
    Cpu,
    /// GPU when the PC allows it, else CPU (also for unknown values).
    #[default]
    #[serde(other)]
    Auto,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub last_tv: Option<LastTv>,
    pub bitrate_mbps: u32,
    pub fps: u32,
    /// `None`: first working hardware encoder in merit order.
    pub encoder: Option<String>,
    pub codec: VideoCodec,
    pub adaptive: bool,
    pub audio: bool,
    /// Cast the screen to `last_tv` as soon as it shows up after the app opens.
    pub auto_recast: bool,
    /// `None`: follow the system language.
    pub language: Option<Lang>,
    /// Windows only: BGRA -> NV12 on the GPU or the CPU.
    pub convert: ConvertPref,
    /// Windows only: do not encode frames while the screen does not change.
    pub skip_static: bool,
    /// The first-run guide was shown (or skipped).
    pub onboarded: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            last_tv: None,
            bitrate_mbps: 20,
            fps: 60,
            encoder: None,
            codec: VideoCodec::H264,
            adaptive: true,
            audio: false,
            auto_recast: false,
            language: None,
            convert: ConvertPref::Auto,
            skip_static: true,
            onboarded: false,
        }
    }
}

/// "Picture quality" in Settings: plain-word presets for bitrate, frame rate and adapting
/// to the network. Other combinations come from Settings › Advanced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Auto,
    /// Higher bitrate at 30 fps: small text stays crisp.
    Sharp,
    /// Less data at 60 fps: for weak Wi-Fi.
    Smooth,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Auto, Quality::Sharp, Quality::Smooth];

    /// (bitrate Mbps, fps, adaptive)
    pub fn values(self) -> (u32, u32, bool) {
        match self {
            Quality::Auto => (20, 60, true),
            Quality::Sharp => (30, 30, true),
            Quality::Smooth => (10, 60, true),
        }
    }
}

impl Config {
    /// The preset matching the current values, `None` for custom ones.
    pub fn quality(&self) -> Option<Quality> {
        Quality::ALL.into_iter().find(|q| q.values() == (self.bitrate_mbps, self.fps, self.adaptive))
    }

    pub fn set_quality(&mut self, q: Quality) {
        (self.bitrate_mbps, self.fps, self.adaptive) = q.values();
    }

    pub fn path() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|d| d.config_dir().join("NANOCast").join("config.json"))
    }

    /// Defaults when there is no file or it cannot be read.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_json(&text).unwrap_or_else(|e| {
                log::warn!("{}: {e}; using defaults", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn from_json(text: &str) -> serde_json::Result<Self> {
        let mut c: Self = serde_json::from_str(text)?;
        c.sanitize();
        Ok(c)
    }

    fn sanitize(&mut self) {
        self.bitrate_mbps = self.bitrate_mbps.clamp(2, 80);
        if !matches!(self.fps, 30 | 60) {
            self.fps = 60;
        }
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
            // Write then rename, so a crash never leaves half a file.
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            log::warn!("saving {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let c = Config {
            last_tv: Some(LastTv {
                name: "Phòng 12".into(),
                addr: Ipv4Addr::new(192, 168, 1, 20),
                port: 48322,
                ctrl_port: 48323,
                manual: false,
                pin: Some("0427".into()),
            }),
            codec: VideoCodec::Hevc,
            auto_recast: true,
            language: Some(Lang::En),
            convert: ConvertPref::Cpu,
            skip_static: false,
            ..Default::default()
        };
        let text = serde_json::to_string(&c).unwrap();
        assert!(text.contains("\"codec\":\"hevc\"") && text.contains("\"language\":\"en\""), "{text}");
        assert!(text.contains("\"convert\":\"cpu\"") && text.contains("\"skip_static\":false"), "{text}");
        assert_eq!(Config::from_json(&text).unwrap(), c);
        // Old / partial / odd files.
        assert_eq!(Config::from_json("{}").unwrap(), Config::default());
        let d = Config::default();
        assert_eq!((d.convert, d.skip_static), (ConvertPref::Auto, true));
        assert_eq!(Config::from_json("{\"convert\": \"gpu\"}").unwrap().convert, ConvertPref::Gpu);
        assert_eq!(Config::from_json("{\"convert\": \"npu\", \"fps\": 30}").unwrap().fps, 30);
        let c = Config::from_json("{\"fps\": 45, \"bitrate_mbps\": 500, \"future\": 1}").unwrap();
        assert_eq!((c.fps, c.bitrate_mbps), (60, 80));
        assert!(Config::from_json("not json").is_err());
    }

    #[test]
    fn quality_presets() {
        let mut c = Config::default();
        assert_eq!(c.quality(), Some(Quality::Auto));
        c.set_quality(Quality::Sharp);
        assert_eq!((c.bitrate_mbps, c.fps, c.quality()), (30, 30, Some(Quality::Sharp)));
        c.adaptive = false;
        assert_eq!(c.quality(), None);
        assert!(!Config::default().onboarded);
    }
}
