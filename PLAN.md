# NANOCast — Kế hoạch dự án

Phần mềm **miễn phí, mã nguồn mở, viết bằng Rust**, dùng cho dạy học:
mở app lên → tự quét TV cùng mạng → bấm chọn TV → cast ngay toàn màn hình hoặc **đúng một ứng dụng**
(Chrome/Brave/PowerPoint…), Full HD, độ trễ thấp nhất kỹ thuật cho phép, có nút bật/tắt âm thanh.
Không đăng nhập, không tài khoản, không internet, không thu phí.

---

## 1. Yêu cầu chốt

| Yêu cầu | Cách đáp ứng |
|---|---|
| Nhẹ, đơn giản | Một file `.exe` ~5–10 MB, không cài driver, không service nền, không dependency ngoài. Giao diện 1 cửa sổ: danh sách TV + 3 nút. |
| Rust, đa nền tảng | Lõi 100 % Rust; capture/encode qua API gốc từng hệ điều hành đứng sau một `trait` chung. Windows trước, macOS/Linux sau. |
| Mở app là thấy TV, chọn là cast | Receiver trên TV tự quảng bá qua mDNS; app PC quét trong <1 s; không PIN, không ghép đôi (tuỳ chọn mã 4 số cho lớp học, mặc định tắt). |
| Bật/tắt âm thanh | WASAPI loopback → Opus; nút toggle đổi được ngay trong lúc cast. Tắt = không capture, không tốn CPU. |
| HD | 1920×1080 @ 60 fps, H.264 15–30 Mbps (chữ nét khi cuộn trang). |
| "Không trễ" | Mục tiêu **≤ 40 ms glass-to-glass qua Ethernet, ≤ 70 ms qua Wi-Fi 5 GHz** (Chromecast hiện tại 300–1000 ms). Mắt người không phân biệt được với HDMI khi dạy học/trình chiếu. Về vật lý không thể = 0 ms qua mạng — xem §2. |
| Máy yếu dùng được | Encode bằng **phần cứng** (Intel iGPU/NVIDIA/AMD) qua Media Foundation → CPU ~2–5 %. Zero-copy: khung hình đi thẳng GPU → encoder, không qua RAM. Chỉ gửi khi màn hình có thay đổi. Máy không có HW encoder: tự hạ xuống 720p30 + encoder mềm. |
| Free | Giấy phép MIT hoặc Apache-2.0. Không telemetry. |

## 2. Thực tế về độ trễ (để đặt kỳ vọng đúng)

| Khâu | ms | NANOCast làm gì |
|---|---|---|
| Lấy khung hình | 0–16 | Windows Graphics Capture, nhận ngay khi màn hình đổi |
| Encode | 2–5 | HW encoder, không B-frame, CBR, intra-refresh |
| Mạng | 1–3 Ethernet / 5–25 Wi-Fi | UDP, không buffer, gói ≤1200 byte |
| Decode trên TV | 3–10 | MediaCodec low-latency mode |
| TV xử lý hình | 10–100 | **Người dùng phải bật Game Mode / tắt motion smoothing** — app hiện nhắc 1 lần |
| **Tổng** | **~25–50** | |

Muốn đúng 0 ms thì chỉ có HDMI/Wireless HDMI 60 GHz (phần cứng, 3–6 triệu). Ngoài phạm vi phần mềm.

## 3. Kiến trúc

```
PC (nanocast, Rust)                                     TV
+--------------------------------------------+          +-----------------------------+
| GUI egui: [TV list] [Cast màn hình]        |  mDNS    | NANOCast Receiver           |
|           [Cast app ▼] [🔊 bật/tắt] [Dừng] | <------- |  - Android TV / Google TV / |
|                                            |          |    Fire TV / box Android    |
| Capture (trait Capturer)                   |  UDP     |    (Kotlin, ~1k dòng)       |
|  win: Windows Graphics Capture (màn hình   |  video   |  - hoặc nanocast-receiver   |
|       hoặc 1 cửa sổ HWND) → D3D11 texture  | -------> |    (Rust) chạy trên PC cũ / |
|  mac: ScreenCaptureKit                     |  UDP     |    Raspberry Pi cắm HDMI    |
|  linux: PipeWire                           |  audio   |    vào TV/máy chiếu         |
|        ↓ GPU (không copy)                  | -------> |                             |
| Encoder (trait Encoder)                    |  TCP     |  decode HW → SurfaceView    |
|  win: Media Foundation H.264 HW            |  control |  Opus → AudioTrack          |
|       (tự chọn Intel/NVIDIA/AMD MFT)       | <------> |                             |
|  mac: VideoToolbox   linux: VA-API         |          +-----------------------------+
|  fallback: openh264 720p                   |
|        ↓                                   |
| Audio: WASAPI loopback → Opus 48 kHz       |
| Packetizer + control (protocol nhỏ, §4)    |
+--------------------------------------------+
```

