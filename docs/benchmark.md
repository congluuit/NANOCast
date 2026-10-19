# Benchmark

Số đo thực tế của pipeline sender. Cập nhật mỗi khi đổi capture/encoder/mạng.

## 2026-10-19 — Spike 1: WGC → NV12 (CPU) → Media Foundation H.264 HW → file

Máy đo: Intel Core i9-13900HX, Intel UHD Graphics (iGPU) + NVIDIA RTX 4050 Laptop (Optimus),
Windows 10 Pro 19045, màn hình 1920×1080 @ 144 Hz. Lệnh: `cargo run --release -p spike-win -- --encoder 1 --seconds 8`.

Chưa có mạng và chưa có receiver: đây là chi phí ở phía PC trước khi gửi đi.

| Cấu hình | Khung/giây ra | Bitrate thật | BGRA→NV12 (CPU) | Encode (ProcessInput→ProcessOutput) | Capture→encoded |
|---|---|---|---|---|---|
| Màn hình, QSV, không pacing (144 fps vào) | 144 | 45 Mbps | 0,99 ms | avg 6,3 ms, p95 6,7 ms | avg 11,4 ms, p95 11,3 ms |
| Màn hình, QSV, pacing 60 fps, 20 Mbps | 60,4 | 17,5 Mbps | 0,83 ms | avg 12,8 ms, p50 13,2, p95 20,0 | avg 17,8 ms, p50 16,2, p95 23,2 |
| Cửa sổ Brave (1920×1040), QSV, 60 fps | 48,7* | 13,6 Mbps | 0,89 ms | avg 13,7 ms, p95 20,5 | avg 18,8 ms, p95 23,3 |

\* đo trước khi sửa pacing (lấy mỗi khung thứ 3 của 144 Hz); sau khi sửa, màn hình đạt 60,4 fps.
Giá trị max (~320 ms) chỉ xuất hiện ở khung đầu tiên khi encoder khởi tạo.

### Kết luận
- **Encoder phần cứng Intel QSV qua Media Foundation chạy được, không cần SDK**, 1080p60 ổn định, 1 khung vào → 1 khung ra (không buffer thêm).
- Encode 6–13 ms. Ở 144 fps vào encoder nhanh hơn (6 ms) so với 60 fps (13 ms): nghi iGPU hạ xung khi tải nhẹ. Cần thử `AVEncCommonQualityVsSpeed`, hoặc kiểm tra bằng NVIDIA.
- Chuyển màu trên CPU chỉ ~0,9 ms (rayon, 24 luồng); trên máy yếu sẽ cao hơn → vẫn nên chuyển sang GPU ở Phase 1.
- Tổng chi phí phía PC hiện ~16 ms (p50). Cộng mạng LAN 1–3 ms + decode TV ~5–10 ms → mục tiêu ≤ 40 ms glass-to-glass trên Ethernet là khả thi.
- File `.h264` là Annex B chuẩn (AUD, SPS, PPS, slice), TV decode trực tiếp được.

### Vấn đề gặp
| Vấn đề | Xử lý |
|---|---|
| `NVIDIA H.264 Encoder MFT` không activate được (E_UNEXPECTED), cả `ActivateObject` lẫn `CoCreateInstance` | Chưa rõ nguyên nhân; nghi Optimus (màn hình nối iGPU). Chromium cũng xử lý bằng cách thử lần lượt từng encoder. NANOCast sẽ thử lần lượt theo thứ tự merit và dùng cái đầu tiên chạy được. |
| MFT phần cứng trả `MF_E_TRANSFORM_STREAM_CHANGE` ở output đầu tiên | Set lại output type từ `GetOutputAvailableType(0,0)`, chờ event HaveOutput tiếp theo. |
| `GetEvent` chặn vô hạn khi dùng blocking | Poll `MF_EVENT_FLAG_NO_WAIT` + ngủ 200 µs, có timeout. |
| Windows 10 19045: WGC không hỗ trợ tắt viền vàng (`BorderConfigUnsupported`) và `MinimumUpdateInterval` | Dùng Default; pacing fps tự làm trong callback. Win11 có thể bật. |
| `CODECAPI_AVEncCommonRealTime` không hỗ trợ trên QSV | Bỏ qua, không ảnh hưởng. |

