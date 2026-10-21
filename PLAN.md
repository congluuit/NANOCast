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

- **Discovery:** receiver quảng bá `_nanocast._udp.local` (tên TV, phiên bản, codec hỗ trợ, độ phân giải tối đa). TXT: `w`, `h`, `codec` (`h264` hoặc `h264,hevc`), `ver`, `ctrl` (cổng control; thiếu thì mặc định 48323), `pin` (`1` = TV đang bật mã 4 số). Crate `mdns-sd`. Fallback UDP broadcast cổng 48321 cho mạng chặn mDNS.
- **Control (TCP cổng 48323, mỗi dòng một object JSON, loại trong trường `"t"`):** sender mở kết nối tới receiver.
  - sender → receiver: `hello {ver, name, pin?}`, `start {w, h, fps, codec, audio: bool}` (`codec`: `h264` | `hevc`), `audio {on: bool}`, `pause {on: bool}` (TV giữ khung cuối, sender ngừng capture/encode/tiếng), `blank {on: bool}` (TV tô đen), `stop`. `start` / `stop` / mất kết nối đều huỷ pause và blank.
  - receiver → sender: `caps {ver, name, max_w, max_h, codecs: [..], low_latency: bool}` (trả lời `hello`), `error {code, message}` (`code = "bad_pin"`: `hello` thiếu / sai mã; receiver đóng kết nối ngay sau đó), `keyframe_request {reason}` (tối đa 1 lần / 500 ms; reason: `packet_loss`, `decoder_start`, `decoder_backlog`), `stats {fps, lost, decode_ms}` mỗi 1 s (`lost` = số gói mất trong 1 s vừa qua).
  - Loại thông điệp lạ / trường lạ được bỏ qua để hai phía nâng cấp độc lập. Receiver chỉ giữ 1 sender: sender có `hello` được chấp nhận thay sender cũ (kết nối chưa `hello` hợp lệ không làm gì được).
  - **Mã 4 số** (tuỳ chọn, mặc định tắt; Android: nút OK trên remote, desktop: `--pin`): TV hiện mã, TXT `pin=1`; `hello` không có đúng `pin` → `error {code: "bad_pin"}` + đóng, sender đang cast không bị ảnh hưởng; UDP hình (48322) và tiếng (48324) chỉ được nhận từ IP của sender đã đưa đúng mã (hết kết nối thì lại cần mã).
  - **Codec**: sender chỉ gửi HEVC khi được chọn **và** `caps.codecs` có `"hevc"` (Android: khi máy có decoder `video/hevc`); receiver desktop chỉ có `["h264"]`. Không có kênh control → H.264.
  - **Bitrate thích nghi** (sender, `nanocast_core::adapt`): mỗi `stats` so với số gói đã gửi trong giây đó; mất > 1 %, ≥ 3 `keyframe_request` trong 5 s, hoặc TV giải mã < 80 % số khung gửi mà > 1 khung/khung → hạ một bậc 30 → 20 → 15 → 10 Mbps (tối đa 1 bậc / 2 s; < 15 Mbps thì 30 fps); 10 s sạch → tăng một bậc, không vượt bitrate cài đặt. Đổi bitrate trước hết bằng `CODECAPI_AVEncCommonMeanBitRate` lúc đang chạy; 1,5 s sau đo lại, MFT nhận lệnh mà vẫn giữ bitrate cũ (Intel QSV) thì mở lại encoder (khung đầu là IDR) cho mọi lần đổi sau; đổi fps luôn mở lại encoder.
  - Control là tuỳ chọn: không kết nối được thì sender vẫn stream UDP (chỉ mất khả năng xin keyframe).
  - Mã: `crates/core/src/control.rs` (enum `Message`, `write_msg`, `ControlReader`, `ERR_BAD_PIN`), `crates/core/src/control_link.rs` (phía sender, dùng chung Windows/macOS), `crates/receiver-desktop/src/control.rs`, `receiver-androidtv/.../ControlServer.kt`.
