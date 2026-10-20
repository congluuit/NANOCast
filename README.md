# NANOCast

[![CI](https://github.com/redharegames/NANOCast/actions/workflows/ci.yml/badge.svg)](https://github.com/redharegames/NANOCast/actions/workflows/ci.yml)

Cast màn hình PC (hoặc đúng một ứng dụng) lên TV cùng mạng LAN, độ trễ thấp, dùng cho dạy học.
Miễn phí, mã nguồn mở (MIT hoặc Apache-2.0), viết bằng Rust. Không tài khoản, không internet, không cài driver.

- PC: `nanocast.exe` (Windows 10/11), một file ~7 MB, không cần quyền admin. macOS: cùng GUI (`crates/mac-capture`, [docs/macos.md](docs/macos.md)), bản cài `.dmg` universal (Intel + Apple Silicon), đã viết và type-check nhưng chưa chạy trên Mac thật ([Cài trên Mac](#cài-trên-mac)).
- TV: app **NANOCast Receiver** cho Android TV / Google TV / box Android (`receiver-androidtv/`).

Kế hoạch và giao thức: [PLAN.md](PLAN.md). Số đo và vấn đề đã gặp: [docs/benchmark.md](docs/benchmark.md).

## Tải về / Cài đặt

Mọi bản phát hành ở [GitHub Releases](https://github.com/redharegames/NANOCast/releases/latest):

| File | Dùng khi |
|---|---|
| `NANOCast-Setup-<version>.exe` | Cài đặt bình thường (tiếng Việt / English). Mặc định cài cho riêng người dùng hiện tại vào `%LOCALAPPDATA%\Programs\NANOCast`, **không cần quyền admin**; chọn "cho mọi người dùng" thì cài vào Program Files (cần admin). Có shortcut Start Menu, tuỳ chọn Desktop, gỡ trong *Settings → Apps*. |
| `NANOCast-<version>-portable-win64.zip` | Không muốn cài: giải nén, chạy `nanocast.exe` (chép USB mang theo được). |
| `NANOCast-Receiver-<version>-androidtv.apk` | Cài lên TV / box Android: `adb install -r <file>.apk`, hoặc chép vào USB và mở bằng trình quản lý file trên TV (bật *Cho phép nguồn không xác định*). |
| `NANOCast-<version>-macos-universal.dmg` | Mac (Intel và Apple Silicon), macOS 12.3+. Xem [Cài trên Mac](#cài-trên-mac). |

Cài im lặng (triển khai cho cả phòng máy): `NANOCast-Setup-<version>.exe /VERYSILENT /TASKS=desktopicon`.

Đóng gói tại máy (cần [Inno Setup 6](https://jrsoftware.org/isinfo.php), ví dụ `winget install JRSoftware.InnoSetup`):

```bash
cargo build --release -p nanocast
ISCC.exe installer\nanocast.iss
powershell -ExecutionPolicy Bypass -File installer\make-portable.ps1 -NoBuild
```

Kết quả nằm trong `installer/out/`. Phiên bản lấy từ `Cargo.toml` (`[workspace.package] version`). Phát hành: tăng version, push tag `v<version>`; workflow `release.yml` tự build installer, zip portable, APK, `.dmg` macOS và tạo GitHub Release.

### Cài trên Mac

> Bản Mac chưa được chạy thử trên Mac thật; nếu gặp lỗi, xin mở issue kèm phiên bản macOS và loại máy (Intel / Apple Silicon).

Cần macOS 12.3 trở lên, Mac Intel hoặc Apple Silicon (M1/M2/M3/M4…).

1. Tải `NANOCast-<version>-macos-universal.dmg` ở [GitHub Releases](https://github.com/redharegames/NANOCast/releases/latest), mở ra, kéo **NANOCast** vào **Applications**.
2. Mở lần đầu: app chưa ký Developer ID nên macOS chặn. **Chuột phải** vào NANOCast trong Applications > **Open** > **Open** (chỉ một lần). macOS 15 trở lên: mở thử một lần, rồi vào *System Settings > Privacy & Security*, bấm **Open Anyway**. Hoặc chạy trong Terminal:
   ```bash
   xattr -dr com.apple.quarantine /Applications/NANOCast.app
   ```
3. Cấp quyền ghi màn hình: *System Settings > Privacy & Security > Screen Recording* (macOS 15: *Screen & System Audio Recording*), bật **NANOCast**, rồi **thoát hẳn (Cmd+Q) và mở lại** app.
4. macOS 15+: khi được hỏi quyền mạng cục bộ (Local Network), chọn **Allow**, nếu không sẽ không thấy TV.

Tự đóng gói `.dmg` trên Mac (cần `xcode-select --install` và [rustup](https://rustup.rs)):

```bash
bash installer/macos/build-mac.sh
```

Kết quả: `installer/out/NANOCast-<version>-macos-universal.dmg` (thêm `--per-arch` để có bản riêng `-arm64` / `-x86_64`). Chi tiết, quyền và lỗi thường gặp: [docs/macos.md](docs/macos.md) mục 3a–3b.

## Build (PC)

Cần Rust stable (MSVC) trên Windows.

```bash
cargo build --release
```

Kết quả: `target/release/nanocast.exe`. Chép đi đâu cũng chạy được (icon và mọi thứ đã nhúng sẵn).

### Sender Linux (chưa kiểm chứng)

`crates/linux-capture` (`nanocast-linux`): chụp màn hình/cửa sổ qua xdg-desktop-portal + PipeWire (Wayland; hệ thống hiện hộp thoại chọn nguồn) hoặc X11 (MIT-SHM), mã hoá H.264 bằng FFmpeg (VA-API Intel/AMD, NVENC, V4L2 M2M trên Raspberry Pi 4, x264) hoặc OpenH264, âm thanh từ PipeWire/PulseAudio. Viết xong nhưng chưa chạy trên máy Linux thật và chưa nối vào GUI. Thử bằng dòng lệnh (Ubuntu/Debian):

```bash
sudo apt install build-essential pkg-config clang libclang-dev libpipewire-0.3-dev libspa-0.2-dev libavcodec-dev libavutil-dev libpulse-dev
cargo run --release -p nanocast-linux --example linux_cast -- --list
cargo run --release -p nanocast-linux --example linux_cast -- <ip-tv> 1 30 --audio
```

Gói cho Fedora / Raspberry Pi OS, quyền portal, giới hạn Pi 4/5 và các lỗi hay gặp: [docs/linux.md](docs/linux.md).

## Cài receiver lên TV

Build APK và cài qua `adb` theo hướng dẫn trong [docs/benchmark.md](docs/benchmark.md) (mục *Spike 2 → Build và cài*):

```bash
cd receiver-androidtv
gradlew.bat assembleDebug
adb connect <ip-tv>:5555
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Mở app *NANOCast* trên TV; góc trái hiện IP của TV. Nên bật **Game Mode** trên TV để giảm trễ.

Hướng dẫn đầy đủ (Game Mode từng hãng TV, bật Developer options, cài APK không cần PC, mẹo mạng, đo độ trễ bằng điện thoại): [docs/tv-setup.md](docs/tv-setup.md). Đăng lên Google Play: [docs/play-store.md](docs/play-store.md).

**Mã 4 số (tuỳ chọn, mặc định tắt)**: bấm **OK** trên điều khiển TV để bật/tắt. Khi bật, góc dưới phải hiện `Mã / Code 1234`; PC phải nhập đúng mã mới cast được, và TV chỉ nhận hình/tiếng từ đúng PC đó (học sinh không cast đè được). Mã được nhớ qua các lần mở app; tắt rồi bật lại thì có mã mới.

## Cách dùng

`nanocast.exe` dẫn từng bước, mỗi màn hình một việc; chữ to, không thuật ngữ. Lần đầu mở có **hướng dẫn 3 trang** (bấm **Bỏ qua** nếu đã quen; xem lại bằng nút **?** góc trên).

| 1. Chọn TV | 2. Chiếu gì | 3. Đang cast |
|---|---|---|
| ![Chọn TV](docs/screenshots/1-chon-tv.png) | ![Chiếu gì](docs/screenshots/2-chieu-gi.png) | ![Đang cast](docs/screenshots/3-dang-cast.png) |

1. **Chọn TV.** TV và máy tính cùng mạng Wi-Fi/LAN; mở app NANOCast trên TV. TV tự hiện thành thẻ (app luôn tìm, không cần bấm gì):
   - **Nhanh nhất · cần app NANOCast**: TV / box Android hoặc PC đang chạy app NANOCast (receiver). TV phải cài app này (APK) mới hiện ở đây và mới thêm được bằng địa chỉ IP. Nút **Tải app NANOCast cho TV (APK)** ngay dưới danh sách (và trong hướng dẫn lần đầu) tải đúng file `NANOCast-Receiver-<version>-androidtv.apk` của phiên bản đang dùng từ GitHub Releases; chép vào USB rồi mở trên TV, hoặc tải thẳng trên TV bằng app Downloader (xem [docs/tv-setup.md](docs/tv-setup.md)).
   - **Không cần cài, hơi trễ**: TV có sẵn Chromecast (Google TV, Android TV, Chromecast). Loa Google Home / Nest không hiện.
   - Không thấy TV? Bấm **TV của tôi không hiện?**: 3 điều cần kiểm tra và ô gõ địa chỉ IP đang hiện trên TV (mạng trường hay chặn việc tự tìm).
2. **Chiếu gì.** Hai nút lớn: **Toàn màn hình** (có nhiều màn hình thì chọn ở ô *Màn hình*) hoặc **Một ứng dụng** (danh sách cửa sổ đang mở, tên dễ hiểu: PowerPoint, Google Chrome…; cửa sổ hệ thống đã được lọc bỏ). Chỉ cửa sổ đó lên TV, kể cả khi bị che; menu bật ra là cửa sổ riêng nên không hiện, cần thấy menu thì chọn Toàn màn hình. Công tắc **Âm thanh** (mặc định tắt) gửi tiếng của máy tính (YouTube, PowerPoint…) ra loa TV.
   TV đang bật **mã 4 số** (thẻ có ổ khoá, *Cần mã trên TV*): app hỏi mã đang hiện trên TV bằng một ô số lớn; nhập một lần, app nhớ cho lần sau.
3. **Đang cast lên &lt;TV&gt;.** Thanh trạng thái bằng lời: *Tốt* / *Mạng yếu, đang giảm chất lượng* / *Mất kết nối* / *Đang tạm dừng* / *Màn hình đen*. Ba nút lớn:
   - **Tạm dừng**: TV giữ hình đang hiện, máy tính ngừng gửi hình và tiếng (giáo viên làm việc riêng); **Tiếp tục** để chiếu lại. Phím tắt toàn cục **Ctrl+Shift+P** (dùng được cả khi cửa sổ đang ẩn).
   - **Màn hình đen**: TV tô đen (học sinh tập trung nghe giảng); **Hiện lại** để chiếu tiếp.
   - **Dừng**: ngừng cast, quay về bước 2.

   **Đổi nội dung chiếu** quay về bước 2 mà không dừng; công tắc Âm thanh đổi được ngay khi đang cast. Số liệu kỹ thuật (fps, Mbps, encode/decode ms, gói mất, bitrate và lý do, bộ mã hoá, xử lý hình) nằm trong **Chi tiết**.

**Khi có sự cố**, app nói bằng lời kèm cách sửa, mã lỗi kỹ thuật chỉ nằm trong *Chi tiết*:
- Mất kết nối với TV (Wi-Fi rớt, tắt app trên TV): *Mất kết nối, tự thử lại sau 10 giây*, có nút **Thử lại ngay**; TV quay lại là tự chiếu tiếp. Thử 6 lần (~1,5 phút) không được thì dừng và ghi các bước kiểm tra.
- Gõ IP sai: *Địa chỉ chưa đúng. Ví dụ: 192.168.1.20*. Mã sai: *Mã chưa đúng, hãy xem lại trên TV*.
- Không mã hoá / không lấy được hình: nói rõ và gợi ý đổi Bộ mã hoá hoặc chọn Toàn màn hình.
- Chromecast chưa cast được (bản hiện tại chưa hỗ trợ TV đó): gợi ý cài app NANOCast lên TV.

**Khay hệ thống (Windows)**: đóng cửa sổ khi đang cast thì app *không* thoát mà thu xuống khay (góc dưới phải), vẫn cast. Bấm đúp biểu tượng để mở lại; chuột phải: **Mở NANOCast**, **Tạm dừng / Tiếp tục**, **Dừng cast**, **Thoát**. Không cast thì đóng cửa sổ là thoát. macOS/Linux: đóng cửa sổ khi đang cast chỉ thu nhỏ.

**Cài đặt** (nút bánh răng; lưu tự động trong `%APPDATA%\NANOCast\config.json`):

![Cài đặt](docs/screenshots/4-cai-dat.png)

- **Ngôn ngữ**: Tiếng Việt (mặc định) / English.
- **Chất lượng hình**: **Tự động** (20 Mbps, 60 fps, tự chỉnh theo mạng; nên dùng), **Nét** (30 Mbps, 30 fps: chữ nhỏ rõ hơn, hợp tài liệu), **Mượt** (10 Mbps, 60 fps: hợp Wi-Fi yếu).
- **Tự cast lại khi mở NANOCast**: mở app là chiếu màn hình lên TV dùng lần trước ngay khi thấy TV đó (chờ tối đa 20 s). App luôn nhớ TV cuối và mã của nó.
- **Xem lại hướng dẫn**.
- **Nâng cao**:
  - **Codec** H.264 (mặc định) hoặc HEVC (nhẹ mạng hơn). HEVC chỉ dùng khi TV báo giải mã được; nếu không app tự dùng H.264 và ghi lý do trong Chi tiết.
  - **Bộ mã hoá** phần cứng (mặc định *Tự động*: thử lần lượt, bỏ qua cái không chạy).
  - **Bitrate** 10/20/30 Mbps + **Tự chỉnh theo mạng**: TV báo mất gói > 1 %/s hoặc xin keyframe liên tục thì hạ 30 → 20 → 15 → 10 Mbps (dưới 15 Mbps thì 30 fps), 10 s ổn định thì tăng lại. **Khung hình** 30/60 fps. Chọn khác ba mức ở trên thì phần Chất lượng ghi *thiết lập riêng*.
  - **Xử lý hình** (Windows) Tự động / Card đồ hoạ / CPU và **Bỏ qua khung hình không đổi**.

Giao diện sáng/tối theo hệ thống (`NANOCAST_THEME=light|dark` để ép). Chữ dùng font **Be Vietnam Pro** nhúng sẵn (đủ dấu tiếng Việt, giấy phép SIL OFL 1.1, xem [assets/fonts/OFL.txt](assets/fonts/OFL.txt)).

Hạn chế hiện tại: âm thanh lấy từ thiết bị phát mặc định của Windows (đổi thiết bị mặc định khi đang cast thì tắt rồi bật lại **Âm thanh**); cần encoder phần cứng (Intel Quick Sync, NVIDIA, AMD) qua Media Foundation; nội dung có DRM (Netflix…) sẽ đen; Windows 10 luôn có viền vàng quanh vùng đang cast; Chromecast không có Tạm dừng / Màn hình đen.

Thử không cần TV: `cargo run -p nanocast-core --example advertise -- "TV thử"` quảng bá một TV giả để xem danh sách trong app, hoặc chạy `nanocast-receiver --windowed` trên cùng máy.

## Receiver desktop (PC cũ / mini PC / Raspberry Pi cắm HDMI vào TV, máy chiếu)

TV không phải Android (Samsung, LG, máy chiếu thường): cắm một PC cũ / mini PC / Raspberry Pi vào cổng HDMI và chạy `nanocast-receiver`. Nó quảng bá mình qua mDNS (tên = tên máy) nên hiện trong danh sách của `nanocast.exe` như một TV, nhận hình + tiếng và chiếu toàn màn hình.

```bash
cargo build --release -p nanocast-receiver
target/release/nanocast-receiver
```

- Màn hình chờ hiện tên máy và IP (để gõ tay vào `nanocast.exe` khi mạng chặn mDNS).
- Phím: **O** bật/tắt số liệu (fps, gói mất, decode ms), **F**/**F11** full screen, **Esc** thoát.
- Tuỳ chọn: `--name "Phong 12"` (chữ trên màn hình chỉ có A–Z, 0–9, dấu tiếng Việt hiện thành `?`), `--windowed`, `--on-top`, `--overlay`, `--no-audio`, `--no-mdns`, `--max 1920x1080` (độ phân giải quảng bá), `--pin` (mã 4 số ngẫu nhiên, hiện trên màn hình chờ) hoặc `--pin-code 1234` (tự chọn mã), `--exit-after <giây>` (đo thử). Log chi tiết: `RUST_LOG=debug`. Chọn backend GPU: `WGPU_BACKEND=dx12|vulkan|gl`.
- Tạm dừng: giữ hình cuối, góc trên phải có dấu `‖`. Màn hình đen: tô đen. Mã 4 số: như receiver Android (sai mã thì bị từ chối, chỉ nhận UDP từ PC đã nhập đúng mã).
- **Chỉ giải mã H.264** (không có HEVC): receiver báo `codecs: ["h264"]` nên `nanocast.exe` tự gửi H.264 dù Cài đặt chọn HEVC.
- Cổng cần mở trên firewall: UDP 48322 (hình), UDP 48324 (tiếng), TCP 48323 (control), UDP 5353 (mDNS). Windows hỏi lần đầu: chọn mạng Private.
- Decode hiện là **OpenH264 phần mềm** (không cần cài gì; 1080p60 ~4 ms/khung trên i9, xem [docs/benchmark.md](docs/benchmark.md) mục *Receiver desktop*). Máy yếu: để sender ở **30 fps**. Có [NASM](https://www.nasm.us/) trong PATH lúc build thì OpenH264 dùng SIMD và nhanh hơn. Decoder phần cứng (d3d11va/dxva2, vaapi/v4l2m2m) chưa làm.
- Hiển thị qua wgpu: cần GPU có DX12, Vulkan hoặc OpenGL ES 3.0 / OpenGL 3.3.

Thử không cần PC khác: `spike-win --seconds 8 --out test.h264` rồi `spike-send test.h264 --to 127.0.0.1 --loop`; tiếng: `cargo run --release -p nanocast-receiver --example send_tone`.

### Raspberry Pi / Linux (chưa kiểm chứng)

Chưa chạy thử trên Pi/Linux; các bước dưới đây là dự kiến.

- Raspberry Pi 4 / 5 (Pi 3 trở xuống chỉ có OpenGL ES 2 → wgpu không chạy), **Raspberry Pi OS 64-bit (Bookworm) bản Desktop**: receiver cần phiên đồ hoạ X11/Wayland (winit), không chạy từ console trần.
- Gói build và runtime:

```bash
sudo apt install build-essential pkg-config libasound2-dev libxkbcommon-x11-0 libwayland-client0 mesa-vulkan-drivers
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo build --release -p nanocast-receiver
```

- Chạy: `./target/release/nanocast-receiver --name "Phong 12"` (thử `WGPU_BACKEND=gl` nếu Vulkan lỗi). Tự chạy khi đăng nhập: tạo `~/.config/autostart/nanocast-receiver.desktop` với `Exec=/home/pi/nanocast-receiver` và `Type=Application`; bật tự đăng nhập desktop trong `raspi-config`.
- Pi 4 decode 1080p phần mềm có thể không kịp 60 fps: đặt sender 30 fps (hoặc 720p). Decode phần cứng qua `v4l2m2m` là bước tiếp theo.
- Âm thanh ra HDMI: chọn thiết bị phát mặc định là HDMI trong cài đặt âm thanh của desktop (receiver dùng thiết bị mặc định qua ALSA/PipeWire).

## Cấu trúc

| Thư mục | Nội dung |
|---|---|
| `crates/app` | `nanocast.exe`: GUI egui 3 bước (`gui.rs`), icon/nút vẽ tay (`widgets.rs`), Chromecast (`chromecast.rs`), khay hệ thống + Ctrl+Shift+P (`tray.rs`), chuỗi VI/EN (`i18n.rs`) |
| `crates/win-capture` | `nanocast-win`: capture WGC → NV12 → Media Foundation H.264/HEVC → UDP, bitrate thích nghi, âm thanh WASAPI loopback → Opus |
| `crates/mac-capture` | `nanocast-mac`: sender macOS (ScreenCaptureKit → VideoToolbox H.264), cùng API, chưa kiểm chứng |
| `crates/linux-capture` | `nanocast-linux`: sender Linux (portal/PipeWire hoặc X11 → FFmpeg/OpenH264 H.264 → UDP), chưa kiểm chứng |
| `crates/core` | Giao thức, discovery mDNS, kênh control (cả phía sender: `control_link`), logic bitrate thích nghi (`adapt`), Opus audio |
| `crates/spike-win`, `crates/spike-send` | Công cụ đo/thử dòng lệnh (`--codec hevc`, file `.h265`); `cargo run -p nanocast-core --example ctrl_probe` gửi tay hello/pin/pause/blank tới receiver |
| `receiver-androidtv` | Receiver Android TV (Kotlin) |
| `crates/receiver-desktop` | `nanocast-receiver`: receiver cho PC cũ / mini PC / Raspberry Pi (OpenH264 + wgpu + cpal) |
| `tools/icongen`, `assets` | Icon |
| `installer`, `.github/workflows` | Installer Inno Setup, zip portable, CI và release |