### Việc tiếp theo
1. Spike receiver Android TV (MediaCodec low-latency) → đo decode.
2. Nối UDP, đo glass-to-glass bằng quay 240 fps.
3. GPU color convert (D3D11 VideoProcessor) + đưa texture thẳng vào MFT qua `IMFDXGIDeviceManager`.

## 2026-10-19 — Spike 2: receiver Android TV (UDP → MediaCodec → SurfaceView)

Mã: `receiver-androidtv/` (Kotlin, không thư viện ngoài, minSdk 26) và tool gửi `crates/spike-send`.

### Giao thức đã dùng (PLAN.md §4)
- Video UDP cổng **48322**, header 14 byte **big-endian**: `magic u16 = 0x4E43 ("NC") | seq u32 | frame_id u32 | idx u16 | count u16`, payload ≤ 1200 byte. Một frame = một access unit H.264 Annex B (tối đa 1024 gói ≈ 1,2 MB). *(Ban đầu idx/count là u8 = 255 gói ≈ 300 KB; keyframe 1080p thực tế 320–330 KB nên đã nâng lên u16 cùng ngày.)*
- mDNS: `NsdManager` quảng bá `_nanocast._udp.local`, tên = tên thiết bị (`Settings.Global.DEVICE_NAME`, dự phòng `Build.MODEL`), port 48322, TXT `w=1920 h=1080 codec=h264 ver=0.1`.

### Receiver làm gì
- `UdpReceiver` (luồng ưu tiên cao) → `FrameAssembler` ráp theo `frame_id/idx/count`, giao frame theo thứ tự tăng dần ngay khi đủ gói; frame cũ thiếu gói bị bỏ khi frame mới hơn đã đủ (không chờ). Đếm gói mất theo khoảng trống `seq`.
- `VideoDecoder`: MediaCodec `video/avc` 1920×1080, async mode trên HandlerThread riêng, `csd-0/1` lấy từ SPS/PPS của keyframe đầu, `KEY_LOW_LATENCY=1` (API ≥ 30), `KEY_PRIORITY=0`, `KEY_OPERATING_RATE=60`; mỗi output `releaseOutputBuffer(index, true)` ngay lập tức. Codec được tạo trên luồng decoder để luồng mạng không bị chặn.
- Overlay góc trái: IP:port, trạng thái mDNS, tên decoder (HW/SW, có hỗ trợ `FEATURE_LowLatency` không), fps, Mbps, gói mất, frame bỏ, **decode ms** (avg/p95 trên ~120 frame gần nhất + p50/p95/p99 cả phiên).
- *Decode ms* = từ lúc nhận đủ gói của frame đến lúc decoder trả output buffer (gồm chờ input buffer + decode). Không gồm thời gian compositor/TV hiển thị.
- Mỗi 5 s ghi một dòng `NANOCast.Stats` vào logcat để thu số liệu.

### Build và cài
```bash
cd receiver-androidtv
./gradlew assembleDebug          # Windows: gradlew.bat assembleDebug
# APK: receiver-androidtv/app/build/outputs/apk/debug/app-debug.apk (~0,9 MB)
```
Cần Android SDK (platform 35) và JDK 17+ (JDK 21 hoặc JBR của Android Studio đều chạy). Nếu không mở bằng Android Studio thì tạo `receiver-androidtv/local.properties` với `sdk.dir=C\:\Users\<user>\AppData\Local\Android\Sdk`. Hoặc mở thư mục `receiver-androidtv/` bằng Android Studio → Run.

Cài lên TV thật (Android TV / Google TV / box Android):
1. TV: Settings → Device Preferences → About → bấm *Build* 7 lần → Developer options → bật **USB debugging** / **Network debugging**.
2. PC cùng mạng LAN:
```bash
adb connect <ip-tv>:5555
adb install -r receiver-androidtv/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n com.redharegames.nanocast.receiver/.MainActivity
```
   App có `LEANBACK_LAUNCHER` nên cũng hiện trên launcher của TV (tên *NANOCast*). Overlay hiện IP của TV.