- **Video (UDP):** header 14 byte big-endian: `magic u16 | seq u32 | frame_id u32 | idx u16 | count u16` + payload H.264 (hoặc HEVC theo `start.codec`) Annex B ≤1200 byte (tối đa 1024 gói/khung ≈ 1,2 MB; keyframe 1080p có thể >300 KB nên u8 không đủ). Gói của một khung được **rải đều** theo tốc độ trần (mặc định 80 Mbps) để không tràn buffer UDP của TV. Mất gói → receiver gửi `keyframe_request`; sender ép keyframe (`AVEncVideoForceKeyFrame`). Intra-refresh chỉ dùng khi encoder hỗ trợ (QSV qua Media Foundation không hỗ trợ).
- **Audio (UDP cổng 48324, `AUDIO_PORT`):** header 10 byte big-endian `magic u16 = 0x4E41 ("NA") | seq u32 | pts_ms u32` + payload Opus; một gói = một frame Opus 20 ms, 48 kHz stereo, 96 kbps VBR, application Audio, complexity 3 (`AUDIO_*` trong `protocol.rs`). `seq` đếm gói (phát hiện mất, nối tiếp qua các lần bật/tắt trong một lần cast), `pts_ms` = thời điểm mẫu đầu của frame. **Payload rỗng = 20 ms im lặng**: khi không có gì phát (loopback trả 0 hoặc không trả gì) sender vẫn gửi mỗi 20 ms một gói chỉ có header (4 kbps) để receiver giữ nhịp; không dùng DTX vì libopus 1.3.1 chỉ có DTX ở chế độ SILK (thoại), khung CELT im lặng vẫn ~240 B. Opus pre-skip = 312 mẫu (OpusHead của receiver). Bật/tắt: sender gửi `audio {on}` trên kênh control; tắt = dừng capture hẳn, không có luồng UDP này. Receiver giữ ≤ 80 ms trong AudioTrack, bỏ gói khi đầy hoặc khi > 100 ms đang chờ, `audio {on:false}` / `stop` → xả buffer và im lặng. Mã: `crates/core/src/audio.rs`, `crates/win-capture/src/audio.rs`, `receiver-androidtv/.../AudioPlayer.kt`.
- Không mã hoá (LAN lớp học); tuỳ chọn mã 4 số hiển thị trên TV khi bật trong receiver (xem *Mã 4 số* ở trên; mã đi dạng rõ trên LAN, chỉ chống cast nhầm/cast đè, không phải bảo mật).

## 5. Lộ trình

### Phase 1 — MVP Windows → Android TV (3–4 tuần)
Mục tiêu: cast được, đo được số, dùng được trong lớp.

- [x] Khởi tạo Cargo workspace (`core`, `app`, `receiver-desktop`), CI GitHub Actions build Windows. *(CI chưa chạy trên GitHub vì chưa push)*
- [x] `Capturer` Windows: WGC màn hình + cửa sổ (crate `windows-capture` hoặc `windows-rs` trực tiếp), lấy D3D11 texture, phát hiện khung không đổi để bỏ qua. *(2026-10-20: hash ô 64×64 bằng compute shader trên device của WGC, khung giống thì không convert/encode/gửi, tối đa 1 s lặp lại khung cuối; BGRA→NV12 + co giãn/viền đen bằng `ID3D11VideoProcessor` ngay trên texture WGC (`nanocast_win::gpu`), đường CPU cũ làm fallback tự động, ép bằng `NANOCAST_CONVERT=gpu|cpu`. Số đo: `docs/benchmark.md` mục zero-copy GPU.)*
- [x] `Encoder` Windows: Media Foundation H.264 HW, nhận D3D11 texture (DXGI device manager), low-latency mode, CBR, không B-frame, GOP vô hạn + intra-refresh. Benchmark trên máy iGPU Intel đời 2015. *(2026-10-20: `MFT_MESSAGE_SET_D3D_MANAGER` trước media type khi MFT báo `MF_SA_D3D11_AWARE`, sample `MFCreateDXGISurfaceBuffer`; QSV 1080p60 chuyển động: CPU tiến trình 106 % → 13 % một nhân, tĩnh → 1,9 % và ~10 kbps. Intra-refresh: QSV qua MF không hỗ trợ (keyframe theo yêu cầu thay thế). NVIDIA MFT vẫn E_UNEXPECTED kể cả `MFTEnum2` theo LUID. **Còn thiếu**: đo trên máy iGPU Intel đời 2015 thật — mới giả lập bằng 2 nhân E; ô chọn GPU/CPU trong GUI.)*
- [x] Packetizer UDP + control TCP + mDNS browse.
- [x] Audio: WASAPI loopback (`cpal`) → Opus (`unsafe-libopus`: libopus 1.3.1 chuyển sang Rust, không cần CMake/C), toggle.
- [x] GUI egui: danh sách TV tự cập nhật, nút Cast màn hình / Cast app (chọn từ danh sách cửa sổ) / Âm thanh / Dừng, hiển thị fps + bitrate + ms (+ kbps âm thanh).
- [x] Receiver Android TV (Kotlin): NsdManager quảng bá, nhận UDP, ráp frame, MediaCodec `KEY_LOW_LATENCY`, SurfaceView, AudioTrack. APK sideload + Play Store (free).
- [ ] Đo trễ bằng quay 240 fps, ghi `docs/benchmark.md`. Tối ưu cho tới khi đạt §7.

