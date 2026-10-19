# NANOCast

[![CI](https://github.com/redharegames/NANOCast/actions/workflows/ci.yml/badge.svg)](https://github.com/redharegames/NANOCast/actions/workflows/ci.yml)

Cast màn hình PC (hoặc đúng một ứng dụng) lên TV cùng mạng LAN, độ trễ thấp, dùng cho dạy học.
Miễn phí, mã nguồn mở (MIT hoặc Apache-2.0), viết bằng Rust. Không tài khoản, không internet, không cài driver.

- PC: `nanocast.exe` (Windows 10/11), một file ~7 MB, không cần quyền admin.
- TV: app **NANOCast Receiver** cho Android TV / Google TV / box Android (`receiver-androidtv/`).

Kế hoạch và giao thức: [PLAN.md](PLAN.md). Số đo và vấn đề đã gặp: [docs/benchmark.md](docs/benchmark.md).

## Tải về / Cài đặt

Mọi bản phát hành ở [GitHub Releases](https://github.com/redharegames/NANOCast/releases/latest):

| File | Dùng khi |
|---|---|
| `NANOCast-Setup-<version>.exe` | Cài đặt bình thường (tiếng Việt / English). Mặc định cài cho riêng người dùng hiện tại vào `%LOCALAPPDATA%\Programs\NANOCast`, **không cần quyền admin**; chọn "cho mọi người dùng" thì cài vào Program Files (cần admin). Có shortcut Start Menu, tuỳ chọn Desktop, gỡ trong *Settings → Apps*. |
| `NANOCast-<version>-portable-win64.zip` | Không muốn cài: giải nén, chạy `nanocast.exe` (chép USB mang theo được). |
| `NANOCast-Receiver-<version>-androidtv.apk` | Cài lên TV / box Android: `adb install -r <file>.apk`, hoặc chép vào USB và mở bằng trình quản lý file trên TV (bật *Cho phép nguồn không xác định*). |

Cài im lặng (triển khai cho cả phòng máy): `NANOCast-Setup-<version>.exe /VERYSILENT /TASKS=desktopicon`.

Đóng gói tại máy (cần [Inno Setup 6](https://jrsoftware.org/isinfo.php), ví dụ `winget install JRSoftware.InnoSetup`):

```bash
cargo build --release -p nanocast
ISCC.exe installer\nanocast.iss
powershell -ExecutionPolicy Bypass -File installer\make-portable.ps1 -NoBuild
```

Kết quả nằm trong `installer/out/`. Phiên bản lấy từ `Cargo.toml` (`[workspace.package] version`). Phát hành: tăng version, push tag `v<version>`; workflow `release.yml` tự build installer, zip portable, APK và tạo GitHub Release.

## Build (PC)

Cần Rust stable (MSVC) trên Windows.

```bash
cargo build --release
```

Kết quả: `target/release/nanocast.exe`. Chép đi đâu cũng chạy được (icon và mọi thứ đã nhúng sẵn).

## Cài receiver lên TV

Build APK và cài qua `adb` theo hướng dẫn trong [docs/benchmark.md](docs/benchmark.md) (mục *Spike 2 → Build và cài*):

```bash
cd receiver-androidtv
gradlew.bat assembleDebug
adb connect <ip-tv>:5555
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Mở app *NANOCast* trên TV; góc trái hiện IP của TV. Nên bật **Game Mode** trên TV để giảm trễ.

## Cách dùng

1. TV và PC cùng mạng (Wi-Fi/LAN). Mở app NANOCast trên TV.
2. Mở `nanocast.exe`: TV tự hiện trong danh sách (mDNS). Không thấy? Bấm **Quét lại**, hoặc gõ IP của TV vào ô **IP của TV** rồi **Thêm** (mạng trường hay chặn mDNS).
3. Chọn TV, rồi bấm:
   - **Cast màn hình**: cả màn hình (có nhiều màn hình thì chọn trong ô bên cạnh).
   - **Cast ứng dụng…**: chọn một cửa sổ (Chrome, PowerPoint…); chỉ cửa sổ đó lên TV, kể cả khi bị che. Menu bật ra là cửa sổ riêng nên không hiện; cần thấy menu thì dùng Cast màn hình.
4. **🔊 Âm thanh** (mặc định tắt): đánh dấu để gửi âm thanh của PC (mọi ứng dụng: video YouTube, PowerPoint…) lên loa TV; bỏ đánh dấu để tắt. Đổi được ngay trong lúc cast, hình không bị gián đoạn. Tắt thì PC không thu âm thanh nữa (không tốn CPU).
5. **Dừng** để ngừng. Đóng app cũng dừng cast.

Dòng trạng thái hiện fps, Mbps, thời gian encode phía PC, kbps âm thanh (~100 kbps khi có tiếng, ~4 kbps khi im lặng) và số liệu TV gửi về (fps, gói mất, decode ms, số lần TV xin keyframe).
**Cài đặt**: encoder phần cứng (mặc định *Tự động*: thử lần lượt theo thứ tự ưu tiên, bỏ qua cái không chạy), bitrate 10/20/30 Mbps, 30/60 fps.

Hạn chế hiện tại: âm thanh lấy từ thiết bị phát mặc định của Windows (đổi thiết bị mặc định khi đang cast thì bỏ chọn rồi chọn lại **Âm thanh**); cần encoder H.264 phần cứng (Intel Quick Sync, NVIDIA, AMD) qua Media Foundation; nội dung có DRM (Netflix…) sẽ đen; Windows 10 luôn có viền vàng quanh vùng đang cast.

Thử không cần TV: `cargo run -p nanocast-core --example advertise -- "TV thử"` quảng bá một TV giả để xem danh sách trong app.

## Receiver desktop (PC cũ / mini PC / Raspberry Pi cắm HDMI vào TV, máy chiếu)

TV không phải Android (Samsung, LG, máy chiếu thường): cắm một PC cũ / mini PC / Raspberry Pi vào cổng HDMI và chạy `nanocast-receiver`. Nó quảng bá mình qua mDNS (tên = tên máy) nên hiện trong danh sách của `nanocast.exe` như một TV, nhận hình + tiếng và chiếu toàn màn hình.

```bash
cargo build --release -p nanocast-receiver
target/release/nanocast-receiver
```

- Màn hình chờ hiện tên máy và IP (để gõ tay vào `nanocast.exe` khi mạng chặn mDNS).
- Phím: **O** bật/tắt số liệu (fps, gói mất, decode ms), **F**/**F11** full screen, **Esc** thoát.
- Tuỳ chọn: `--name "Phong 12"` (chữ trên màn hình chỉ có A–Z, 0–9, dấu tiếng Việt hiện thành `?`), `--windowed`, `--on-top`, `--overlay`, `--no-audio`, `--no-mdns`, `--max 1920x1080` (độ phân giải quảng bá), `--exit-after <giây>` (đo thử). Log chi tiết: `RUST_LOG=debug`. Chọn backend GPU: `WGPU_BACKEND=dx12|vulkan|gl`.
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
| `crates/app` | `nanocast.exe`: GUI egui |
| `crates/win-capture` | `nanocast-win`: capture WGC → NV12 → Media Foundation H.264 → UDP, kênh control, âm thanh WASAPI loopback → Opus |
| `crates/core` | Giao thức, discovery mDNS, kênh control, Opus audio |
| `crates/spike-win`, `crates/spike-send` | Công cụ đo/thử dòng lệnh |
| `receiver-androidtv` | Receiver Android TV (Kotlin) |
| `crates/receiver-desktop` | `nanocast-receiver`: receiver cho PC cũ / mini PC / Raspberry Pi (OpenH264 + wgpu + cpal) |
| `tools/icongen`, `assets` | Icon |
| `installer`, `.github/workflows` | Installer Inno Setup, zip portable, CI và release |