### Cast đúng một ứng dụng
Windows Graphics Capture bắt trực tiếp cửa sổ (HWND), kể cả khi bị che hoặc thu nhỏ → không cần màn hình ảo, không cần driver.
Hạn chế: menu/dropdown là cửa sổ con riêng nên không hiện; app hiển thị chú thích "dùng chế độ Cast màn hình nếu cần thấy menu".
Cửa sổ không phải 16:9 → letterbox nền đen trên GPU.

### Vì sao không dùng giao thức Chromecast / Moonlight có sẵn
- Chromecast mirroring: receiver của Google buffer ~200 ms+, không điều khiển được, SDK yêu cầu đăng ký. Loại.
- Moonlight: rất tốt nhưng **TV phải chủ động kết nối tới PC và ghép PIN** → ngược với luồng "mở app PC, chọn TV". Giao thức cũng nặng (RTSP, ENet, mã hoá AES). Làm protocol riêng đơn giản hơn nhiều.

## 4. Giao thức NANOCast (cố ý tối giản)

- **Discovery:** receiver quảng bá `_nanocast._udp.local` (tên TV, phiên bản, codec hỗ trợ, độ phân giải tối đa). TXT: `w`, `h`, `codec`, `ver`, `ctrl` (cổng control; thiếu thì mặc định 48323). Crate `mdns-sd`. Fallback UDP broadcast cổng 48321 cho mạng chặn mDNS.
- **Control (TCP cổng 48323, mỗi dòng một object JSON, loại trong trường `"t"`):** sender mở kết nối tới receiver.
  - sender → receiver: `hello {ver, name}`, `start {w, h, fps, codec, audio: bool}`, `audio {on: bool}`, `stop`.
  - receiver → sender: `caps {ver, name, max_w, max_h, codecs: [..], low_latency: bool}` (trả lời `hello`), `keyframe_request {reason}` (tối đa 1 lần / 500 ms; reason: `packet_loss`, `decoder_start`, `decoder_backlog`), `stats {fps, lost, decode_ms}` mỗi 1 s (`lost` = số gói mất trong 1 s vừa qua).
  - Loại thông điệp lạ / trường lạ được bỏ qua để hai phía nâng cấp độc lập. Receiver chỉ giữ 1 sender: kết nối mới thay kết nối cũ.
  - Control là tuỳ chọn: không kết nối được thì sender vẫn stream UDP (chỉ mất khả năng xin keyframe).
  - Mã: `crates/core/src/control.rs` (enum `Message`, `write_msg`, `ControlReader`), `receiver-androidtv/.../ControlServer.kt`.
- **Video (UDP):** header 14 byte big-endian: `magic u16 | seq u32 | frame_id u32 | idx u16 | count u16` + payload H.264 Annex B ≤1200 byte (tối đa 1024 gói/khung ≈ 1,2 MB; keyframe 1080p có thể >300 KB nên u8 không đủ). Gói của một khung được **rải đều** theo tốc độ trần (mặc định 80 Mbps) để không tràn buffer UDP của TV. Mất gói → receiver gửi `keyframe_request`; sender ép keyframe (`AVEncVideoForceKeyFrame`). Intra-refresh chỉ dùng khi encoder hỗ trợ (QSV qua Media Foundation không hỗ trợ).
- **Audio (UDP):** Opus 20 ms, 96 kbps, header giống video. Tắt âm = không có luồng này.
- Không mã hoá (LAN lớp học); tuỳ chọn mã 4 số hiển thị trên TV khi bật trong receiver.