3. Gửi stream từ PC (file do spike-win tạo):
```bash
cargo run --release -p spike-win -- --encoder 1 --seconds 12 --out test.h264
cargo run --release -p spike-send -- test.h264 --to <ip-tv> --loop
```
4. Thu số: `adb logcat -s NANOCast.Stats`.

`spike-send` tách file thành access unit (theo AUD, hoặc SPS/PPS/SEI/slice có `first_mb_in_slice = 0` sau một slice), gửi mỗi frame thành loạt gói liền nhau, pacing 60 fps bằng deadline (sleep + spin 1 ms cuối), `--loop` để phát lặp (frame_id/seq tăng liên tục), `--seconds N` để dừng.

### Kết quả đo — emulator (chưa có TV thật)
Máy: cùng PC ở Spike 1 (i9-13900HX), Android Emulator, image `android-35 google_apis x86_64` (điện thoại, **không phải image Android TV**) với profile `tv_1080p` 1920×1080, WHPX, `-gpu host`.
Decoder được chọn: `c2.goldfish.h264.decoder` — báo HW nhưng thực chất decode ở phía máy host qua goldfish pipe, `FEATURE_LowLatency` = không. **Số này không đại diện cho SoC của TV**, chỉ chứng minh pipeline đúng.

Luồng: `test.h264` của spike-win (700 frame, 18,4 Mbps, 2 keyframe, keyframe lớn nhất 228 KB), phát lặp 90 s.
Do mạng emulator API 35 (guest 10.0.2.16 sau router ảo) không nhận được `adb emu redir udp`, tool gửi được cross-compile cho `x86_64-linux-android` (NDK 28 linker) và chạy *bên trong* emulator gửi tới `127.0.0.1:48322` — đường mạng là loopback, không có Wi-Fi/Ethernet.

| Chỉ số (90 s, 5 250 frame) | Giá trị |
|---|---|
| fps hiển thị (output decoder) | 59–62, trung bình 60,0 |
| Bitrate nhận | 20,1 Mbps |
| Ráp frame (gói đầu → gói cuối của frame) | avg 0,96 ms |
| **Decode ms** cả phiên | **avg 10,8 · p50 8,8 · p95 25,0 · p99 43,5** |
| Decode ms theo cửa sổ 2 s | avg 7,7–19,0, p95 10,7–53,7 |
| Gói mất / frame bỏ / lỗi decoder | 24 / 1 / 0 |
| Sender restart (dừng 3 s rồi phát lại) | tự hồi phục, 602/602 frame decode |

Hình hiển thị đúng (ảnh chụp màn hình emulator khớp nội dung màn hình PC khi quay).

### Nhận xét
- Pipeline UDP → ráp → MediaCodec → Surface chạy ổn 60 fps 1080p, 20 Mbps, ráp frame < 1 ms.
- **24 gói mất** xảy ra đúng một lần (1/8 lần lặp), ở t ≈ 58 s, đúng lúc IDR đầu file (234 KB = 196 gói) được bắn liền một mạch trong ~1 ms. `net.core.rmem_max` của guest chỉ 256 KB (app xin 4 MB, nhận 512 KB tính cả overhead của kernel), nên một burst cỡ đó cộng với luồng nhận bị lỡ lịch một chút là tràn buffer. Trên TV thật cũng sẽ vậy → sender cần **rải gói của frame lớn** trong khoảng thời gian frame và dùng **intra-refresh thay vì IDR** như PLAN đã định; receiver sau này gửi `keyframe_request`.
- Lần đầu thử, tạo MediaCodec trên luồng mạng (100–300 ms) làm tràn socket buffer → đã chuyển sang luồng decoder; các frame đến trong lúc decoder đuổi kịp backlog khởi động bị loại khỏi thống kê latency.
- Khi decoder tụt quá 60 frame, receiver xả backlog và chờ IDR kế tiếp (không decode tiếp ra hình vỡ). Với GOP 10 s của spike-win, điều đó có thể là 10 s hình đứng → cần intra-refresh + `keyframe_request`.
- mDNS: `NsdManager` đăng ký thành công (`registered sdk_gphone64_x86_64 _nanocast._udp port 48322` trong logcat), nhưng **chưa kiểm chứng việc PC thấy được dịch vụ** vì mạng NAT của emulator không chuyển multicast ra LAN.

