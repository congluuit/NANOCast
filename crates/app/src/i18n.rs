//! GUI strings in Vietnamese and English. One `Tr` table per language; `{}` in a string is
//! filled in order by `fill`. The default language follows the system locale (Vietnamese
//! for `vi*`, English otherwise) and can be changed in Settings (saved in the config).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Vi,
    En,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::Vi, Lang::En];

    /// From a BCP 47 tag such as "vi-VN" or "en-US".
    pub fn from_locale(tag: &str) -> Self {
        if tag.trim().to_ascii_lowercase().starts_with("vi") {
            Lang::Vi
        } else {
            Lang::En
        }
    }

    pub fn system() -> Self {
        sys_locale::get_locale().map(|l| Self::from_locale(&l)).unwrap_or(Lang::En)
    }

    /// Name of the language in itself (for the Settings list).
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::Vi => "Tiếng Việt",
            Lang::En => "English",
        }
    }

    pub fn tr(self) -> &'static Tr {
        match self {
            Lang::Vi => &VI,
            Lang::En => &EN,
        }
    }
}

/// Replaces each `{}` in `template` with the next argument.
pub fn fill(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut args = args.iter();
    let mut rest = template;
    while let Some(i) = rest.find("{}") {
        out.push_str(&rest[..i]);
        if let Some(a) = args.next() {
            out.push_str(&a.to_string());
        }
        rest = &rest[i + 2..];
    }
    out.push_str(rest);
    out
}

pub struct Tr {
    // TV list
    pub tv_heading: &'static str,
    pub rescan: &'static str,
    pub rescan_hint: &'static str,
    pub searching: &'static str,
    pub mdns_error: &'static str,
    pub typed_in: &'static str,
    pub pin_badge: &'static str,
    pub pin_badge_hint: &'static str,
    pub tv_ip: &'static str,
    pub add: &'static str,
    pub bad_ip: &'static str,
    // Actions
    pub cast_screen: &'static str,
    pub cast_app: &'static str,
    pub stop: &'static str,
    pub pick_tv_first: &'static str,
    pub portal_hint: &'static str,
    pub pause: &'static str,
    pub resume: &'static str,
    pub pause_hint: &'static str,
    pub blank: &'static str,
    pub blank_hint: &'static str,
    pub no_ctrl_hint: &'static str,
    pub audio: &'static str,
    pub audio_hint: &'static str,
    pub audio_mac13: &'static str,
    pub audio_on_at_start: &'static str,
    pub audio_off_paren: &'static str,
    pub screen_n: &'static str,
    pub screen: &'static str,
    pub display_n: &'static str,
    // Status
    pub idle_hint: &'static str,
    pub connecting: &'static str,
    pub casting: &'static str,
    pub paused_status: &'static str,
    pub blank_status: &'static str,
    pub stopped: &'static str,
    pub source_closed: &'static str,
    pub error: &'static str,
    pub pin_refused: &'static str,
    pub waiting_recast: &'static str,
    // Stats
    pub tv_lost: &'static str,
    pub tv_no_numbers: &'static str,
    pub tv_ctrl_lost: &'static str,
    pub tv_udp_only: &'static str,
    pub sound: &'static str,
    pub sound_error: &'static str,
    pub silent: &'static str,
    pub off: &'static str,
    pub rate_line: &'static str,
    pub reason_settings: &'static str,
    pub reason_fixed: &'static str,
    pub reason_loss: &'static str,
    pub reason_keyframes: &'static str,
    pub reason_decoder: &'static str,
    pub reason_recovered: &'static str,
    pub fallback_receiver: &'static str,
    pub fallback_encoder: &'static str,
    // Settings
    pub settings: &'static str,
    pub encoder: &'static str,
    pub automatic: &'static str,
    pub automatic_long: &'static str,
    pub no_hw_encoder: &'static str,
    pub mf_error: &'static str,
    pub codec: &'static str,
    pub codec_h264: &'static str,
    pub codec_hevc: &'static str,
    pub codec_hevc_hint: &'static str,
    pub bitrate: &'static str,
    pub adaptive: &'static str,
    pub adaptive_hint: &'static str,
    pub frame_rate: &'static str,
    pub auto_recast: &'static str,
    pub auto_recast_hint: &'static str,
    pub language: &'static str,
    pub lang_system: &'static str,
    pub applies_next: &'static str,
    // Window picker
    pub pick_app: &'static str,
    pub filter: &'static str,
    pub refresh: &'static str,
    pub minimized: &'static str,
    pub no_windows: &'static str,
    pub menu_note: &'static str,
    pub close: &'static str,
    // PIN dialog
    pub pin_title: &'static str,
    pub pin_body: &'static str,
    pub pin_wrong: &'static str,
    pub pin_cast: &'static str,
    pub cancel: &'static str,
    // macOS permission
    pub perm_missing: &'static str,
    pub perm_button: &'static str,
}