## 5. Lộ trình

### Phase 1 — MVP Windows → Android TV (3–4 tuần)
Mục tiêu: cast được, đo được số, dùng được trong lớp.

- [ ] Khởi tạo Cargo workspace (`core`, `app`, `receiver-desktop`), CI GitHub Actions build Windows.
- [ ] `Capturer` Windows: WGC màn hình + cửa sổ (crate `windows-capture` hoặc `windows-rs` trực tiếp), lấy D3D11 texture, phát hiện khung không đổi để bỏ qua.
- [ ] `Encoder` Windows: Media Foundation H.264 HW, nhận D3D11 texture (DXGI device manager), low-latency mode, CBR, không B-frame, GOP vô hạn + intra-refresh. Benchmark trên máy iGPU Intel đời 2015.
- [ ] Packetizer UDP + control TCP + mDNS browse.
- [ ] Audio: WASAPI loopback (`cpal`) → Opus (`audiopus`), toggle.
- [ ] GUI egui: danh sách TV tự cập nhật, nút Cast màn hình / Cast app (chọn từ danh sách cửa sổ) / Âm thanh / Dừng, hiển thị fps + bitrate + ms.
- [ ] Receiver Android TV (Kotlin): NsdManager quảng bá, nhận UDP, ráp frame, MediaCodec `KEY_LOW_LATENCY`, SurfaceView, AudioTrack. APK sideload + Play Store (free).
- [ ] Đo trễ bằng quay 240 fps, ghi `docs/benchmark.md`. Tối ưu cho tới khi đạt §7.

### Phase 2 — Hoàn thiện cho lớp học (2 tuần)
- [ ] Receiver desktop bằng Rust (`nanocast-receiver`): cho máy chiếu/TV thường qua PC cũ hoặc Raspberry Pi; decode bằng HW của máy đó.
- [ ] Tự thích nghi bitrate theo mất gói / RTT; hạ 1080p30 khi Wi-Fi yếu và báo trên GUI.
- [ ] Nút **Tạm dừng** (đóng băng hình trên TV khi giáo viên làm việc riêng) và **Màn hình đen**.
- [ ] Nhớ TV đã dùng, tự cast lại khi mở app (tuỳ chọn).
- [ ] Mã 4 số tuỳ chọn trên receiver để tránh học sinh cast đè.
- [ ] Installer nhỏ (Inno Setup) hoặc chỉ 1 exe portable; bản tiếng Việt / English.

### Phase 3 — Đa nền tảng (3–4 tuần)
- [ ] macOS: ScreenCaptureKit + VideoToolbox + audio tap (crate `screencapturekit`, `objc2`).
- [ ] Linux: PipeWire capture + VA-API encode.
- [ ] HEVC/AV1 cho TV mới (giảm bitrate một nửa ở cùng chất lượng), giữ H.264 làm mặc định.
- [ ] Receiver cho LG webOS / Samsung Tizen qua transport WebRTC (`webrtc-rs`) — tuỳ chọn, chỉ khi có nhu cầu thật; cách rẻ hơn là box Android TV ~500k.

## 6. Tech stack (Rust)

| Thành phần | Crate / API | Ghi chú |
|---|---|---|
| GUI | `eframe`/`egui` | Nhẹ (~3 MB), thuần Rust, không webview |
| Capture Windows | `windows-capture` hoặc `windows` (WGC) | Màn hình + cửa sổ, texture GPU |
| Encode Windows | `windows` (Media Foundation) | HW của mọi hãng, không cần SDK NVIDIA/AMD |
| Fallback encode | `openh264` | Máy không có HW encoder |
| Audio | `cpal` (WASAPI loopback) + `audiopus` | Opus 48 kHz stereo |
| Discovery | `mdns-sd` | Không cần daemon |
| Mạng | `tokio` (UDP/TCP) | |
| Receiver TV | Kotlin, MediaCodec, NsdManager | ~1k dòng, không thư viện ngoài |
| Receiver desktop | Rust + `ffmpeg-next` hoặc MF decoder | Phase 2 |
| Build | GitHub Actions, `cargo-dist` | Release tự động |