### Phase 2 — Hoàn thiện cho lớp học (2 tuần)
- [x] Receiver desktop bằng Rust (`nanocast-receiver`): cho máy chiếu/TV thường qua PC cũ hoặc Raspberry Pi. *(2026-10-19: mDNS + UDP + control + audio, OpenH264 phần mềm + wgpu full screen, kiểm chứng trên Windows với `spike-send`/`spike-win`; còn: decode HW của máy đó (d3d11va/vaapi/v4l2m2m), chạy thử Pi/Linux — `docs/benchmark.md` mục Receiver desktop)*
- [x] Tự thích nghi bitrate theo mất gói / keyframe_request / decode của TV; hạ 1080p30 khi Wi-Fi yếu và báo trên GUI (dòng *Bitrate N Mbps · F fps (lý do)*). *(2026-10-19: §4 Bitrate thích nghi; kiểm chứng với receiver giả báo mất 3 %: 30→20→15 Mbps @60 → 10 Mbps @30 → 15 → 20 sau 10 s sạch; chưa: RTT, Wi-Fi thật)*
- [x] Nút **Tạm dừng** (đóng băng hình trên TV khi giáo viên làm việc riêng) và **Màn hình đen**. *(control `pause {on}` / `blank {on}`; kiểm chứng trên emulator Android và receiver desktop)*
- [x] Nhớ TV đã dùng, tự cast lại khi mở app (tuỳ chọn). *(`%APPDATA%\NANOCast\config.json`: TV cuối + mã, bitrate, fps, encoder, codec, âm thanh, ngôn ngữ, "Tự cast lại khi mở app")*
- [x] Mã 4 số tuỳ chọn trên receiver để tránh học sinh cast đè. *(Android: OK trên remote; desktop: `--pin`; GUI hỏi mã khi TXT `pin=1` hoặc TV từ chối; kiểm chứng trên emulator)*
- [x] GUI tiếng Việt / English (bảng chuỗi `crates/app/src/i18n.rs`, chọn trong Cài đặt, mặc định tiếng Việt từ Phase 5).
- [x] Installer nhỏ (Inno Setup) hoặc chỉ 1 exe portable; bản tiếng Việt / English. (`installer/`: setup 4,7 MB cài không cần admin + zip portable; CI và release tự động trong `.github/workflows/`)

