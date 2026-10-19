# NANOCast trên Linux (Phase 3)

Crate `crates/linux-capture` (`nanocast-linux`) là bản Linux của `nanocast-win` / `nanocast-mac`: cùng giao thức (PLAN.md mục 4), cùng API (`capture::Source`, `sources::{list_monitors, list_windows, window_alive}`, `session::{CastConfig, CastSession, CastStats, Phase, EndReason, CodecFallback}`, `mf_encoder::hw_encoder_names`, `control_link`, `audio`) để GUI `crates/app` chỉ cần chọn crate bằng `cfg(target_os)`.

> **Trạng thái 2026-10-19: viết xong, CHƯA KIỂM CHỨNG trên máy Linux thật** (chưa chạy với màn hình, GPU, TV thật). Code được viết trên Windows; mức kiểm tra đã làm được ghi ở mục 2.

## 1. Pipeline

```
Nguồn (capture::backend()):
  Portal  : xdg-desktop-portal ScreenCast (ashpd, D-Bus) -> hộp thoại hệ thống chọn màn hình / cửa sổ
            -> PipeWire node (pw_stream, BGRx/BGRA/RGBx/RGBA/NV12, SHM/memfd; DMA-BUF thì mmap)
  X11     : MIT-SHM GetImage của root (màn hình RandR) hoặc pixmap Composite của 1 cửa sổ,
            con trỏ vẽ thêm bằng XFixes
  -> nhịp theo fps (Pacer, giống WGC; fps đổi được khi đang cast)
  -> RGB32/NV12 -> NV12 BT.709 limited, co giãn vào khung cố định + viền đen (CPU, rayon)
  -> encoder (thử lần lượt, cái đầu tiên mở được quyết định khung hình):
       h264_vaapi (Intel/AMD) > h264_nvenc (NVIDIA) > h264_v4l2m2m (Pi 4) > libx264 > OpenH264
     không B-frame, CBR, GOP dài, ép IDR khi TV gửi keyframe_request,
     Annex B, SPS/PPS trước mọi IDR (annexb::ParamSets bù cho v4l2m2m)
  -> nanocast_core::protocol::PacedSender (UDP 48322) + ControlLink (TCP 48323)
Âm thanh: PipeWire stream.capture.sink (monitor của output mặc định), F32LE 48 kHz stereo
  (fallback PulseAudio @DEFAULT_MONITOR@) -> Opus 20 ms (nanocast_core::audio) -> UDP 48324,
  gói im lặng 4 kbps khi không có tiếng, giống hệt Windows.
```

| Windows (`nanocast-win`) | Linux (`nanocast-linux`) |
|---|---|
| `capture.rs` WGC | `capture.rs` chọn back end; `portal.rs` + `pw_video.rs` (PipeWire), `x11.rs` (X11) |
| `convert.rs` BGRA -> NV12 | `convert.rs` BGRx/RGBx (stride bất kỳ) và NV12 -> NV12, vẽ con trỏ; có test |
| `mf_encoder.rs` Media Foundation | `encoder.rs` + `ff_encoder.rs` (libavcodec) + `oh_encoder.rs` (OpenH264); `mf_encoder::hw_encoder_names` là bí danh |
| `audio.rs` WASAPI loopback | `audio.rs` PipeWire / PulseAudio monitor |
| `sources.rs` HWND / HMONITOR | `sources.rs` X11 window id (trường `hwnd`) / chỉ số RandR 1..; trên Wayland: mục giả "chọn trong hộp thoại" |
| — | `annexb.rs` giữ SPS/PPS trước IDR; có test |

Cargo features (mặc định bật hết): `pipewire` (portal + PipeWire hình và tiếng), `ffmpeg` (libavcodec), `openh264` (encoder phần mềm build từ source), `pulse` (PulseAudio). Tắt bớt khi máy thiếu thư viện, ví dụ chỉ X11 + OpenH264: `--no-default-features --features openh264`.

### Vì sao FFmpeg (libavcodec) mà không phải `libva` / `cros-codecs`