pub static VI: Tr = Tr {
    tv_heading: "TV trong mạng",
    rescan: "↻ Quét lại",
    rescan_hint: "Tìm lại TV trong mạng (mDNS)",
    searching: "Đang tìm TV… Mở app NANOCast trên TV (cùng mạng Wi-Fi/LAN).",
    mdns_error: "mDNS lỗi: {}. Hãy nhập IP của TV.",
    typed_in: "nhập tay",
    pin_badge: "🔒 mã",
    pin_badge_hint: "TV này yêu cầu mã 4 số hiện trên màn hình TV",
    tv_ip: "IP của TV:",
    add: "Thêm",
    bad_ip: "IP không hợp lệ (vd. 192.168.1.20)",
    cast_screen: "🖥 Cast màn hình",
    cast_app: "🗔 Cast ứng dụng…",
    stop: "⏹ Dừng",
    pick_tv_first: "Chọn một TV trước",
    portal_hint: "Hệ thống sẽ hỏi bạn chọn màn hình hoặc cửa sổ.",
    pause: "⏸ Tạm dừng",
    resume: "▶ Tiếp tục",
    pause_hint: "TV giữ nguyên hình hiện tại; PC ngừng gửi hình và tiếng cho tới khi bấm Tiếp tục",
    blank: "⬛ Màn hình đen",
    blank_hint: "TV hiện màn hình đen; bỏ chọn để hiện lại hình",
    no_ctrl_hint: "TV này không có kênh điều khiển",
    audio: "🔊 Âm thanh",
    audio_hint: "Gửi âm thanh của PC (mọi ứng dụng) lên TV. Bật/tắt được trong lúc cast.",
    audio_mac13: "Âm thanh hệ thống cần macOS 13 trở lên",
    audio_on_at_start: "(bật khi bắt đầu cast)",
    audio_off_paren: "(tắt)",
    screen_n: "màn hình {}",
    screen: "màn hình",
    display_n: "Màn hình {}",
    idle_hint: "Chọn TV rồi bấm Cast. Bật Game Mode trên TV để giảm trễ.",
    connecting: "Đang kết nối tới {}…",
    casting: "● Đang cast {} → {}",
    paused_status: "⏸ Đã tạm dừng: TV giữ hình cuối",
    blank_status: "⬛ TV đang hiện màn hình đen",
    stopped: "Đã dừng cast.",
    source_closed: "Cửa sổ/màn hình đã đóng, đã dừng cast.",
    error: "Lỗi: {}",
    pin_refused: "TV từ chối mã. Nhập mã đang hiện trên TV.",
    waiting_recast: "Đang chờ {} để tự cast lại…",
    tv_lost: "mất {} gói (tổng {})",
    tv_no_numbers: "TV  chưa có số liệu",
    tv_ctrl_lost: "TV  mất kênh điều khiển (vẫn gửi hình)",
    tv_udp_only: "TV  không có kênh điều khiển (chỉ UDP)",
    sound: "Âm thanh",
    sound_error: "lỗi: {}",
    silent: "(im lặng)",
    off: "tắt",
    rate_line: "Bitrate {} Mbps · {} fps ({})",
    reason_settings: "theo cài đặt",
    reason_fixed: "cố định",
    reason_loss: "giảm vì mất {}% gói",
    reason_keyframes: "giảm vì TV xin keyframe liên tục ({} lần)",
    reason_decoder: "giảm vì TV giải mã chậm ({} fps, {} ms)",
    reason_recovered: "tăng lại, mạng đã ổn định",
    fallback_receiver: "HEVC → H.264: TV không hỗ trợ HEVC",
    fallback_encoder: "HEVC → H.264: PC không có encoder HEVC",
    settings: "Cài đặt",
    encoder: "Encoder",
    automatic: "Tự động",
    automatic_long: "Tự động (thử lần lượt)",
    no_hw_encoder: "Không có encoder phần cứng {}",
    mf_error: "Lỗi encoder: {}",
    codec: "Codec",
    codec_h264: "H.264 (mặc định)",
    codec_hevc: "HEVC",
    codec_hevc_hint: "Nhẹ mạng hơn ở cùng chất lượng. Chỉ dùng khi TV giải mã được HEVC; nếu không sẽ tự dùng H.264.",
    bitrate: "Bitrate",
    adaptive: "Tự chỉnh theo mạng",
    adaptive_hint: "Giảm bitrate (30→20→15→10 Mbps, dưới 15 Mbps thì 30 fps) khi TV báo mất gói, tăng lại sau 10 s ổn định",
    frame_rate: "Khung hình",
    auto_recast: "Tự cast lại khi mở app",
    auto_recast_hint: "Mở app là cast màn hình tới TV dùng lần trước ngay khi thấy TV đó",
    language: "Ngôn ngữ",
    lang_system: "Theo hệ thống",
    applies_next: "Áp dụng từ lần Cast tiếp theo.",
    pick_app: "Chọn ứng dụng để cast",
    filter: "Lọc theo tên…",
    refresh: "↻ Làm mới",
    minimized: "  (đang thu nhỏ)",
    no_windows: "Không có cửa sổ nào.",
    menu_note: "Menu/hộp thoại bật ra là cửa sổ riêng nên không hiện; cần thấy menu thì dùng Cast màn hình.",
    close: "Đóng",
    pin_title: "Nhập mã trên TV",
    pin_body: "{} yêu cầu mã 4 số đang hiện trên màn hình TV.",
    pin_wrong: "Mã chưa đúng, thử lại.",
    pin_cast: "Cast",
    cancel: "Huỷ",
    perm_missing: "NANOCast chưa có quyền Ghi màn hình (Screen Recording). Cấp quyền rồi mở lại app.",
    perm_button: "Cấp quyền",
};