### Phase 3 — Đa nền tảng (3–4 tuần)
- [x] macOS: ScreenCaptureKit + VideoToolbox + audio tap — **viết xong, CHƯA KIỂM CHỨNG trên Mac thật** (2026-10-19). GUI `crates/app` đã nối bằng `cfg(target_os = "macos")` (font Arial, dòng xin quyền Screen Recording, tắt nút Âm thanh trên macOS 12); `cargo clippy -p nanocast --target aarch64-apple-darwin/x86_64-apple-darwin` sạch. Trên macOS: tạm dừng/màn đen/mã 4 số/bitrate thích nghi (chỉ bitrate, fps giữ nguyên) có; HEVC chưa (tự gửi H.264). `crates/mac-capture` (`nanocast-mac`, bindings `objc2-*` 0.3): SCStream NV12 scale sẵn trên GPU (màn hình hoặc một cửa sổ, 1/fps, queueDepth 3, có con trỏ) → VTCompressionSession H.264 (RealTime, không B-frame, low-latency rate control nếu máy hỗ trợ, ép IDR theo `keyframe_request`) → AVCC→Annex B + SPS/PPS trước mỗi keyframe → `PacedSender`; âm thanh SCK `capturesAudio` (macOS 13+) → Opus → UDP 48324. API giống `nanocast-win` (`session::CastSession`, `sources`, `capture::Source`). Đã: `cargo check` cho `x86_64-apple-darwin` và `aarch64-apple-darwin` sạch (kể cả clippy) từ Windows, test AVCC→Annex B chạy trên Windows. Chưa: chạy trên Mac, nối GUI `crates/app` (cần sửa `cfg`), đóng gói `.app` universal + ký. Hướng dẫn build/quyền/các lỗi có thể gặp: `docs/macos.md`.
- [x] Đóng gói macOS: `installer/macos/build-mac.sh` (build `nanocast` cho `aarch64-apple-darwin` + `x86_64-apple-darwin`, `lipo` universal, `NANOCast.app` với Info.plist `com.redharegames.nanocast` / macOS 12.3+ / icon `.icns` từ `icongen` 16..1024, `codesign` ad-hoc, `NANOCast-<version>-macos-universal.dmg` có lối tắt Applications, `--per-arch` cho bản `-arm64` / `-x86_64`); CI job `macos` build hai target, `release.yml` đưa `.dmg` lên GitHub Release; hướng dẫn mở lần đầu (chuột phải > Open / `xattr`) và quyền Screen Recording ở `docs/macos.md` mục 3a–3b, README *Cài trên Mac* (2026-10-20). **CHƯA CHẠY trên Mac thật**: chỉ `cargo check --release -p nanocast` hai target trên Windows và `bash -n`.
- [x] Linux: PipeWire capture + VA-API encode — **viết xong, CHƯA KIỂM CHỨNG trên máy Linux thật** (2026-10-19). `crates/linux-capture` (`nanocast-linux`): xdg-desktop-portal ScreenCast (`ashpd`, hộp thoại hệ thống chọn màn hình/cửa sổ, nhớ lựa chọn màn hình bằng restore token) → PipeWire (`pipewire` 0.10, BGRx/BGRA/RGBx/RGBA/NV12, memfd; DMA-BUF tuyến tính thì mmap), hoặc X11 (`x11rb`: MIT-SHM, cửa sổ qua Composite, con trỏ XFixes) cho phiên X11; nhịp fps + NV12 BT.709 trên CPU vào khung cố định → H.264 qua libavcodec (`ffmpeg-sys-next`: `h264_vaapi` > `h264_nvenc` > `h264_v4l2m2m` (Pi 4, 1080p30) > `libx264`) hoặc OpenH264 build từ source; encoder phần mềm 720p30; không B-frame, CBR, ép IDR, SPS/PPS trước mọi IDR (`annexb::ParamSets`) → `PacedSender`; âm thanh PipeWire `stream.capture.sink` (fallback PulseAudio `@DEFAULT_MONITOR@`) → Opus → UDP 48324. Chọn FFmpeg thay vì `libva`/`cros-codecs` vì một API phủ VA-API + NVENC + V4L2 M2M (Pi) + phần mềm (lý do: `docs/linux.md` mục 1). API giống `nanocast-win` (cả pin, adaptive bitrate, pause/blank; HEVC lùi về H.264 như macOS). Đã: `cargo build`/`test` trên Windows (test `convert`, `annexb`), `cargo clippy --target x86_64-unknown-linux-gnu --no-default-features --features portal` sạch; trong container Docker: Debian 12 (PipeWire 0.3.65, FFmpeg 5.1, như Pi OS) `cargo check` mọi feature + 13 test, Debian 13 `clippy -D warnings` sạch; chạy thật trên Xvfb: X11 màn hình và cửa sổ (Composite) → libx264 / OpenH264 → UDP → ghép lại, ffmpeg giải mã 150/150 khung không lỗi; PipeWire audio kết nối + gói im lặng đúng nhịp (chưa có tiếng thật vì WirePlumber không chạy trong container). Chưa: chạy với màn hình/GPU/TV thật (GNOME/KDE/wlroots, X11, Intel/AMD/NVIDIA, Pi 4/5), nối GUI `crates/app` (hướng dẫn `docs/linux.md` mục 7), đóng gói `.deb`. Hướng dẫn gói/quyền/Pi/lỗi: `docs/linux.md`.
- [x] HEVC cho TV mới, giữ H.264 làm mặc định *(2026-10-19: Windows Media Foundation `MFVideoFormat_HEVC` thử lần lượt HW MFT (máy dev: NVIDIA lỗi → Intel H265 MFT, 1080p60 encode p50 13,6 ms); Android `video/hevc` khi `caps.codecs` có hevc (emulator: `c2.goldfish.hevc.decoder` 30 fps, decode ~16 ms); receiver desktop không có HEVC → sender tự chọn H.264; chọn trong Cài đặt. Chưa: TV thật, so sánh chất lượng ở cùng bitrate)*
- [ ] AV1 (TV/box mới có decoder AV1).
- [ ] Receiver cho LG webOS / Samsung Tizen qua transport WebRTC (`webrtc-rs`) — tuỳ chọn, chỉ khi có nhu cầu thật; cách rẻ hơn là box Android TV ~500k.