## 7. Tiêu chí nghiệm thu

- Mở app → thấy TV trong ≤ 1 s; bấm → TV hiện hình trong ≤ 2 s.
- 1080p60, chữ nhỏ trên trang web nét khi cuộn, không block.
- Glass-to-glass ≤ 40 ms (Ethernet), ≤ 70 ms (Wi-Fi 5 GHz), TV ở Game Mode.
- CPU sender ≤ 5 % trên laptop Intel i3 đời 2015 (iGPU encode); RAM ≤ 100 MB.
- Toggle âm thanh không làm giật hình; âm/hình lệch ≤ 40 ms.
- Chạy 2 tiết học liên tục (90 phút) không rớt, không tăng trễ.
- Binary Windows ≤ 10 MB, không cài driver, chạy không cần quyền admin.

## 8. Rủi ro

| Rủi ro | Giảm thiểu |
|---|---|
| TV lớp học là Samsung/LG (không Android) | Box Android TV / Fire Stick ~500k cắm HDMI, hoặc receiver desktop trên PC cũ (Phase 2); WebRTC receiver (Phase 3) |
| Wi-Fi trường yếu/nhiều người | Khuyến nghị dây mạng cho TV; adaptive bitrate; 1080p30 fallback |
| Máy giáo viên không có HW encoder | openh264 720p30, vẫn ≤ 100 ms |
| Media Foundation HW encoder có latency/bug tuỳ driver | Thử nghiệm sớm trên Intel/NVIDIA/AMD; cho phép chọn encoder trong Settings |
| Nội dung DRM (Netflix) bị đen | Giới hạn của Windows, ghi rõ trong README |
| mDNS bị chặn trên mạng trường | Fallback broadcast UDP + nhập IP tay |

## 9. Cấu trúc repo

```
NANOCast/
├── Cargo.toml              # workspace
├── assets/                 # icon.svg, banner.svg (vẽ tay), nanocast.ico, generated/
├── tools/icongen/          # render SVG -> PNG/ICO/mipmap Android (cargo run -p icongen)
├── crates/
│   ├── core/               # protocol, au, discovery, trait capture/encoder (có)
│   ├── app/                # GUI egui (sender); build.rs nhúng icon
│   ├── spike-win/          # spike capture+encode+stream Windows (có)
│   ├── spike-send/         # gửi file .h264 qua UDP để test receiver (có)
│   └── receiver-desktop/   # receiver Rust cho PC cũ / Pi (Phase 2)
├── receiver-androidtv/     # app Kotlin (có)
├── docs/
│   ├── benchmark.md        # số đo (có)
│   └── tv-setup.md         # hướng dẫn Game Mode từng hãng TV
└── .github/workflows/
```

## 10. Việc làm ngay

1. ~~Tạo workspace, viết spike: WGC → Media Foundation H.264 → ghi file `.h264`; đo thời gian encode mỗi khung.~~
   **Xong 2026-10-19** (`crates/spike-win`): QSV 1080p60, encode ~13 ms, capture→encoded ~16 ms. Số đo trong `docs/benchmark.md`.
2. ~~Spike receiver Android TV: nhận file/UDP → MediaCodec → hiện hình; đo trễ decode.~~
   **Xong 2026-10-19** (`receiver-androidtv/`): emulator 1080p60, decode ~11 ms (chưa đo TV thật).
3. ~~Nối 2 spike qua UDP~~ **Xong**: `spike-win --discover` stream thẳng tới TV, rải gói, keyframe theo yêu cầu; icon app xong.
   Còn lại: **đo glass-to-glass trên TV thật** (quay 240 fps), cập nhật `docs/benchmark.md`.
4. ~~Kênh control TCP (hello/caps/keyframe_request/audio on-off) ở cả hai phía~~
   **Xong 2026-10-19**: `nanocast_core::control`, `spike-win` (hello/start/stop, keyframe_request → ép IDR), receiver Android (caps, keyframe_request, stats 1 s, TXT `ctrl`). Kiểm chứng trên emulator, chưa trên TV thật; `audio` mới chỉ được nhận/ghi log (chưa có đường âm thanh).
   Tiếp theo: GUI egui dùng kênh này.