### Chưa kiểm chứng
- Chạy trên **TV/box Android thật** (decoder phần cứng thật, `KEY_LOW_LATENCY`, Wi-Fi/Ethernet thật) và trên image emulator Android TV; số decode thật cần đo lại trên TV.
- PC tìm thấy TV qua mDNS (`_nanocast._udp.local`, TXT).
- Đường PC → TV qua LAN thật bằng `spike-send` (mới chạy loopback trong emulator).

### Việc tiếp theo
1. Cài lên một TV/box Android thật, đo decode ms qua Ethernet và Wi-Fi 5 GHz.
2. ~~Sender: rải gói trong khoảng frame, intra-refresh; đưa hằng số giao thức vào `crates/core`.~~ Xong (Spike 3 bên dưới).
3. Đo glass-to-glass (quay 240 fps) với capture → encode → UDP → TV.

## 2026-10-19 — Spike 3: stream live PC → TV, giao thức vào core, icon

Mã: `crates/core` (`protocol`, `au`, `discovery`), `crates/spike-win --to/--discover`, `crates/spike-send` dùng core, `tools/icongen`, `assets/`.

- **Rải gói (PacedSender):** mỗi gói cách nhau `bytes×8/pace_bps`, mặc định 80 Mbps. Khung P ~35 KB → ~3,5 ms; keyframe 330 KB → 28 ms (chỉ khi ép keyframe). Loopback 6 s: 342/342 khung gửi, hàng đợi không đầy.
- **Keyframe theo yêu cầu:** `CODECAPI_AVEncVideoForceKeyFrame` hoạt động trên QSV (`--keyframe-every 2` → 3 keyframe/6 s). **`CODECAPI_AVEncVideoGradualIntraRefresh` không hỗ trợ** trên Intel QSV MFT → chiến lược hồi phục là keyframe theo `keyframe_request`, không phải intra-refresh.
- **Header 14 byte:** keyframe 1080p thực tế 317–331 KB > 255×1200 B → nâng idx/count lên u16 (receiver Kotlin đã sửa theo, APK build lại).
- **Discovery:** `nanocast_core::discovery::discover()` browse `_nanocast._udp.local.` bằng `mdns-sd`; `spike-win --discover` tự chọn TV nếu chỉ có một. Chưa có TV thật trên LAN để xác nhận PC thấy được.
- **Icon:** vẽ tay `assets/icon.svg` + `assets/banner.svg`; `cargo run -p icongen` render PNG/ICO/mipmap; `nanocast.exe` nhúng icon qua `winresource` (kiểm tra bằng ExtractAssociatedIcon → 32×32 OK).

Lệnh chạy thử với TV thật (cài APK trước, TV và PC cùng LAN):
```bash
cargo run --release -p spike-win -- --encoder 1 --discover --seconds 60 --keyframe-every 5
```

## 2026-10-19 — Spike 4: kênh control TCP (hello/caps/start/stop, keyframe_request, stats)

Mã: `crates/core/src/control.rs` (+ `CONTROL_PORT = 48323` trong `protocol.rs`, `ReceiverInfo.ctrl_port` đọc từ TXT `ctrl` trong `discovery.rs`), `crates/spike-win/src/ctrl.rs`, `receiver-androidtv/.../ControlServer.kt` (+ hook trong `FrameAssembler`, `VideoDecoder`, `Stats`, `Discovery`, `MainActivity`). Định dạng gói video UDP không đổi.