pub static EN: Tr = Tr {
    tv_heading: "TVs on the network",
    rescan: "↻ Rescan",
    rescan_hint: "Search the network for TVs again (mDNS)",
    searching: "Looking for TVs… Open the NANOCast app on the TV (same Wi-Fi/LAN).",
    mdns_error: "mDNS error: {}. Type the TV's IP instead.",
    typed_in: "typed in",
    pin_badge: "🔒 code",
    pin_badge_hint: "This TV asks for the 4-digit code shown on its screen",
    tv_ip: "TV IP:",
    add: "Add",
    bad_ip: "Invalid IP (e.g. 192.168.1.20)",
    cast_screen: "🖥 Cast screen",
    cast_app: "🗔 Cast app…",
    stop: "⏹ Stop",
    pick_tv_first: "Pick a TV first",
    portal_hint: "The system will ask which screen or window to share.",
    pause: "⏸ Pause",
    resume: "▶ Resume",
    pause_hint: "The TV keeps the current picture; the PC sends no picture or sound until Resume",
    blank: "⬛ Black screen",
    blank_hint: "The TV shows black; untick to show the picture again",
    no_ctrl_hint: "This TV has no control channel",
    audio: "🔊 Sound",
    audio_hint: "Send the PC's sound (all apps) to the TV. Can be switched while casting.",
    audio_mac13: "System sound needs macOS 13 or later",
    audio_on_at_start: "(on when casting starts)",
    audio_off_paren: "(off)",
    screen_n: "screen {}",
    screen: "screen",
    display_n: "Display {}",
    idle_hint: "Pick a TV and press Cast. Turn on Game Mode on the TV for less delay.",
    connecting: "Connecting to {}…",
    casting: "● Casting {} → {}",
    paused_status: "⏸ Paused: the TV keeps the last picture",
    blank_status: "⬛ The TV shows a black screen",
    stopped: "Casting stopped.",
    source_closed: "The window/screen closed, casting stopped.",
    error: "Error: {}",
    pin_refused: "The TV refused the code. Enter the code shown on the TV.",
    waiting_recast: "Waiting for {} to cast again…",
    tv_lost: "lost {} pkts (total {})",
    tv_no_numbers: "TV  no numbers yet",
    tv_ctrl_lost: "TV  control channel lost (still sending)",
    tv_udp_only: "TV  no control channel (UDP only)",
    sound: "Sound",
    sound_error: "error: {}",
    silent: "(silent)",
    off: "off",
    rate_line: "Bitrate {} Mbps · {} fps ({})",
    reason_settings: "as set",
    reason_fixed: "fixed",
    reason_loss: "lowered: {}% packets lost",
    reason_keyframes: "lowered: TV keeps asking for keyframes ({}×)",
    reason_decoder: "lowered: TV decodes slowly ({} fps, {} ms)",
    reason_recovered: "raised again, network is stable",
    fallback_receiver: "HEVC → H.264: the TV cannot decode HEVC",
    fallback_encoder: "HEVC → H.264: no HEVC encoder on this PC",
    settings: "Settings",
    encoder: "Encoder",
    automatic: "Automatic",
    automatic_long: "Automatic (try each)",
    no_hw_encoder: "No hardware {} encoder",
    mf_error: "Encoder error: {}",
    codec: "Codec",
    codec_h264: "H.264 (default)",
    codec_hevc: "HEVC",
    codec_hevc_hint: "Less bandwidth at the same quality. Only used when the TV decodes HEVC; H.264 otherwise.",
    bitrate: "Bitrate",
    adaptive: "Adapt to the network",
    adaptive_hint: "Lower the bitrate (30→20→15→10 Mbps, 30 fps below 15 Mbps) when the TV reports loss, raise it after 10 s without trouble",
    frame_rate: "Frame rate",
    auto_recast: "Cast again when the app opens",
    auto_recast_hint: "On start, cast the screen to the last TV as soon as it shows up",
    language: "Language",
    lang_system: "System",
    applies_next: "Applies from the next cast.",
    pick_app: "Choose an app to cast",
    filter: "Filter by name…",
    refresh: "↻ Refresh",
    minimized: "  (minimized)",
    no_windows: "No windows.",
    menu_note: "Pop-up menus and dialogs are separate windows and do not show; use Cast screen to see them.",
    close: "Close",
    pin_title: "Enter the code on the TV",
    pin_body: "{} asks for the 4-digit code shown on its screen.",
    pin_wrong: "Wrong code, try again.",
    pin_cast: "Cast",
    cancel: "Cancel",
    perm_missing: "NANOCast has no Screen Recording permission yet. Grant it, then reopen the app.",
    perm_button: "Grant access",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_and_fill() {
        assert_eq!(Lang::from_locale("vi-VN"), Lang::Vi);
        assert_eq!(Lang::from_locale("VI"), Lang::Vi);
        assert_eq!(Lang::from_locale("en-US"), Lang::En);
        assert_eq!(Lang::from_locale("fr-FR"), Lang::En);
        assert_eq!(fill(VI.casting, &[&"màn hình 1", &"Phòng 12"]), "● Đang cast màn hình 1 → Phòng 12");
        assert_eq!(fill(EN.tv_lost, &[&3, &10]), "lost 3 pkts (total 10)");
        assert_eq!(fill("a {} b {}", &[&1]), "a 1 b ");
    }

    #[test]
    fn placeholders_match_between_languages() {
        // Every string with placeholders must have the same number of `{}` in both languages.
        let count = |s: &str| s.matches("{}").count();
        let pairs = [
            (VI.mdns_error, EN.mdns_error),
            (VI.screen_n, EN.screen_n),
            (VI.display_n, EN.display_n),
            (VI.connecting, EN.connecting),
            (VI.casting, EN.casting),
            (VI.error, EN.error),
            (VI.waiting_recast, EN.waiting_recast),
            (VI.tv_lost, EN.tv_lost),
            (VI.sound_error, EN.sound_error),
            (VI.rate_line, EN.rate_line),
            (VI.reason_loss, EN.reason_loss),
            (VI.reason_keyframes, EN.reason_keyframes),
            (VI.reason_decoder, EN.reason_decoder),
            (VI.no_hw_encoder, EN.no_hw_encoder),
            (VI.mf_error, EN.mf_error),
            (VI.pin_body, EN.pin_body),
        ];
        for (vi, en) in pairs {
            assert_eq!(count(vi), count(en), "{vi} / {en}");
        }
    }
}