- VA-API trực tiếp (`libva` crate) nghĩa là tự dựng tham số SPS/PPS/slice và quản lý surface tham chiếu cho từng driver: nhiều code, khó đúng, không kiểm thử được từ Windows.
- `cros-codecs` có encoder VA-API nhưng nhắm ChromeOS, API còn đổi liên tục, cần libva mới, và **không có đường V4L2 M2M stateful** cho Raspberry Pi 4.
- libavcodec có sẵn trên mọi distro, một API cho cả `h264_vaapi`, `h264_nvenc`, `h264_v4l2m2m`, `libx264`, đã có chế độ low-latency (`async_depth=1`, `zerolatency`, `forced-idr`). Gọi thẳng C API qua `ffmpeg-sys-next` (không dùng lớp safe `ffmpeg-next` vì nó không bọc hwcontext). Bindings sinh lúc build nên chạy với FFmpeg 4.4 … 8.x.
- OpenH264 (crate `openh264`, BSD, build từ source) là phương án cuối khi FFmpeg không có encoder nào dùng được (ví dụ Fedora `ffmpeg-free` không có x264, hoặc build không có FFmpeg).
- Encoder phần mềm (x264, OpenH264) bị giới hạn 720p30 (PLAN.md: máy không có encoder phần cứng). `h264_v4l2m2m` giới hạn 1080p30.

## 2. Những gì đã kiểm chứng và chưa

Đã làm (2026-10-19, trên máy dev Windows + Docker Desktop):

- Windows: `cargo build -p nanocast-linux` sạch (chỉ có `convert`, `annexb`); `cargo test -p nanocast-linux` chạy 12 test.
- Windows, check chéo `--target x86_64-unknown-linux-gnu --no-default-features --features portal`: `cargo clippy --all-targets` sạch (X11, portal/ashpd, session, sources, capture, audio, example). Với default features, check chéo dừng ở build script của `libpulse-sys` / `pipewire-sys` / `ffmpeg-sys-next` / `openh264-sys2` (cần pkg-config, header và libclang của Linux) nên phần đó kiểm tra trong container:
- Container **Debian 12 bookworm** (PipeWire 0.3.65, libavcodec 59 = FFmpeg 5.1, như Raspberry Pi OS Bookworm): `cargo check --all-targets` mọi feature sạch, 13 test qua (thêm test `session::plan`), example build được.
- Container **Debian 13 trixie** (PipeWire/FFmpeg mới hơn): `cargo clippy --all-targets -- -D warnings` mọi feature sạch, 13 test qua.
- **Chạy thật trong container** (Xvfb 1920×1080, không GPU), `linux_cast` gửi tới receiver UDP giả bằng Python, ghép gói thành `.h264` rồi kiểm bằng `ffprobe` + `ffmpeg -f null`:

  | Thử | Kết quả |
  |---|---|
  | X11 màn hình, `libx264`, 30 fps, 5 s | 150 khung 1280×720 (tự hạ 720p30 vì encoder phần mềm), giải mã không lỗi; MIT-SHM + con trỏ XFixes chạy; chuyển màu 1080p→720p ~7 ms/khung, capture→gửi ~14–16 ms |
  | X11 màn hình, `openh264`, 30 fps | 150 khung, giải mã không lỗi; encode ~17–23 ms, capture→gửi ~25–35 ms |
  | X11 cửa sổ `xclock` (Composite pixmap), `libx264` | 150 khung, giải mã không lỗi |
  | Âm thanh PipeWire (`pipewire` + `wireplumber` trong container) | Kết nối PipeWire, negotiate F32LE/48 kHz, stream `nanocast-system-sound` vào graph; nhịp gói im lặng đúng (448 gói / 9 s, seq liên tục, magic 0x4E41), luồng Opus ~0,45 % một nhân. **Chưa** có mẫu âm thanh thật đi qua: WirePlumber không chạy được trong container (thiếu logind/system bus) nên stream không được nối vào monitor. |

  Số đo trên là trong container trên máy dev, không phải phần cứng mục tiêu. `encode_ms` của x264 trong lần chạy đó ghi 0 vì mốc thời gian lấy sau `avcodec_send_frame`; đã sửa (lấy trước), chưa chạy lại.

