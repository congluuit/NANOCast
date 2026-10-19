# NANOCast

Cast màn hình PC (hoặc đúng một ứng dụng) lên TV cùng mạng LAN, độ trễ thấp, dùng cho dạy học.
Miễn phí, mã nguồn mở (MIT hoặc Apache-2.0), viết bằng Rust. Không tài khoản, không internet, không cài driver.

- PC: `nanocast.exe` (Windows 10/11), một file ~7 MB, không cần quyền admin.
- TV: app **NANOCast Receiver** cho Android TV / Google TV / box Android (`receiver-androidtv/`).

Kế hoạch và giao thức: [PLAN.md](PLAN.md). Số đo và vấn đề đã gặp: [docs/benchmark.md](docs/benchmark.md).

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
4. **Dừng** để ngừng. Đóng app cũng dừng cast.

Dòng trạng thái hiện fps, Mbps, thời gian encode phía PC và số liệu TV gửi về (fps, gói mất, decode ms, số lần TV xin keyframe).
**Cài đặt**: encoder phần cứng (mặc định *Tự động*: thử lần lượt theo thứ tự ưu tiên, bỏ qua cái không chạy), bitrate 10/20/30 Mbps, 30/60 fps.

Hạn chế hiện tại: chưa có âm thanh (Phase 1b); cần encoder H.264 phần cứng (Intel Quick Sync, NVIDIA, AMD) qua Media Foundation; nội dung có DRM (Netflix…) sẽ đen; Windows 10 luôn có viền vàng quanh vùng đang cast.

Thử không cần TV: `cargo run -p nanocast-core --example advertise -- "TV thử"` quảng bá một TV giả để xem danh sách trong app.

## Cấu trúc

| Thư mục | Nội dung |
|---|---|
| `crates/app` | `nanocast.exe`: GUI egui |
| `crates/win-capture` | `nanocast-win`: capture WGC → NV12 → Media Foundation H.264 → UDP, kênh control |
| `crates/core` | Giao thức, discovery mDNS, kênh control |
| `crates/spike-win`, `crates/spike-send` | Công cụ đo/thử dòng lệnh |
| `receiver-androidtv` | Receiver Android TV (Kotlin) |
| `tools/icongen`, `assets` | Icon |