### Phase 4 — Chromecast có sẵn trong TV (không cài gì trên TV) — thêm 2026-10-20
Tình huống khẩn cấp (HDMI hỏng): TV nào có Chromecast built-in / Google TV cũng nhận được ngay.
- [x] Crate `nanocast-cast`: tìm TV qua mDNS `_googlecast._tcp`, kênh Cast v2 (TLS 8009, protobuf), launch receiver. **Xong 2026-10-20**: `discovery` (TXT fn/md/id), `proto` (CastMessage viết tay cho prost, không cần protoc), `channel` (rustls+ring bỏ kiểm cert tự ký, luồng I/O riêng, PING/PONG 5 s, `request` theo requestId, LAUNCH/STOP); thử trên Google Nest Mini thật (TLS 1.2, GET_STATUS, LAUNCH, LOAD). API `lib.rs` giữ nguyên; mặc định đổi thành 1080p60 12 Mbps (mượt hơn cast của Chrome). Chi tiết: `docs/google-cast.md`.
- [x] Đường A (chắc chắn chạy): Default Media Receiver phát luồng fMP4 H.264 + AAC live do PC phục vụ qua HTTP (trễ ~1–3 s). **Xong 2026-10-20**: muxer fMP4 tự viết (`fmp4.rs`), HTTP chunked cổng 47900 (`http.rs`), AAC qua Media Foundation (`aac.rs`), keyframe ≥ 1 s; Chrome trên PC phát 1080p60 + AAC, trễ bộ đệm 2,28 s ổn định, 0 khung rớt. Chưa thử trên TV thật (không có TV Cast bật); TV cần vào được cổng 47900 (Wi-Fi Private + cho phép qua Tường lửa Windows).
- [x] Đường B (trễ thấp, khó): Cast Streaming / mirroring như Chrome (OFFER/ANSWER, RTP + SRTP), trễ ~0,1–0,4 s. **Viết xong 2026-10-20, CHƯA KIỂM CHỨNG trên TV thật**: `mirror.rs` (OFFER H.264 1080p60 + Opus, AES-128-CTR theo khung, gói RTP kiểu Cast, Sender Report, NACK → gửi lại, PLI → keyframe, chia gói theo tốc độ); là đường mặc định, tự lùi về đường A nếu TV không mở app 0F5096E8 / không ANSWER / không gửi RTCP trong 5 s (đã thấy lùi đúng trên Nest Mini). Test loopback UDP qua.
- [x] GUI: TV Chromecast hiện cùng danh sách, gắn nhãn "Không cần cài, hơi trễ"; chọn là cast. *(chưa thử trên Chromecast thật)*

### Phase 5 — UI cho người không rành kỹ thuật — thêm 2026-10-20
- [x] Luồng 3 bước lớn: Chọn TV → Cast gì → Đang cast (nút Dừng to), chữ to, icon rõ, không thuật ngữ (2026-10-20). `crates/app` viết lại: `gui.rs` (3 màn hình + chỉ báo bước, thẻ TV có nhãn *Nhanh nhất · cần app NANOCast* (kèm nút tải đúng APK receiver của phiên bản từ GitHub Releases) cho app NANOCast / *Không cần cài, hơi trễ* cho Chromecast từ `nanocast_cast::discover` chạy nền mỗi 3 s, lọc loa Google Home/Nest; hai ô lớn Toàn màn hình / Một ứng dụng với danh sách cửa sổ tên dễ hiểu, lọc cửa sổ hệ thống; công tắc Âm thanh; trạng thái bằng lời Tốt / Mạng yếu / Mất kết nối, số liệu trong *Chi tiết*), `widgets.rs` (icon vẽ bằng painter, nút/thẻ/công tắc lớn, tranh minh hoạ), `chromecast.rs` (discover/start/stop không chặn UI; chọn thẻ Chromecast → `CastSession::start` với `SourceSpec`, lỗi "chưa hỗ trợ" thành thông báo thân thiện, `CastStats` trong Chi tiết). Đã cast thật tới `nanocast-receiver --windowed` cùng máy qua luồng mới (có/không mã 4 số, âm thanh, tạm dừng, màn hình đen); ảnh chụp `docs/screenshots/`. Chưa thử cast Chromecast tới TV thật.
- [x] Thông báo lỗi bằng tiếng người kèm cách sửa (không thấy TV sau 8 s → 3 bước kiểm tra + ô IP; IP sai; TV cần mã → ô 4 số lớn, mã sai; mất kết nối → tự thử lại sau 10 s có đếm ngược + *Thử lại ngay*, 6 lần thì dừng và ghi cách sửa; lỗi bộ mã hoá / chụp hình; Chromecast chưa hỗ trợ). Mã lỗi kỹ thuật chỉ trong *Chi tiết*. Phát hiện TV tắt: kênh điều khiển rớt hoặc không mở được trong 3 s (receiver nào cũng có kênh này) → khởi động lại phiên.
- [x] Hướng dẫn lần đầu (3 trang có hình vẽ, bỏ qua được, xem lại bằng nút ?), Cài đặt một trang (ngôn ngữ, chất lượng Tự động / Nét / Mượt thay bitrate, tự cast lại, Nâng cao: codec, bộ mã hoá, bitrate, fps, xử lý hình), khay hệ thống Windows (`tray-icon`: đóng cửa sổ khi đang cast = ẩn xuống khay, menu Mở / Tạm dừng / Dừng / Thoát, bấm đúp để mở), phím tắt toàn cục Ctrl+Shift+P tạm dừng (`global-hotkey`, chạy cả khi cửa sổ ẩn), font Be Vietnam Pro nhúng (OFL, `assets/fonts/`), mặc định tiếng Việt (English chọn trong Cài đặt), sáng/tối theo hệ thống, cửa sổ 560×540 co giãn được. macOS/Linux: chưa có khay/phím tắt (đóng khi đang cast chỉ thu nhỏ), chưa build thử GUI mới trên hai hệ này.