Chưa kiểm chứng (cần máy Linux thật):

- Portal: hộp thoại GNOME / KDE / wlroots, restore token (không hỏi lại lần sau), đóng cửa sổ hoặc bấm "Stop sharing" -> `EndReason::SourceClosed`. Toàn bộ `pw_video.rs` mới chỉ được biên dịch, chưa chạy.
- Định dạng PipeWire thực tế từng compositor (BGRx/BGRA; có compositor gửi DMA-BUF dù không khai báo modifier không?).
- X11 với window manager thật (`_NET_CLIENT_LIST`, cửa sổ bị che / ra ngoài màn hình / thu nhỏ), fallback không SHM.
- Encoder phần cứng: VA-API Intel (iHD, cả low-power), AMD (radeonsi), NVENC, v4l2m2m trên Pi 4; ép IDR theo `keyframe_request`; đổi bitrate khi đang chạy (x264/NVENC) và mở lại encoder (VA-API).
- Âm thanh có tiếng thật qua PipeWire `stream.capture.sink` (PipeWire ≥ 0.3.33) và fallback PulseAudio.
- Kênh control, PIN, adaptive bitrate, pause/blank với TV thật (code giống hệt Windows).
- Độ trễ glass-to-glass, CPU trên phần cứng thật, số đo cho `docs/benchmark.md`.

### 2.1 Tự kiểm tra lại trong container

Repo mount chỉ đọc, target dir trong volume (chạy từ thư mục repo):

```bash
docker run --rm -v "$PWD:/src:ro" -v nanocast-target:/target rust:1-bookworm sh -c '
  apt-get update && apt-get install -y --no-install-recommends pkg-config clang libclang-dev g++     libpipewire-0.3-dev libspa-0.2-dev libavcodec-dev libavutil-dev libpulse-dev &&
  rustup component add clippy && cd /src &&
  CARGO_TARGET_DIR=/target cargo clippy -p nanocast-linux --all-targets --locked &&
  CARGO_TARGET_DIR=/target cargo test -p nanocast-linux --locked'
```

Chạy thử hình không cần màn hình thật: thêm `xvfb x11-apps python3 ffmpeg`, chạy `Xvfb :99 &`, `DISPLAY=:99 NANOCAST_CAPTURE=x11 linux_cast 127.0.0.1 1 5 --encoder=libx264 --fps=30` và một receiver UDP cổng 48322 ghép gói theo header 14 byte (PLAN.md mục 4).

## 3. Gói cần cài