- **Giao thức:** TCP 48323, mỗi dòng một object JSON, loại ở trường `"t"` (serde `tag = "t"`, `snake_case`). Chi tiết trong PLAN.md §4. Loại lạ → `Message::Unknown`, trường lạ bỏ qua; dòng > 64 KB hoặc JSON hỏng → lỗi `InvalidData`, bên đọc bỏ qua dòng đó.
- **spike-win:** sau khi chọn target (`--to`/`--discover`) mở TCP (timeout 1 s) tới `ip:ctrl_port`, gửi `hello`; gửi `start` khi encoder đã tạo (kích thước thật); luồng đọc in `caps`/`stats`, `keyframe_request` bật một `AtomicBool` mà vòng encoder kiểm tra trước mỗi khung → `MfEncoder::request_keyframe()`; kết thúc gửi `stop`. TCP lỗi → chỉ cảnh báo, vẫn stream UDP.
- **Receiver Android:** `ServerSocket` 48323 trên luồng riêng, mỗi kết nối một luồng đọc, mọi lệnh ghi qua một executor (luồng mạng/decoder không bao giờ chặn vì TCP). `keyframe_request` khi: có lỗ trong `seq` (`packet_loss`), decoder chưa có SPS/PPS — vào giữa GOP hoặc sau lỗi codec (`decoder_start`), sau khi xả backlog và đang chờ IDR (`decoder_backlog`); giới hạn 1 lần / 500 ms. `stats` mỗi 1 s: fps decode ra, số gói mất trong giây đó, decode ms trung bình cửa sổ (0 nếu giây đó không có khung). `caps.low_latency` = decoder mà MediaCodec chọn có `FEATURE_LowLatency`. Overlay thêm dòng `ctrl: …  kf req N`.

### Đã kiểm chứng
| Kiểm tra | Kết quả |
|---|---|
| `cargo test` | 8/8 (5 test mới của `control`: roundtrip mọi loại, định dạng dây, đọc khoan dung (CRLF, thứ tự khoá khác, số nguyên cho float, loại lạ, JSON hỏng), dòng quá dài, qua TCP loopback) |
| `cargo build --release`, `gradlew assembleDebug` | sạch (2 cảnh báo cũ của spike-win không liên quan) |
| `spike-win --to 127.0.0.1`, không có receiver | `warning: control channel … unavailable (connection timed out); streaming UDP only`, UDP gửi 160/160 khung. Trên Windows connect tới cổng đóng trên loopback không bị từ chối ngay mà hết timeout 1 s |
| spike-win ↔ receiver giả (Python) | nhận `hello`, `start {1920,1080,60,h264,false}`, `stop`; 2 `keyframe_request` → 3 keyframe (1 đầu + 2 ép) |
| spike-win ↔ app thật trên emulator API 35 (`adb forward tcp:48323`), video do `spike-send` chạy trong emulator phát một file bắt đầu giữa GOP | `caps` nhận đúng (`low_latency false` với `c2.goldfish`); 2 × `decoder_start` cách nhau 507 ms (giới hạn 500 ms hoạt động); có mất gói (buffer guest 256 KB) → `packet_loss` cách nhau ≥ 800 ms; QSV ép IDR theo từng yêu cầu (5 keyframe / 9 s); `stats` mỗi giây (vd. 59,8 fps, decode 9,9 ms); `stop` + đóng kết nối được log ở receiver |
| Receiver bị force-stop giữa chừng | sender in `receiver closed the control connection`, UDP tiếp tục tới hết |

### Chưa kiểm chứng
- TV/box Android thật và LAN thật (TCP PC → TV, PC đọc được TXT `ctrl` qua mDNS).
- Hồi phục hình thật sau mất gói: emulator chỉ cho thấy request đi và IDR được ép; trong thử nghiệm video do `spike-send` (file) phát chứ không phải spike-win, nên IDR ép chưa tới decoder. Cần đo trên TV: thời gian từ lỗ `seq` → IDR hiện hình (kỳ vọng ≈ RTT + 1 khung encode + ~28 ms rải gói keyframe).
- `audio {on}`: receiver chỉ ghi log (chưa có đường âm thanh).
- Sender chưa dùng `stats` (adaptive bitrate là Phase 2); receiver chưa từ chối `start` vượt `max_w/max_h`.