- [x] Cài app TV qua Wi-Fi từ app máy tính, không cần USB (2026-10-21): nút **Cài app lên TV** trên thẻ Chromecast mở hộp thoại (3 bước bật Gỡ lỗi USB → Bắt đầu cài → từng bước có dấu tích: Tải app / Kết nối / Cho phép trên TV / Cài / Mở app / Tắt gỡ lỗi, lỗi bằng lời + Thử lại). `adb.rs`: ADB client tự viết nói thẳng với `adbd` cổng 5555 (CNXN/AUTH/OPEN/WRTE/OKAY/CLSE, khoá RSA 2048 `%APPDATA%\NANOCast\adbkey.pem` ký token kiểu SHA-1 DigestInfo, gửi khoá công khai định dạng `adbkey.pub` khi TV chưa biết → TV hỏi *Cho phép*, chờ 90 s; sync SEND/DATA/DONE để đẩy file; nhận `STLS` = TV chỉ có Wireless debugging). `tv_install.rs`: APK từ `NANOCAST_APK` / cạnh exe / cache / tải GitHub Releases (`ureq`, rustls), `pm install -r` (khác chữ ký → `pm uninstall` rồi cài lại; TV có bản mới hơn coi như xong), `am start`, cuối cùng luôn `settings put global adb_wifi_enabled 0; adb_enabled 0`. Kiểm chứng: emulator API 35 qua cổng 5555 — tải APK v0.1.0 từ Releases, cài, mở app, tắt ADB (~2 s, emulator `offline` sau đó, cả sau khi khởi động lại); khoá công khai khớp từng byte `adbkey.pub` do `adb` tạo; adbd giả trong test cho: ký → khoá → Cho phép, không ai bấm (hết giờ), STLS, cổng đóng, đẩy file 150 KB qua gói 4 KB. Chưa thử trên TV thật (`ro.adb.secure=1`, hộp thoại Cho phép thật), chưa xem tận mắt hộp thoại cài trong GUI (chỉ thấy nút trên thẻ).
- [x] Ghép mã *Wireless debugging* (Android 11+, TV không có cổng 5555) (2026-10-21): `adb_wifi.rs` — ghép nối như AOSP `pairing_connection` + `pairing_auth`: TLS 1.3 (rustls/ring, hai phía đều trình chứng chỉ, chấp nhận mọi chứng chỉ server nhưng vẫn kiểm chữ ký bắt tay), mật khẩu = mã 6 số + 64 byte `export_keying_material("adb-label\0")`, SPAKE2 kiểu BoringSSL `spake25519` viết lại trên `curve25519-dalek` (crate `spake2` là biến thể khác, không dùng được: khác điểm M/N, khác cách băm transcript; điểm M/N tái tạo từ seed trong test, mask nhân hệ số cofactor như "password scalar hack" của BoringSSL), khoá AES-128-GCM qua HKDF-SHA256, trao `PeerInfo` 8192 byte (dòng `adbkey.pub` của ta / GUID của TV); chứng chỉ X.509 v3 tự ký dựng tay bằng DER từ khoá RSA sẵn có. `adb.rs`: nhận `STLS` → trả `STLS` → TLS trên cùng socket → `CNXN` trong TLS, `Device::shell`/`push` không đổi; TV chưa biết khoá (cắt sau bắt tay) → `NeedsPairing`. mDNS tìm cổng ghép nối / cổng ADB-TLS theo IP của TV. `tv_install.rs`: có mã → ghép nối trước; rồi thử 5555, rồi cổng `_adb-tls-connect`; lỗi mới *Mã sai*, *Không thấy bảng ghép nối*, *Đã ghép nhưng chưa thấy cổng* (ô nhập địa chỉ thủ công). GUI: ô mã 6 số lớn + *Nhập địa chỉ thủ công*, nút *Ghép nối và cài*. Kiểm chứng: `adb pair` thật (platform-tools 37) ghép nối thành công với server ghép nối giả của ta (→ SPAKE2/EKM/AES-GCM khớp BoringSSL); emulator API 35 (AVD mới `NANOCast-wifi-test`, Gỡ lỗi không dây bật qua UI): ghép nối bằng mã thật → TV liệt kê `nanocast@test` trong *Thiết bị đã ghép nối*, mã sai → `WrongCode` (bảng mã vẫn mở, nhập mã đúng sau đó vẫn được), ADB qua TLS → banner + `shell` chạy; test adbd giả: TLS đã ghép / chưa ghép (bị từ chối → `NeedsPairing`) / STLS hỏng, ghép rồi kết nối, mã sai, luồng `tv_install::connect` đầy đủ. Chưa thử: TV thật (`ro.adb.secure=1` — emulator là 0 nên chấp nhận mọi khoá qua TLS, nhánh từ chối chỉ có trong test giả), mDNS thật trên LAN (emulator nằm sau NAT, đã dùng `adb forward`), chưa xem tận mắt ô nhập mã trong GUI.
- [x] Cài lên TV không có Chromecast theo IP (2026-10-21, học từ OwnTV `tools/chung.ps1`): link **Cài app lên TV qua Wi-Fi (gõ địa chỉ IP)** cạnh *Tải app NANOCast cho TV* (dưới danh sách, trong *TV của tôi không hiện?*, khi không thấy TV nào, trang 1 hướng dẫn) mở cùng hộp thoại với ô IP; `tv_install::parse_tv_ip` nhận kiểu gõ dễ dãi (dấu phẩy, khoảng trắng, `:cổng`, `http://`, số 0 đứng đầu), IP lưu `install_ip` trong config; lỗi không kết nối / cần ghép mã thì sửa IP ngay trong hộp thoại; TV không hiện hộp Cho phép → hướng dẫn *Thu hồi uỷ quyền gỡ lỗi USB* rồi bật lại. Không lấy của OwnTV: Python + `adb_shell`, tải `adb.exe` để ghép mã rồi `adb tcpip 5555` (NANOCast tự ghép mã và tắt ADB khi xong), đẩy file bằng base64 qua shell (sync của mình chạy được). Chưa xem tận mắt hộp thoại, chưa thử TV thật.