Rust stable (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`), rồi:

**Ubuntu 22.04 / 24.04, Debian 12, Linux Mint 21+**

```bash
sudo apt install build-essential pkg-config clang libclang-dev \
  libpipewire-0.3-dev libspa-0.2-dev libavcodec-dev libavutil-dev libpulse-dev \
  xdg-desktop-portal
```

Runtime theo GPU: Intel `intel-media-va-driver-non-free` (Gen 8+; i965 cho máy cũ: `i965-va-driver`), AMD `mesa-va-drivers`, NVIDIA: driver proprietary (NVENC có trong `libnvidia-encode`). Kiểm tra VA-API: `sudo apt install vainfo && vainfo` phải có dòng `VAProfileH264... : VAEntrypointEncSlice` (hoặc `EncSliceLP`). Backend portal theo desktop: `xdg-desktop-portal-gnome`, `xdg-desktop-portal-kde`, hoặc `xdg-desktop-portal-wlr` (Sway/Hyprland dùng `-hyprland`).

**Fedora 39+**

```bash
sudo dnf install gcc gcc-c++ pkgconf-pkg-config clang-devel \
  pipewire-devel libavcodec-free-devel libavutil-free-devel pulseaudio-libs-devel \
  xdg-desktop-portal
```

`ffmpeg-free` của Fedora không có x264 và chỉ có VA-API H.264 khi cài `mesa-va-drivers-freeworld` (RPM Fusion) hoặc `intel-media-driver`. Muốn đủ encoder: bật RPM Fusion và `sudo dnf swap ffmpeg-free ffmpeg --allowerasing`, rồi cài `ffmpeg-devel`. Không có encoder FFmpeg nào thì NANOCast tự dùng OpenH264 (720p30).

**Raspberry Pi OS 64-bit (Bookworm), Pi 4 / Pi 5**

```bash
sudo apt install build-essential pkg-config clang libclang-dev \
  libpipewire-0.3-dev libspa-0.2-dev libavcodec-dev libavutil-dev libpulse-dev
```

Bookworm Desktop mặc định là Wayland (labwc/wayfire) + PipeWire, có `xdg-desktop-portal-wlr`. Xem mục 6.

## 4. Build và chạy thử (không cần GUI)

```bash
cargo build --release -p nanocast-linux --example linux_cast
./target/release/examples/linux_cast --list
./target/release/examples/linux_cast 192.168.1.50                       # màn hình 1, 30 s
./target/release/examples/linux_cast 192.168.1.50 window:0x3a00007 60 --audio
./target/release/examples/linux_cast 192.168.1.50 1 30 --encoder=libx264 --fps=30 --mbps=8
```

- `--list` in back end đang dùng, encoder có trên máy, màn hình và cửa sổ (X11) hoặc mục "chọn trong hộp thoại" (Wayland).
- Biến môi trường: `NANOCAST_CAPTURE=x11|portal` ép back end; `NANOCAST_VAAPI_DEVICE=/dev/dri/renderD129` chọn GPU cho VA-API; `NANOCAST_FFMPEG_DEBUG=1` cho log chi tiết của FFmpeg; `NANOCAST_PIN=1234` mã 4 số của TV; `RUST_LOG=debug`.
- Thử không cần TV: chạy `nanocast-receiver` (receiver desktop) trên chính máy đó hoặc một PC khác, hoặc `nc -ul 48322 > /dev/null` để chỉ xem sender chạy.

## 5. Quyền và hộp thoại portal

- **Wayland**: ứng dụng không được tự chụp màn hình. Mỗi lần cast, xdg-desktop-portal hiện hộp thoại của desktop để người dùng chọn màn hình hoặc cửa sổ; NANOCast không chọn hộ được. Chọn trong GUI chỉ quyết định loại (màn hình / cửa sổ).
- **Nhớ lựa chọn**: portal ScreenCast phiên bản 4+ (GNOME 43+, KDE Plasma 5.27+, xdg-desktop-portal-wlr mới) cho phép giữ quyền: NANOCast lưu restore token theo chỉ số màn hình vào `~/.local/state/nanocast/portal-tokens`, lần sau cast cùng màn hình thì không hỏi nữa. Xoá file đó để hỏi lại. Cửa sổ thì luôn hỏi.
- Đang cast thì desktop hiện biểu tượng chia sẻ màn hình; bấm "Stop" ở đó sẽ kết thúc cast (`EndReason::SourceClosed`).
- **X11**: không có hộp thoại, không cần quyền; mọi app X11 đều chụp được màn hình (đó cũng là lý do Wayland chặn).
- **Âm thanh**: PipeWire/PulseAudio không cần quyền cho ứng dụng thường. Flatpak sẽ cần `--socket=pulseaudio` (chưa đóng gói Flatpak).
- **Firewall**: sender chỉ gửi ra; `ufw` / `firewalld` mặc định không chặn. mDNS tìm TV cần cho phép UDP 5353 vào (Fedora `firewalld` mặc định cho phép `mdns`; với `ufw` đang bật: `sudo ufw allow 5353/udp`).
- VA-API cần quyền đọc `/dev/dri/renderD*`: người dùng phải thuộc nhóm `render` (thường đã có sẵn; kiểm tra bằng `groups`, thiếu thì `sudo usermod -aG render $USER` rồi đăng nhập lại).

## 6. Raspberry Pi 4 / 5

- **Pi 4**: có khối mã hoá H.264 phần cứng qua V4L2 M2M (`/dev/video11`, driver `bcm2835-codec`), FFmpeg gọi là `h264_v4l2m2m`. Giới hạn **1080p30** (level 4.0), nên NANOCast tự hạ fps xuống 30. Buffer của V4L2 làm encoder trễ thêm 1–2 khung; ParamSets chèn SPS/PPS vì encoder này chỉ gửi chúng một lần. Đổi bitrate khi đang cast phải mở lại encoder (có IDR).
- **Pi 5**: **không có** encoder H.264 phần cứng (chỉ còn decoder HEVC). NANOCast dùng `libx264` (nếu FFmpeg có) hoặc OpenH264, ở 720p30, tốn khoảng 1–2 nhân Cortex-A76 (ước tính, chưa đo).
- Chuyển màu RGB->NV12 trên CPU là phần nặng nhất trên Pi (ước tính 10–20 ms/khung 1080p trên Pi 4). Đặt `--fps=30` và nếu vẫn chậm thì cast cửa sổ (khung 1280×720 với encoder phần mềm).
- Bookworm Desktop (Wayland, labwc/wayfire): dùng portal `xdg-desktop-portal-wlr`; bản wlr hỗ trợ chọn màn hình, **không hỗ trợ chọn cửa sổ** (NANOCast báo lỗi rõ ràng "cannot share a Window"). Muốn cast cửa sổ trên Pi: chuyển sang X11 trong `sudo raspi-config` > Advanced Options > Wayland > X11, khi đó dùng back end X11.
- GPU memory: không cần đổi `gpu_mem` cho v4l2m2m trên Bookworm 64-bit.
- Pi cũng có thể làm **receiver** (README mục *Raspberry Pi / Linux*), độc lập với phần sender này.

## 7. GUI `crates/app` trên Linux (đã nối, 2026-10-19)

`nanocast` (crates/app) đã chọn `nanocast-linux` bằng `cfg(target_os = "linux")`, giống cách làm với macOS (docs/macos.md mục 4):

1. `Cargo.toml` gốc có `nanocast-linux = { path = "crates/linux-capture" }` trong `[workspace.dependencies]`. `crates/app/Cargo.toml` có khối `[target.'cfg(target_os = "linux")'.dependencies]` với `nanocast-linux` (feature mặc định) và `eframe` (`glow`, `default_fonts`, `wayland`, `x11`).
2. `crates/app/src/main.rs`: các cổng `cfg` là `any(windows, target_os = "macos", target_os = "linux")`.
3. `crates/app/src/gui.rs`: `use nanocast_linux as platform` và các hàm riêng cho Linux:
   - `screen_permission()` trả `true`, `request_screen_permission()` không làm gì (quyền do hộp thoại portal lo).
   - `audio_supported()` = `sources::audio_supported()`.
   - `system_fonts()` lấy file đầu tiên tồn tại trong các đường dẫn DejaVu Sans (Debian/Ubuntu `/usr/share/fonts/truetype/dejavu/`, Fedora `/usr/share/fonts/dejavu-sans-fonts/`, Arch `/usr/share/fonts/TTF/`) và Noto Sans (Debian `truetype/noto/`, Fedora `google-noto/`, `google-noto-vf/`). Font nào thấy trước làm font chữ chính; không thấy font nào thì giữ font của egui.
   - `sender_name()` đọc `COMPUTERNAME`, `HOSTNAME`, rồi `/etc/hostname` (app GUI trên nhiều distro không có biến `HOSTNAME`).
4. Wayland: khi `sources::portal_picks_source()` là true và chưa cast, dưới hàng nút Cast hiện dòng "Hệ thống sẽ hỏi bạn chọn màn hình hoặc cửa sổ." (`i18n::Tr::portal_hint`). Danh sách cửa sổ chỉ có một mục (`capture::PORTAL_WINDOW`). Màn hình có kích thước 0×0 (không đọc được qua XWayland) chỉ hiện tên, không hiện "(0×0)".
5. Settings > Encoder: `mf_encoder::hw_encoder_names(VideoCodec::H264)` trả tên FFmpeg (`h264_vaapi`, …, `openh264`), HEVC trả danh sách rỗng. Phần này dùng chung code GUI, không phải sửa gì.

Đã kiểm tra trên máy dev Windows:

- `cargo build -p nanocast`, `cargo clippy -p nanocast --all-targets`, `cargo test -p nanocast` (Windows) vẫn sạch.
- `cargo clippy -p nanocast --target x86_64-unknown-linux-gnu --all-targets` với feature mặc định dừng ở build script `libpulse-sys` (cần pkg-config Linux), như mục 2.
- Bản sao workspace đặt `nanocast-linux` thành `default-features = false, features = ["portal"]`: `cargo clippy -p nanocast --target x86_64-unknown-linux-gnu --all-targets` sạch phần `nanocast` (code GUI cho Linux biên dịch được và không có cảnh báo).

Chưa làm:

- Clippy/build đầy đủ feature trong container (mục 2.1, đổi `-p nanocast-linux` thành `-p nanocast`). Lần này Docker Desktop không chạy nên chưa chạy được.
- Chạy cửa sổ GUI thật trên Linux (X11 và Wayland).
- Đóng gói: `.deb` bằng `cargo-deb` (phụ thuộc runtime `libpipewire-0.3-0`, `libavcodec59|libavcodec60|libavcodec61`, `libpulse0`, `xdg-desktop-portal`) và file `.desktop` + icon từ `assets/generated`. AppImage khó vì libavcodec/VA-API driver phải lấy từ hệ thống.

## 8. Ghi chú thiết kế

- Khung encoder cố định như Windows: màn hình giữ tỉ lệ thật, tối đa 1080p (hoặc 720p với encoder phần mềm); cửa sổ dùng khung đầy đủ cố định, co giãn + viền đen, đổi cỡ cửa sổ không tạo stream mới. Trên portal, kích thước lấy từ portal (có thể là pixel logic khi scale 2x); khung PipeWire kích thước nào cũng được co vào khung cố định.
- Thứ tự mở: kênh control -> nguồn (hộp thoại portal) -> encoder (quyết định khung + fps tối đa) -> bắt đầu stream. Bấm Dừng khi hộp thoại portal đang mở: cast kết thúc ngay, session portal đóng khi người dùng trả lời hộp thoại.
- PipeWire: chỉ lấy buffer mới nhất trong mỗi lần `process`, buffer cũ trả lại ngay; chuyển màu ngay trong callback (luồng `pw-video`). Không khai báo DMA-BUF modifier nên compositor dùng memfd; DMA-BUF tuyến tính vẫn đọc được bằng mmap + `DMA_BUF_IOCTL_SYNC`. Zero-copy DMA-BUF -> VA-API (import surface, scale_vaapi) là việc sau.
- X11: không có thông báo thay đổi nên chụp đều theo fps (encoder làm khung tĩnh rất nhỏ). Cửa sổ dùng Composite `NameWindowPixmap` (đúng nội dung kể cả khi bị che), không có Composite thì chụp vùng màn hình của cửa sổ. Cửa sổ thu nhỏ: TV giữ khung cuối.
- Encoder lỗi trước khung đầu tiên: thử encoder kế tiếp với cùng khung (giống Windows thử MFT kế tiếp). VA-API thử lần lượt các `/dev/dri/renderD*`, mỗi node thử entry point thường rồi low-power.
- Adaptive bitrate: x264 và NVENC đổi bitrate tại chỗ; VA-API, v4l2m2m, OpenH264 phải mở lại encoder (khung đầu là IDR). fps tối đa là fps encoder đã mở (30 với Pi 4 / phần mềm).
- Âm thanh: PipeWire tự chuyển sang F32LE 48 kHz stereo nên không cần resampler; output mặc định đổi giữa chừng thì session manager (WirePlumber) tự nối lại monitor mới (khác Windows).
- HEVC chưa làm: `hevc_vaapi` / `hevc_nvenc` dễ thêm qua cùng `ff_encoder.rs`, nhưng `annexb::ParamSets` cần thêm VPS và loại NAL HEVC.

## 9. Lỗi có thể gặp

| Triệu chứng | Nguyên nhân có thể | Cách xử lý |
|---|---|---|
| Build lỗi `pkg-config ... libpipewire-0.3 was not found` | Thiếu gói dev | Cài gói ở mục 3, hoặc build `--no-default-features --features openh264` (chỉ X11) |
| Build lỗi `Unable to find libclang` | bindgen cần libclang | `sudo apt install clang libclang-dev` (Fedora: `clang-devel`) |
| `xdg-desktop-portal ScreenCast is not available` | Chưa cài portal / backend, hoặc chạy ngoài phiên desktop | Cài `xdg-desktop-portal` + backend của desktop; trên X11 dùng `NANOCAST_CAPTURE=x11` |
| `screen sharing was cancelled in the system dialog` | Người dùng bấm Huỷ | Cast lại và chọn nguồn |
| `cannot share a Window` | Portal wlr không hỗ trợ cửa sổ | Cast màn hình, hoặc dùng phiên X11 |
| Hình đen trên Wayland khi ép `NANOCAST_CAPTURE=x11` | X11 qua XWayland chỉ thấy app X11 | Bỏ biến môi trường, dùng portal |
| Màu sai / hình méo chéo | Compositor gửi định dạng / stride lạ hoặc DMA-BUF có tiling | Gửi log dòng "PipeWire screen format"; báo lỗi kèm tên compositor |
| `h264_vaapi ... Function not implemented` / `No usable encoding entrypoint` | Driver VA-API không có encoder H.264 (Intel cũ dùng i965, AMD thiếu `mesa-va-drivers`, Fedora `ffmpeg-free`) | `vainfo`; cài driver ở mục 3; NANOCast tự lùi sang x264/OpenH264 |
| VA-API chọn nhầm GPU (laptop 2 GPU) | Node `renderD128` là GPU khác | `NANOCAST_VAAPI_DEVICE=/dev/dri/renderD129` |
| `h264_nvenc ... Cannot load libcuda.so.1` | Không có driver NVIDIA proprietary | Bình thường trên máy không có NVIDIA; tự bỏ qua |
| Pi 4: `h264_v4l2m2m ... Could not find a valid device` | Thiếu `/dev/video11` / user không thuộc nhóm `video` | `ls /dev/video1*`, `sudo usermod -aG video $USER` |
| TV đen, log "frame too large to packetize" | Keyframe quá to | Giảm bitrate |
| fps thấp, `convert_ms` cao | Chuyển màu CPU chậm (Pi, máy cũ) | Giảm fps (`--fps=30`), cast cửa sổ, hoặc dùng encoder phần mềm (khung 720p) |
| Không có tiếng, `audio_kbps` ≈ 4 | Không có gì đang phát, hoặc PipeWire < 0.3.33 (không có `stream.capture.sink`, ghi micro thay vì loa) | Phát thử nhạc; nâng PipeWire; hoặc tắt PipeWire audio để dùng PulseAudio monitor |
| `no system sound capture` | Không có PipeWire lẫn PulseAudio | Cài `pipewire-pulse` / `pulseaudio` |
| Không thấy TV | mDNS bị firewall chặn | Mục 5; hoặc nhập IP tay |

## 10. Việc tiếp theo

1. Chạy `linux_cast` trên: Ubuntu 24.04 GNOME Wayland (Intel iGPU), KDE Plasma Wayland (AMD), một phiên X11, Raspberry Pi 4 và Pi 5 (Bookworm). Sửa lỗi lúc chạy, ghi số đo vào `docs/benchmark.md`.
2. Nối GUI theo mục 7, đóng gói `.deb`.
3. Zero-copy: nhận DMA-BUF có modifier từ PipeWire, import thẳng vào VA surface (`AV_PIX_FMT_DRM_PRIME` -> `hwmap` sang VAAPI) và co giãn/đổi màu bằng `scale_vaapi`, bỏ chuyển màu CPU.
4. HEVC qua `hevc_vaapi` / `hevc_nvenc` (ParamSets cho VPS/SPS/PPS HEVC).
5. CI Linux: thêm job `ubuntu-latest` chạy `cargo clippy -p nanocast-linux` với các gói ở mục 3.