## 6. Tech stack (Rust)

| Thành phần | Crate / API | Ghi chú |
|---|---|---|
| GUI | `eframe`/`egui` | Nhẹ (~3 MB), thuần Rust, không webview |
| Capture Windows | `windows-capture` hoặc `windows` (WGC) | Màn hình + cửa sổ, texture GPU |
| Encode Windows | `windows` (Media Foundation) | HW của mọi hãng, không cần SDK NVIDIA/AMD |
| Fallback encode | `openh264` | Máy không có HW encoder |
| Audio | `cpal` (WASAPI loopback) + `unsafe-libopus` | Opus 48 kHz stereo; Rust thuần nên không cần CMake (audiopus/opus-sys đều cần) |
| Discovery | `mdns-sd` | Không cần daemon |
| Cài app TV qua Wi-Fi | ADB client tự viết + `rsa`, `ureq` | Không cần `adb.exe`; cổng 5555 |
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
│   ├── app/                # nanocast.exe: GUI egui (sender); build.rs nhúng icon
│   ├── win-capture/        # nanocast-win: WGC → NV12 → MF H.264 → UDP + control (CastSession)
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
5. ~~Phase 1 GUI: `nanocast.exe` dùng được~~ **Xong 2026-10-19**:
   - `crates/win-capture` (`nanocast-win`): capture WGC + pacing fps, NV12 (CPU) vào khung cố định (màn hình: kích thước thật, tối đa 1080p; cửa sổ: 1920×1080, co giãn giữ tỉ lệ + viền đen nên đổi cỡ cửa sổ không cần stream mới), `MfEncoder` (thử lần lượt HW encoder theo merit, bỏ qua cái lỗi kể cả khi lỗi ở khung đầu; Shutdown MFT khi giải phóng), `ControlLink`, `CastSession` (luồng riêng, dừng sạch theo thứ tự capture → mạng → encoder → `stop`). `spike-win` dùng lại crate này.
   - `crates/app`: GUI eframe/egui (glow), tiếng Việt (font Segoe UI của Windows), danh sách TV mDNS liên tục (`discovery::Browser`) + Quét lại + nhập IP tay, Cast màn hình (chọn màn hình), Cast ứng dụng… (lọc cửa sổ), Dừng, trạng thái PC/TV/keyframe_request, Cài đặt encoder/bitrate/fps. Release: subsystem GUI (không console), 7,1 MB, không cần admin.
   - Kiểm chứng trên máy dev: NVIDIA MFT lỗi → tự dùng QSV; cast màn hình và cửa sổ tới receiver giả (Python, 127.0.0.1) 60 fps, 20–30 Mbps, encode ~12 ms; `hello/start/stop`, `stats`, `keyframe_request` hiện trên GUI; Dừng và đóng cửa sổ đều gửi `stop` và thoát sạch; TV giả quảng bá mDNS hiện trong danh sách < 2,5 s.
   - Chưa: TV thật trên LAN thật, chọn IP đúng subnet khi receiver có nhiều IP (hiện lấy IPv4 nhỏ nhất), lưu cài đặt, GPU color convert/zero-copy, scale chất lượng cao (đang nearest-neighbour).
   Tiếp theo: thử với TV/box Android thật + đo glass-to-glass.
6. ~~Phase 1b: âm thanh hệ thống PC → TV, bật/tắt trong lúc cast~~ **Xong 2026-10-19** (giao thức §4 *Audio*):
   - `nanocast_core::audio`: `OpusEncoder`/`OpusDecoder` (unsafe-libopus), `to_stereo` (mono/5.1/7.1 → stereo), `Resampler` (tuyến tính, khi thiết bị không phải 48 kHz), `AudioEncoder` (gom PCM thành frame 20 ms, khung im lặng → gói rỗng).
   - `nanocast_win::audio::AudioSender`: WASAPI loopback của thiết bị output mặc định (định dạng mix của thiết bị, f32/i16/i32/u16) trên luồng riêng; mở thiết bị ngay trong luồng đó nên bật/tắt không chặn vòng video. `CastSession::set_audio(on)` + `CastConfig.audio`; `start.audio` mang trạng thái ban đầu, mỗi lần đổi gửi `audio {on}`. Tắt = thả stream cpal + luồng (không tốn CPU).
   - GUI: checkbox **🔊 Âm thanh** (mặc định tắt, đổi được khi đang cast), dòng `Âm thanh  N kbps Opus` / `(im lặng)` / lỗi thiết bị.
   - Receiver: `AudioPlayer.kt` (UDP 48324 → MediaCodec `audio/opus` với csd-0 OpusHead, csd-1/csd-2 → AudioTrack 16-bit, buffer dùng 80 ms, bắt đầu phát khi có 40 ms), xử lý `audio`/`start.audio`/`stop`, overlay dòng audio (kbps, buffer, trễ đầu ra, mất, bỏ, underrun).
   - Đo (`docs/benchmark.md` *Phase 1b*): luồng encoder audio ~1,2–1,5 % một nhân; 12 lần bật/tắt trong 40 s, video giữ 59–61 fps; emulator nghe được nhạc, trễ đầu ra (track + mixer + HAL) ~100–125 ms.
   - Chưa: TV thật (FAST track, trễ thật), đo lệch hình/tiếng (§7 ≤ 40 ms), chọn thiết bị output khác mặc định, đổi thiết bị mặc định giữa chừng (stream vẫn bám thiết bị cũ), PLC/FEC khi mất gói.
   Tiếp theo: đo trên TV thật, cân chỉnh buffer theo `AudioTrack` FAST/HAL period của TV.
7. ~~Phase 2 (phần còn lại) + HEVC~~ **Xong 2026-10-19** (giao thức §4 *Mã 4 số*, *Codec*, *Bitrate thích nghi*; số đo `docs/benchmark.md` *Phase 2: bitrate thích nghi, tạm dừng, mã 4 số, HEVC*):
   - Core: `pause`/`blank`/`error` + `hello.pin` (`control.rs`), `VideoCodec` + `negotiate` (`protocol.rs`), `adapt.rs` (logic thuần, có test), `control_link.rs` (ControlLink chung cho Windows/macOS), TXT `pin`, AU HEVC (`au.rs`).
   - Sender Windows: encoder HEVC qua Media Foundation, đổi bitrate lúc chạy + kiểm tra lại (QSV bỏ qua lệnh → mở lại encoder), đổi fps lúc chạy (capture pacing dùng chung), tạm dừng/màn đen dừng hẳn convert + encode + tiếng, `EndReason::PinRejected`, `CodecFallback`.
   - GUI: `config.rs` (`%APPDATA%\NANOCast\config.json`), `i18n.rs` (VI/EN), nút Tạm dừng / Màn hình đen, hộp nhập mã, dòng bitrate + lý do, Tự cast lại khi mở app, nối macOS bằng `cfg`.
   - Receiver: Android (OK bật/tắt mã, `⏸` + giữ khung cuối, view đen, HEVC, lọc UDP theo IP), desktop (`--pin`/`--pin-code`, `‖` + giữ khung, tô đen, chỉ H.264, lọc UDP).
   - Chưa: TV thật, Wi-Fi thật cho bitrate thích nghi, bấm thử GUI bằng tay (hộp nhập mã, tự cast lại), HEVC trên macOS, so sánh chất lượng HEVC/H.264.
