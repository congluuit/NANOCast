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

## 2026-10-19 — Phase 1 GUI (`nanocast.exe`)

Cùng máy dev. Receiver giả (Python: TCP 48323 trả `caps`, gửi `stats` mỗi giây và một `keyframe_request`; UDP 48322 đếm gói) trên 127.0.0.1, nhập IP tay trong GUI.

| Kiểm tra | Kết quả |
|---|---|
| Chọn encoder *Tự động* | NVIDIA MFT lỗi `CoCreateInstance` (0x8000FFFF) → tự chuyển sang Intel QSV |
| Cast màn hình 1920×1080, 60 fps, 30 Mbps | GUI: 60 fps, 30,0 Mbps, encode 11,8 ms |
| Cast cửa sổ (scale vào 1920×1080, viền đen) | 59–60 fps, 29,7–30,2 Mbps, encode 12–13 ms |
| `keyframe_request` từ receiver | đếm trên GUI, QSV ép IDR |
| Dừng / đóng cửa sổ | receiver nhận `stop`, kết nối đóng, process thoát |
| TV giả quảng bá mDNS (`--example advertise`) | hiện trong danh sách < 2,5 s, tự chọn |
| `nanocast.exe` release | 7,1 MB, subsystem GUI |

Chưa kiểm chứng trên TV thật.

## 2026-10-19 — Đóng gói (Phase 2)

Mã: `installer/nanocast.iss` (Inno Setup 6.7.3, cài qua `winget install JRSoftware.InnoSetup --scope user` → `%LOCALAPPDATA%\Programs\Inno Setup 6\ISCC.exe`), `installer/Languages/Vietnamese.isl` (bản dịch không chính thức trong repo `jrsoftware/issrc`, `Files/Languages/Unofficial`; thêm BOM UTF-8), `installer/make-portable.ps1`, `.github/workflows/ci.yml`, `.github/workflows/release.yml`.

| File (v0.1.0) | Kích thước |
|---|---|
| `nanocast.exe` (release, LTO thin, strip) | 7 671 808 B (7,3 MiB) |
| `NANOCast-Setup-0.1.0.exe` (LZMA2 ultra64, solid) | 4 706 812 B (4,5 MiB) |
| `NANOCast-0.1.0-portable-win64.zip` (exe + README.txt) | 3 555 437 B (3,4 MiB) |

| Kiểm tra | Kết quả |
|---|---|
| ISCC, version đọc từ `Cargo.toml` bằng ISPP (hoặc `/DAppVersion=`) | biên dịch 2,3 s, không cảnh báo |
| Cài im lặng `/VERYSILENT /CURRENTUSER /TASKS=desktopicon` | exit 0, không hỏi UAC; `%LOCALAPPDATA%\Programs\NANOCast\nanocast.exe`, shortcut Start Menu + Desktop, mục gỡ trong HKCU (NANOCast 0.1.0) |
| Chạy exe đã cài | mở cửa sổ "NANOCast" |
| Gỡ `unins000.exe /VERYSILENT` | exit 0; thư mục, 2 shortcut, khoá HKCU đều xoá sạch |
| Zip portable giải nén | `nanocast.exe` + `README.txt` |
| `actionlint` 1.7.12 trên `ci.yml`, `release.yml` | 0 lỗi |

Ghi chú:
- Cài cho riêng người dùng không thêm luật Windows Firewall (cần admin); lần đầu cast Windows có thể hỏi, chọn mạng Private.
- `cargo fmt --check` hiện còn khác biệt ở hầu hết crate (code chưa từng chạy rustfmt) nên bước fmt trong CI đang `continue-on-error`; chạy `cargo fmt --all` một lần rồi bỏ dòng đó.
- Chưa chạy trên GitHub Actions thật (repo chưa push); job macOS/Linux đặt `continue-on-error`.
- Trong Git Bash phải đặt `MSYS_NO_PATHCONV=1` khi gọi `ISCC "/DAppVersion=…"`, nếu không MSYS đổi `/D…` thành đường dẫn.

## 2026-10-19 — Phase 1b: âm thanh (WASAPI loopback → Opus → UDP 48324 → MediaCodec → AudioTrack)

Máy dev: i9-13900HX (32 luồng), Windows 10 19045, thiết bị phát "Speakers (Realtek High Definition Audio)" 48000 Hz 2 ch f32 (định dạng mix → không cần resample). Opus 96 kbps VBR, application Audio, complexity 3, frame 20 ms. Mã và giao thức: PLAN.md §4 *Audio*, §10 mục 6.

Công cụ:
- `cargo run --release -p nanocast-win --example audio_probe -- 20 [--wav out.wav] [--dump out.nca]`: loopback → Opus → UDP 127.0.0.1:48324, nhận + decode lại trong cùng process, in kbps / gói im lặng / mất / mức đỉnh mỗi giây; `--dump` ghi gói để phát lại.
- `cargo run --release -p nanocast-win --example cast_toggle -- --window "<tiêu đề>" --seconds 40 --every 3`: cast một cửa sổ và bật/tắt âm thanh mỗi N giây, in số liệu `CastStats`.
- `spike-send --audio-dump file.nca [--loop] [--to host]`: phát lại gói audio (đánh số lại seq/pts) tới UDP 48324; build `x86_64-linux-android` để chạy trong emulator (UDP từ host không vào được emulator, xem Spike 2).

### Sender (PC)

| Đo | Kết quả |
|---|---|
| Không có gì phát | 50 gói/s, toàn gói rỗng (header 10 B) → **4,0 kbps** |
| Có nhạc (Alarm01.wav lặp / video) | 50 gói/s, **~100 kbps** trên dây (96 kbps Opus + header + UDP payload), 0 mất trên loopback |
| CPU luồng encoder audio (`GetThreadTimes`, log khi dừng) | **1,15–1,49 % một nhân** (15–20 s có tiếng) = ~0,05 % tổng CPU máy 32 luồng; luồng callback cpal chỉ copy/đổi định dạng |
| CPU cả process `audio_probe` (capture + encode + nhận + decode) | 1,9 % một nhân |
| Audio tắt | không có luồng `audio`, không mở WASAPI → 0 CPU cho âm thanh |
| Trước khi phát tiếng đầu tiên | WASAPI báo một lần "buffer underrun or overrun" (cờ discontinuity khi loopback bắt đầu), vô hại |

Ước lượng cho laptop i3 đời 2015 (§7): nhân chậm hơn ~2,5–3 lần và libopus bản Rust không có SIMD → ~4–5 % một nhân ≈ 1–2 % tổng CPU; chưa đo trên máy đó.

CPU của cả process cast (`cast_toggle`, cửa sổ 960×540 hoạt hình → khung 1920×1080 60 fps) dao động 140–185 % một nhân, chủ yếu do đổi màu NV12 trên CPU; chênh lệch bật/tắt âm thanh (~1,5 %) nhỏ hơn độ dao động nên không đọc được ở mức process.

### Bật/tắt trong lúc cast không làm giật hình

`cast_toggle` cast cửa sổ hoạt hình 60 fps tới receiver giả Python (127.0.0.1: trả `caps`, đo khoảng cách giữa các khung video, đếm gói audio), nhạc phát liên tục:

| Lần chạy | Kết quả |
|---|---|
| 40 s, bật/tắt mỗi 3 s (12 lần) | PC: 59–61 fps ở mọi mẫu 0,5 s, encode 11–14 ms, không thêm khung bị bỏ (2 khung bỏ lúc khởi động, trước lần bật đầu tiên). Receiver: 60 fps, khoảng cách khung p50 14,9 / p99 27,2 / max 35,6 ms (đồng hồ hoạt hình Tk ~14 ms nên vốn có gap 25–30 ms mỗi giây). `audio {on}` tới receiver ~0,1 s sau mỗi lần đổi; audio có tiếng sau ≤ 0,5 s, tắt hẳn sau ≤ 0,5 s. |
| 31 s, bật/tắt mỗi 10 s | Lúc bật/tắt fps không đổi; có **một** lần tụt còn 33–45 fps kéo dài ~2 s, bắt đầu 2 s *sau* khi bật (encode ms và số khung bỏ không đổi → khung capture về ít hơn). Không lặp lại ở 12 lần bật/tắt của lần chạy trên; lúc đó máy đang chạy song song build/test của session khác nên nghi do tải máy, chưa kết luận chắc. |

Lưu ý khi thử: trong khoảng 11–22 s của lần chạy 40 s có một chương trình khác (session khác) gửi video vào cùng cổng 48322 của 127.0.0.1, receiver giả đếm thành "120 fps"; số liệu PC không bị ảnh hưởng.

### Receiver (emulator API 35, `c2.android.opus.decoder`)

Phát lại 20 s nhạc đã ghi (`audio_probe --dump`) bằng `spike-send --audio-dump --loop` bên trong emulator; có lúc chạy song song video 1080p60 (`test.h264`): video vẫn 59–62 fps.

| Cấu hình AudioTrack | Kết quả |
|---|---|
| Buffer 6720 frame, không đặt start threshold | **Không phát**: AudioFlinger chỉ bắt đầu một track streaming khi buffer *đầy*, mà receiver chặn ở 120 ms (< 140 ms) → kẹt ở trạng thái filling. Sửa: `setBufferSizeInFrames` = buffer dùng được và (API 31+) `setStartThresholdInFrames(40 ms)`. |
| Buffer dùng 60 ms, start 40 ms | phát được; ~15 s: 27 gói bỏ (đầy) + 9 underrun → nghe lụp bụp ~1 lần/s. HAL emulator chạy chu kỳ 1088 frame (22,7 ms) so với gói 20 ms. |
| **Buffer dùng 80 ms, start 40 ms** (chọn) | 30 s: 3–9 gói bỏ, 3–4 underrun (2 lúc khởi động); queue trong track trung bình 16–48 ms; 99–105 kbps; 0 mất gói |
| `audio {on:false}` / `{on:true}` / `stop` qua kênh control (adb forward) | tắt: 0 kbps, buffer xả về 0, `audio: off`; bật lại: phát tiếp sau ~40 ms đệm; `stop` → im lặng |

Emulator không cấp FAST track (`performanceMode` = 0 dù yêu cầu LOW_LATENCY) và chạy `-no-window` thì HAL âm thanh của emulator dừng sau ~0,7 s (phải chạy emulator có cửa sổ).

### Trễ âm thanh (ước lượng)

| Khâu | ms |
|---|---|
| WASAPI loopback (chu kỳ shared mode) | ~10 |
| Gom đủ frame 20 ms | 0–20 (tb 10) |
| Opus lookahead (pre-skip 312 mẫu) | 6,5 |
| Mạng | 1–3 Ethernet / 5–25 Wi-Fi |
| Queue trong AudioTrack (bắt đầu 40, trần 80) | tb ~20–45 |
| Mixer + HAL | emulator ~60–75 (đo: write → loa 96–125 ms qua `AudioTimestamp`, hiện ở overlay `out`); TV có FAST track thường 10–40 |
| **Tổng** | **~60–130 ms trên TV** (emulator ~130–170 ms) |

Hình ~25–50 ms (§2) nên tiếng có thể chậm hơn hình ~40–80 ms, vượt mục tiêu lệch ≤ 40 ms của §7. Chưa đo trên TV thật. Hướng giảm nếu cần: frame 10 ms + application RESTRICTED_LOWDELAY (lookahead 2,5 ms), start threshold 20 ms khi TV cấp FAST track, hoặc trễ hình cho khớp tiếng (không muốn cho dạy học).

### Chưa kiểm chứng
- TV/box Android thật (FAST track, trễ thật, lệch hình/tiếng), Wi-Fi thật có jitter và mất gói (chưa có PLC/FEC: gói mất = 20 ms thiếu).
- Thiết bị phát không phải 48 kHz (đường resample chỉ có unit test), 5.1/7.1 (downmix chỉ có unit test), đổi thiết bị phát mặc định giữa chừng.
- Bật/tắt bằng chuột trên GUI thật (logic `set_audio` đã thử qua `cast_toggle`; GUI chỉ kiểm tra hiển thị checkbox).

## 2026-10-19 — Receiver desktop (`nanocast-receiver`, Phase 2)

Mã: `crates/receiver-desktop` (binary `nanocast-receiver`). Cùng giao thức với receiver Android: mDNS `_nanocast._udp` (tên = hostname, TXT w/h/codec/ver/ctrl qua `discovery::Advertiser`), UDP 48322 ráp frame giống `FrameAssembler.kt` (bỏ frame cũ thiếu gói, đếm mất gói theo `seq`), control TCP 48323 giống `ControlServer.kt` (`caps` khi `hello`, `keyframe_request` tối đa 1/500 ms, `stats` mỗi giây), audio UDP 48324 (Opus → `nanocast_core::audio::OpusDecoder` → buffer 40–120 ms → cpal).

Lựa chọn:
- **Decoder: OpenH264 (crate `openh264` 0.9, phần mềm)**, không phải `ffmpeg-next`: máy dev không có FFmpeg dev lẫn libclang (bindgen của `ffmpeg-sys-next` cần), máy lớp học càng không. OpenH264 build từ source kèm theo bằng `cc`, không cần cài gì. Đứng sau trait `VideoDecoder` (`decoder.rs`) để thêm decoder phần cứng sau (d3d11va/dxva2, vaapi/v4l2m2m).
  - Trên máy này **không có NASM** nên OpenH264 build *không có* SIMD x86 (build script tự dùng `nasm` nếu có trong PATH). Số dưới đây là bản C thuần; có NASM sẽ nhanh hơn (chưa đo).
- **Hiển thị: winit 0.30 + wgpu 25**: 3 texture R8 (Y, U, V) → shader đổi BT.709 limited → RGB, letterbox; `PresentMode::AutoNoVsync`, `desired_maximum_frame_latency = 1`, present ngay khi decoder có hình (chỉ giữ hình mới nhất, không xếp hàng). Overlay chữ bằng font bitmap 5×7 nhúng sẵn (không cần file font). Trên Windows thử DX12 trước rồi mới tới backend khác (`WGPU_BACKEND` ghi đè được).
- Hàng chờ decoder tối đa 15 khung (250 ms ở 60 fps); quá thì bỏ cả hàng, chờ IDR, gửi `keyframe_request {decoder_backlog}`; trước IDR đầu tiên: `decoder_start`.

Máy dev: i9-13900HX, Intel UHD (RaptorLake-S) + NVIDIA, Windows 10 19045. Receiver và sender cùng máy, 127.0.0.1. Bản release (`cargo build --release -p nanocast-receiver`): **6,9 MB**.

File thử: `test1080.h264` = `spike-win --seconds 8 --fps 60 --bitrate-mbps 20` (QSV, 1920×1080, desktop bình thường, 17,4 Mbps, 1 IDR); `live2.h264` = 11 s quay lúc cửa sổ receiver hiện trên màn hình đang bị quay (hình lồng nhau, chuyển động nhiều; 18 Mbps, 2 IDR) — trường hợp khó.

| Lần chạy (`spike-send <file> --to 127.0.0.1 --loop`) | Full screen DX12 | fps hiện | complete → decoded tb / p50 / p95 / p99 ms | Gọi decode riêng tb ms | complete → present tb ms | Mất / bỏ |
|---|---|---|---|---|---|---|
| test1080 @ 60 fps, 20 s (1201 khung) | có | 60,2 | 3,87 / 3,2 / 5,9 / 19,6 | 3,59 | 4,58 | 0 / 0 |
| test1080 @ 30 fps, 12 s | có | 30,2 | 3,63 / 3,1 / 5,5 / 8,3 | 3,48 | 4,36 | 0 / 0 |
| live2 (khó) @ 60 fps, 11 s | có | 60,4 | 17,3 / 11,7 / 64,2 / 78,9 | 9,01 | 18,1 | 0 / 0 |
| live2 (khó) @ 30 fps, 12 s | có | 30,2 | 8,13 / 4,0 / 28,0 / 33,9 | 7,00 | 8,92 | 0 / 0 |

"complete → decoded" = từ lúc gói cuối của khung tới lúc decoder trả hình (gồm chờ hàng), cùng định nghĩa với `decode_ms` của receiver Android; "complete → present" = tới lúc `present()` xong. Các lần đo trước đó trong cùng buổi (máy đang chạy build của session khác) cho live2 @ 60 fps tới 57 ms tb, nên số của nội dung khó dao động mạnh theo tải CPU.

CPU / RAM (test1080, full screen, đo `TotalProcessorTime` 10 s): **25 % một nhân** ở 60 fps (≈ 0,8 % tổng máy 32 luồng), 26 % ở 30 fps (CPU hạ xung: decode 5,4 ms/khung); working set **162 MB** (DX12; trước khi đặt `MemoryHints::MemoryUsage` là ~460 MB do gpu-allocator giữ khối lớn; Vulkan ~130 MB, GL ~100 MB lúc chờ).

| Kiểm tra | Kết quả |
|---|---|
| `spike-win --to 127.0.0.1` (sender thật có control) | `hello` → `caps`, `start 1920x1080@60`, `stop` hiện trong log receiver; spike-win: "was connected", nhận đủ `keyframe_request` receiver gửi (`decoder_backlog` khi máy quá tải) |
| Màn hình chờ | tên máy + mọi IPv4 + "DANG CHO MAY TINH KET NOI..." căn giữa; quay lại sau 2 s không có hình hoặc ngay khi nhận `stop` |
| Phím (gửi bằng `WScript.Shell.SendKeys`) | **O** bật overlay (fps, mất gói, decode ms, Mbps, keyframe req, audio) khi đang chiếu; **Esc** thoát trong < 1 s. **F/F11** (bật-tắt full screen) chưa thử |
| Audio: `cargo run --release -p nanocast-receiver --example send_tone -- 127.0.0.1 4` | 200/200 gói, 0 mất, 0 underrun; cpal mở 48000 Hz 2 ch f32, buffer Fixed(480) = 10 ms. (Chưa nghe kiểm bằng tai trong phiên này.) |
| mDNS | log "advertising _nanocast._udp as DESKTOP-…"; chưa thử thấy từ GUI `nanocast.exe` |
| `cargo test -p nanocast-receiver` | 10 test: assembler (thứ tự đảo, mất gói, restart sender), hàng chờ decoder, OpenH264 encode→decode, control qua TCP (caps, giới hạn keyframe, stats, sender mới thay sender cũ), jitter audio, font, letterbox |

Vấn đề gặp:
- Vulkan full screen (Intel): chụp màn hình bằng GDI chỉ thấy khung cũ/trắng và có cảnh báo `Suboptimal present` sau khi chuyển full screen → thêm cấu hình lại swapchain khi suboptimal/đổi kích thước, và mặc định DX12 trên Windows (chụp được, hiển thị đúng).
- Lần đầu đo control, một chương trình khác đang chiếm 127.0.0.1:48323/48322 (session khác) nên receiver không nhận được gì; chạy lại thì bình thường.
- `spike-win` mặc định ghi `out.h264` vào thư mục hiện tại (đã chuyển ra scratchpad).

### Chưa kiểm chứng
- Decoder phần cứng (chưa làm: ffmpeg-next / Media Foundation decoder / VA-API), OpenH264 có NASM, máy yếu thật (PC cũ, mini PC), Raspberry Pi / Linux (hướng dẫn trong README chưa chạy thử).
- TV/máy chiếu thật qua HDMI, LAN/Wi-Fi thật, PC khác thấy receiver qua mDNS, glass-to-glass.
- Nghe âm thanh thật từ `nanocast.exe` (bật Âm thanh) và lệch hình/tiếng; thiết bị phát không phải 48 kHz (đi qua `Resampler`).

## 2026-10-19 — Phase 2: bitrate thích nghi, tạm dừng / màn hình đen, mã 4 số, HEVC

Máy dev như các mục trên (i9 + iGPU Intel + NVIDIA Optimus), emulator `NANOCast-test-API35` (x86_64, `c2.goldfish.*`). Giao thức: PLAN.md §4 (*pause/blank/error*, *Mã 4 số*, *Codec*, *Bitrate thích nghi*).

Công cụ thử:
- `cargo run -p nanocast-core --example ctrl_probe -- 127.0.0.1:48323 [--pin 1234] [--codec hevc] "start; sleep 6; pause on; sleep 6; pause off; blank on; sleep 6; blank off; stop"`: gửi tay control, in mọi message receiver trả về.
- `cast_toggle` (example `nanocast-win`): thêm `--toggle audio|pause|blank`, `--codec`, `--pin`, `--bitrate`, `--fixed`; in thêm codec, codec fallback, bitrate/fps mục tiêu và lý do.
- `spike-win --codec hevc --out x.h265` ghi HEVC; `spike-send x.h265` tách AU theo HEVC (đuôi `.h265`/`.hevc`).
- Emulator: video bằng `spike-send` build cho `x86_64-linux-android` chạy trong máy ảo tới 127.0.0.1, control qua `adb forward tcp:48323 tcp:48323` (kết nối đến từ 127.0.0.1 nên lọc UDP theo IP vẫn khớp). Nhớ `adb forward --remove-all` trước khi thử receiver trên PC, nếu không cổng 48323 trên PC đi thẳng vào emulator.

### Bitrate thích nghi (sender thật → receiver giả báo mất 3 % trong 9 s đầu)

`cast_toggle --bitrate 30`, receiver giả (Python) nhận UDP và gửi `stats {lost = 3 % số gói nhận}`:

| t (s) | Mục tiêu | Thực đo | Cách đổi |
|---|---|---|---|
| 0–2 | 30 Mbps @60 | 29,9 Mbps 60 fps | — |
| 2 | 20 @60 (mất 2,9 %) | vẫn 29,9 | `MeanBitRate` lúc chạy: QSV trả OK nhưng **không áp dụng** (thử cả ép IDR: không đổi) |
| 3,5 | 20 @60 | 15–20 | phát hiện sau 1,5 s (> 125 % mục tiêu) → mở lại encoder; từ đó mọi lần đổi đều mở lại |
| 4 | 15 @60 | 15,0 | mở lại encoder |
| 6 | 10 @30 | 9,8–10,4 Mbps, 29,5–31 fps | mở lại encoder (đổi fps) |
| 9–19 | (hết mất gói) | 10 @30 | chờ 10 s sạch |
| 19 | 15 @60 (mạng ổn định lại) | 14,8–15,3 | mở lại encoder |
| 29 | 20 @60 | 19,8–19,9 | mở lại encoder |

Mỗi lần mở lại encoder: khoảng 0,5 s bitrate thấp (khung đầu IDR), không lỗi, không rớt kết nối. Logic thuần (`nanocast_core::adapt`) có 3 test (bậc xuống/lên, burst keyframe_request, decoder chậm, trần lẻ 25 Mbps, sàn 10 Mbps).

### Tạm dừng / màn hình đen

| Thử | Kết quả |
|---|---|
| Emulator, `ctrl_probe` + `spike-send` (sender giả vẫn gửi hình) | `pause on`: hình đứng yên (decoder vẫn chạy nhưng không render), dấu ⏸ góc trên phải; `pause off`: chạy tiếp. `blank on`: đen toàn màn hình (đè cả overlay số liệu); `blank off`: hình trở lại ở keyframe kế tiếp |
| `cast_toggle --toggle pause --every 4` → `nanocast-receiver` (PC) | Trong lúc dừng sender 0 fps, 0 Mbps (capture bỏ khung trước khi đổi NV12, encoder không chạy, tiếng tắt); tiếp tục → 60 fps 20 Mbps ngay khung sau (IDR ép). Receiver log `paused true/false`, không về màn hình chờ khi đang dừng |
| `cast_toggle --toggle blank --every 3` → `nanocast-receiver` | `blank on/off` đúng thời điểm, receiver xoá hình rồi hiện lại |

### Mã 4 số

| Thử | Kết quả |
|---|---|
| Emulator: `adb shell input keyevent KEYCODE_DPAD_CENTER` | Bật mã (lưu SharedPreferences), hiện `Mã / Code 8958` góc dưới phải, mDNS đăng ký lại (huỷ → đăng ký, ~1 s); bấm lần nữa: tắt |
| `hello` không mã / sai mã | `error {code: "bad_pin"}` rồi đóng kết nối; overlay đếm `code refused 2` |
| `spike-send` gửi UDP khi chưa ai nhập đúng mã | Bị bỏ qua: `fps 0.0 (in 0.0)` dù gói đang tới |
| `hello {pin: "8958"}` | `caps`, hình chạy; sau khi tắt mã, `hello` không mã lại được nhận |
| Receiver desktop `--pin-code 4321` + `cast_toggle --pin 1111` | Sender kết thúc `Ended(PinRejected)`, 0 khung gửi; `--pin 4321`: cast bình thường |
| `cargo test -p nanocast-receiver` | `control::pin_required`: sai/thiếu mã không đá sender đang cast, `allows()` chỉ đúng IP, hết kết nối lại cần mã |

### HEVC

| Thử | Kết quả |
|---|---|
| `spike-win --codec hevc` (Media Foundation, `MFVideoFormat_HEVC`, profile Main) | NVIDIA HEVC MFT lỗi `CoCreateInstance 0x8000FFFF` (như H.264 trên Optimus) → tự dùng **Intel Hardware H265 Encoder MFT**: 1080p60, 15 Mbps thực tế ở mục tiêu 20 Mbps (màn hình tĩnh), encode p50 13,6 / p95 19,6 ms |
| Emulator, `ctrl_probe --codec hevc` + `spike-send x.h265` | Receiver đổi `video/avc → video/hevc`, `c2.goldfish.hevc.decoder`, csd-0 = VPS+SPS+PPS; 30 fps, 0 gói mất, decode 15–16,6 ms; `caps.codecs = ["h264","hevc"]`, TXT `codec=h264,hevc` |
| `cast_toggle --codec hevc` → `nanocast-receiver` | `caps.codecs = ["h264"]` → sender log "sending H.264 instead of Hevc", `CodecFallback::ReceiverH264Only` (GUI ghi lý do) |
| `cast_toggle --codec hevc` → emulator (nhầm qua adb forward) | Emulator báo có hevc → sender mở Intel H265 MFT và gửi HEVC, đúng như thiết kế |

### GUI

- Chạy `nanocast.exe` (release) cạnh `nanocast-receiver --pin-code 2468 --name PIN-TV`: TV hiện trong danh sách kèm `🔒 code`, có nút Tạm dừng / Màn hình đen (tắt khi chưa cast), ngôn ngữ theo hệ thống (English trên máy dev). Đóng cửa sổ thoát sạch.
- Test đơn vị: `config` (đọc/ghi, file cũ/thiếu trường/sai giá trị), `i18n` (locale, số `{}` khớp giữa hai ngôn ngữ), dòng trạng thái VI/EN (bitrate + lý do, codec fallback).
- macOS: `cargo clippy -p nanocast -p nanocast-mac --all-targets --target aarch64-apple-darwin` và `x86_64-apple-darwin` sạch (chỉ type-check).

### Chưa kiểm chứng
- Bấm thử GUI bằng tay: hộp nhập mã (gửi tin nhắn chuột giả vào cửa sổ egui không có tác dụng nên chưa tự động hoá được), "Tự cast lại khi mở app", đổi ngôn ngữ, lưu `config.json` sau khi đổi cài đặt.
- TV/box Android thật (HEVC phần cứng thật, mã 4 số với remote thật), Wi-Fi thật cho bitrate thích nghi (mất gói thật, không phải receiver giả), NVIDIA/AMD MFT có áp dụng `MeanBitRate` lúc chạy hay không (code tự kiểm tra và mở lại encoder nếu không).
- So sánh chất lượng HEVC với H.264 ở cùng bitrate; HEVC trên macOS (chưa làm).

## 2026-10-19 — Kiểm tra receiver Android

Máy dev và emulator như các mục trên (`NANOCast-test-API35`, x86_64, `c2.goldfish.*`, 1920×1080). APK debug mới build (`gradlew.bat assembleDebug`, 0,97 MB), cài bằng `adb install -r`; video bằng `spike-send` build `x86_64-linux-android` chạy trong emulator tới 127.0.0.1 (luồng `spike-win`, 12 s, ~20 Mbps, lặp), control bằng `ctrl_probe` qua `adb forward tcp:48323`. Lưu ý Git Bash trên Windows đổi `/data/...` thành `C:/Program Files/Git/data/...`: đặt `MSYS_NO_PATHCONV=1` khi gọi adb.

| Thử | Kết quả |
|---|---|
| Hình H.264 | `c2.goldfish.h264.decoder` 1920×1080, 59–62 fps, 20 Mbps, 0 gói mất mới, 0 lỗi decoder; ảnh chụp emulator khớp màn hình PC lúc quay |
| Overlay | Hiện IP (10.0.2.15, 10.0.2.16) + cổng udp/tcp/audio, mDNS, trạng thái ctrl (tên + IP máy gửi, độ phân giải, codec, audio off, kf req), decoder, fps in/out, Mbps, gói mất, frame bỏ, decode ms (avg/p95 + session), dòng âm thanh, gợi ý bấm OK, codecs `h264,hevc` |
| `pause on` | Hình đứng yên (không render), biểu tượng ⏸ cam góc trên phải; `pause off` chạy tiếp |
| `blank on` | Toàn màn hình đen (đè cả overlay); `blank off`: hình về ở keyframe kế tiếp |
| Bấm OK (`KEYCODE_DPAD_CENTER`) | Bật mã 4 số, hiện `Mã / Code ####` góc dưới phải; bấm lại: tắt |
| Mã bật, `hello` không mã | `Error bad_pin` rồi đóng kết nối |
| Mã bật, `hello` mã sai (0000) | `Error bad_pin`, đóng kết nối; sau đó UDP vẫn tới nhưng receiver `fps 0.0` (bỏ qua) |
| Mã bật, `hello` đúng mã | `Caps`, stats 60 fps, hình chạy |
| Mã tắt lại, `hello` không mã | Được nhận (`Caps`) |
| Release APK (`assembleRelease`, ký khoá debug) | Cài đè và mở được, tiến trình chạy |

Số quan sát (phiên ~60 s, cả khi `pause`/`blank` thay phiên): decode ms session p50 10,2 / p95 44–54 / p99 ~110 (emulator, đầu phiên có backlog khởi động); cửa sổ 5 s thường avg 9–17 ms.

Cạm bẫy khi thử: đã có hai tiến trình `spike-send` chạy cùng lúc (một lệnh chạy lại) → hai luồng seq trộn nhau, `lost_pkts` nhảy tới ~22 000 và fps dao động. Sau khi `pkill spike-send` và chỉ chạy một sender, `lost_pkts` đứng yên. Không phải lỗi receiver.

Không phát hiện lỗi cần sửa trong `receiver-androidtv/` (chỉ đổi cấu hình build, xem dưới).

### Release / Play Store
- `app/build.gradle.kts`: `release` đọc khoá từ `receiver-androidtv/keystore.properties` (đã `.gitignore`) hoặc biến môi trường `NANOCAST_KEYSTORE`, `NANOCAST_KEYSTORE_PASSWORD`, `NANOCAST_KEY_ALIAS`, `NANOCAST_KEY_PASSWORD`; không có thì ký bằng khoá debug. `isMinifyEnabled = false`. `versionName` đổi từ `0.1.0-spike` sang `0.1.0`.
- `gradlew.bat assembleDebug bundleRelease assembleRelease`: build thành công. `.aab` ~0,72 MB, APK release ~0,72 MB. Đã thử ký `.aab` bằng keystore tạm qua biến môi trường: `jarsigner -verify` báo `jar verified`.
- Hướng dẫn: [play-store.md](play-store.md), [tv-setup.md](tv-setup.md).

### Chưa kiểm chứng
- TV/box Android thật (Game Mode, decoder HW thật, remote thật bấm OK), đường tải APK bằng Downloader, các bước menu từng hãng TV trong `tv-setup.md` (viết theo hiểu biết chung về các hãng, chưa thử trên máy thật).
- Upload `.aab` lên Play Console (chưa có tài khoản/keystore thật), ảnh chụp 1920×1080 cho store chưa tạo.
- Image emulator Android TV (đang dùng image điện thoại với profile `tv_1080p`).

## 2026-10-20 — Phase 1: zero-copy GPU (BGRA→NV12 trên GPU, texture thẳng vào MFT) + bỏ khung không đổi

Mã: `crates/win-capture/src/gpu.rs` (mới), `capture.rs`, `mf_encoder.rs`, `session.rs`; đo bằng `crates/win-capture/examples/pipeline_bench.rs`, thử encoder bằng `examples/nv_probe.rs`.

### Pipeline mới
- **Một device duy nhất**: dùng chính D3D11 device của windows-capture (bật `ID3D11Multithread::SetMultithreadProtected`). Trên luồng capture: `TileHasher` (compute shader `cs_5_0`, hash 32 bit cho mỗi ô 64×64, đọc về ~2 KB/khung 1080p) → nếu có thay đổi, `ID3D11VideoProcessor` đổi BGRA → NV12 BT.709 limited range, co giãn + viền đen về kích thước encoder, ghi thẳng từ texture WGC vào một pool 5 texture NV12 (không copy BGRA, không đọc về RAM). Texture NV12 được giữ chỗ bằng `GpuFrame` (thả = trả lại pool) cho tới khi encoder trả output.
- Encoder: `IMFDXGIDeviceManager` (`MFCreateDXGIDeviceManager` + `ResetDevice`) gửi bằng `MFT_MESSAGE_SET_D3D_MANAGER` **trước** khi đặt media type, chỉ khi MFT báo `MF_SA_D3D11_AWARE`; sample tạo bằng `MFCreateDXGISurfaceBuffer`. MFT không D3D-aware hoặc lỗi khi nhận manager → mở lại với input RAM.
- **Tự chọn + fallback**: không có video processor BGRA→NV12, encoder không nhận texture, lỗi lúc chạy, hoặc capture→encoded > 60 ms trong 1,5 s → chuyển hẳn sang đường CPU cũ (rayon) và mở lại encoder. Ép bằng biến môi trường `NANOCAST_CONVERT=gpu|cpu` (mặc định auto); `NANOCAST_SKIP_STATIC=0` tắt bỏ khung. *(Chưa có ô trong Cài đặt GUI: `crates/app` dùng chung `CastConfig` với Linux/macOS nên chưa thêm trường.)*
- **Bỏ khung không đổi**: khung có hash giống khung trước thì không convert/encode/gửi (đường CPU cũng dùng hash GPU để khỏi đọc về RAM; nếu shader không chạy thì so sánh BGRA trên CPU). Không có khung mới sau **1 s** thì session encode lại khung cuối (TV giữ đồng hồ/timeout). Khung bị bỏ ở đâu đó (hàng đợi đầy, pause) → khung kế tiếp luôn được gửi dù giống, để thay đổi cuối cùng không bị kẹt.
- `CastStats` thêm `gpu_convert`, `frames_unchanged`, `frames_repeated`; `convert_ms` = thời gian CPU trên luồng capture cho mỗi khung (hash + video processor, hoặc đọc về + convert).

### Cách đo
`cargo run --release -p nanocast-win --example pipeline_bench -- --scene motion|static [--cores 2 --skip-cpus 16]`: một `CastSession` thật (cửa sổ 1280×720 → canvas 1920×1080, 60 fps, 20 Mbps, QSV H.264) gửi UDP tới socket cục bộ; cửa sổ test do **tiến trình con** vẽ (không tính vào CPU). CPU = `GetProcessTimes` (user + kernel) của tiến trình sender trong 10 s sau 2 s khởi động, tính theo % **một** nhân. *motion*: toàn bộ cửa sổ đổi mỗi ~16 ms (sọc màu chạy + chữ, trường hợp xấu nhất); *static*: vẽ một lần. "Trước" = `NANOCAST_CONVERT=cpu NANOCAST_SKIP_STATIC=0` (đúng pipeline cũ), "sau" = mặc định (GPU + bỏ khung). Máy: i9-13900HX (8P+16E, 32 luồng), Intel UHD (màn hình) + RTX 4050 Laptop, Win10 19045.

| Kịch bản | Đường | CPU (% 1 nhân) | Khung gửi/s | Mbps | convert | encode | capture→encoded |
|---|---|---|---|---|---|---|---|
| motion, 32 luồng | CPU, không bỏ khung (trước) | 106 % | 61,0 | 20,3 | 2,44 ms | 12,6 ms | 15,7 ms |
| | CPU + bỏ khung | 90 % | 56,1 | 18,7 | 2,95 ms | 12,6 ms | 16,1 ms |
| | GPU, không bỏ khung | 19 % | 57,9 | 20,3 | 0,25 ms | 12,0 ms | 12,4 ms |
| | **GPU + bỏ khung (sau)** | **13 %** | 56,0 | 18,7 | 0,90 ms | 12,3 ms | **13,3 ms** |
| static, 32 luồng | CPU, không bỏ khung (trước) | 107 % | 57,7 | 20,3 | 2,30 ms | 12,6 ms | 15,5 ms |
| | CPU + bỏ khung | 3,4 % | 1,0 (lặp) | 0,00 | — | 15,3 ms | 15,8 ms |
| | GPU, không bỏ khung | 20 % | 57,7 | 20,3 | 0,21 ms | 12,2 ms | 12,5 ms |
| | **GPU + bỏ khung (sau)** | **1,9 %** | 1,1 (lặp) | **0,01** | — | 15,5 ms | 15,6 ms |
| motion, 2 nhân E (CPU 16–17) | CPU, không bỏ khung (trước) | 78 % | 60,7 | 20,3 | 5,72 ms | 13,1 ms | 19,5 ms |
| | CPU + bỏ khung | 76 % | 55,3 | 18,5 | 6,56 ms | 13,2 ms | 20,5 ms |
| | GPU, không bỏ khung | 17,5 % | 60,8 | 20,2 | 0,35 ms | 12,6 ms | 13,0 ms |
| | **GPU + bỏ khung (sau)** | **19,5 %** | 50,8 | 17,5 | 1,17 ms | 12,9 ms | **14,2 ms** |
| static, 2 nhân E | CPU, không bỏ khung (trước) | 80 % | 57,5 | 20,2 | 5,69 ms | 12,9 ms | 19,3 ms |
| | CPU + bỏ khung | 2,2 % | 1,0 (lặp) | 0,00 | — | 14,8 ms | 15,6 ms |
| | GPU, không bỏ khung | 13,6 % | 57,7 | 20,2 | 0,35 ms | 12,7 ms | 13,1 ms |
| | **GPU + bỏ khung (sau)** | **5,2 %** | 1,1 (lặp) | **0,01** | — | 15,6 ms | 15,6 ms |

*Mbps = byte UDP thực nhận ở socket. Ở motion, "bỏ khung" vẫn bỏ được 45–73 khung/10 s vì cửa sổ test vẽ chậm hơn nhịp capture. Ở static, khung gửi là khung lặp mỗi giây (convert không tính vì không có khung mới).*

### Nhận xét
- **CPU giảm 5–8 lần khi có chuyển động** (106 → 13–19 % một nhân ở 1080p60; trên 2 nhân E từ 78 → 18–20 %), capture→encoded giảm ~3 ms (máy mạnh) đến ~6 ms (2 nhân E). Trên máy yếu thật phần convert CPU sẽ còn lớn hơn (5,7 ms trên 2 nhân E so với 2,4 ms trên 32 luồng), nên lợi ích còn lớn hơn.
- **Màn hình tĩnh gần như không tốn gì**: 1,9–5,2 % một nhân và ~10 kbps, thay vì ~100 % CPU và **20 Mbps** — WGC vẫn giao ~58 khung/s cho cửa sổ đứng yên và QSV ở CBR độn bitrate lên đủ 20 Mbps cho các khung giống hệt; bỏ khung giải phóng gần hết băng thông Wi-Fi của lớp học.
- Giá của hash GPU: ~0,6 ms mỗi khung trên luồng capture (chờ GPU đọc về ~2 KB), CPU gần như không đổi.
- Encode ~12–13 ms là của QSV (như Spike 1), không phụ thuộc đường input; khung lặp tĩnh encode ~15 ms (iGPU hạ xung khi rảnh).

### Bẫy khi giả lập máy yếu bằng affinity
Ghim tiến trình vào CPU 0–1 hoặc 2–3 (hai luồng HT của **một** nhân P trên CPU lai Intel) làm tiến trình gần như không được lập lịch: cả đường CPU lẫn GPU tụt còn 9–14 khung/s, capture→encoded 100–370 ms mà tiến trình chỉ dùng ~1 nhân (lần chạy đầu ở CPU 0–1 thì đường CPU lại bình thường, 61 khung/s — không ổn định). Ghim vào 2 hoặc 4 nhân E (CPU 16+, mỗi CPU một nhân vật lý) thì ổn định → các số trên dùng `--cores 2 --skip-cpus 16`. Watchdog 60 ms vẫn được giữ để tự về đường CPU nếu một driver làm đường GPU chậm.

Thiết kế đầu tiên (thiết bị D3D riêng có `VIDEO_SUPPORT`, chép khung WGC sang vòng texture chia sẻ keyed mutex) chạy tốt ở 32 luồng (12 % CPU) nhưng `AcquireSync` thỉnh thoảng chờ 20–30 ms và tốn thêm một lần copy BGRA; đã bỏ để dùng chung device của capture.

### NVIDIA MFT (Optimus) thử lại
`examples/nv_probe.rs`: với mỗi adapter liệt kê encoder bằng `MFTEnum2` + `MFT_ENUM_ADAPTER_LUID` rồi mở (a) kèm D3D manager trên device của adapter đó và đưa texture NV12, (b) không manager, đưa RAM.
| Encoder | Kết quả |
|---|---|
| Intel QSV (`MFTEnumEx` và `MFTEnum2` LUID iGPU) | (a) và (b) đều chạy, 30/30 khung, 17–19 ms |
| NVIDIA H.264 Encoder MFT (`MFTEnumEx`) | `ActivateObject` và `CoCreateInstance` đều `0x8000FFFF` (E_UNEXPECTED) |
| NVIDIA, `MFTEnum2` theo LUID của RTX 4050 (driver báo 2 LUID, thử cả hai) | (a) và (b) đều `0x8000FFFF` ngay ở `ActivateObject`/`CoCreateInstance` |

D3D manager chỉ gửi được **sau** khi có `IMFTransform`, nên không thể là nguyên nhân: MFT NVIDIA hỏng từ bước tạo object trên máy này kể cả khi gắn đúng adapter. Giữ cách cũ (thử lần lượt, bỏ qua cái lỗi). Nghi do driver/Optimus (màn hình nối iGPU); chưa thử NVENC SDK trực tiếp.

### Kiểm tra end-to-end
- `cargo build --release`, `cargo test` (thêm test GPU `video_processor_matches_cpu_levels_and_letterbox`: nguồn trắng 4:3 → Y = 235, viền Y = 16 / UV = 128, khớp đường CPU; texture được trả khi thả `GpuFrame`), `cargo clippy` sạch cho `nanocast-win`, `spike-win`.
- `nanocast-receiver --windowed --on-top --overlay` + `nanocast.exe` (tự cast lại, màn hình 1) trên cùng máy: log "NV12 on the GPU", receiver hiện hình đúng màu, overlay **57 fps, 0 gói mất, decode 4 ms, 28,9 Mbps**. `pipeline_bench --to 127.0.0.1:48322` (cửa sổ 1266×713 co giãn vào 1920×1080) cũng hiện đúng trên receiver.

### Chưa kiểm chứng
- Máy yếu thật (Intel iGPU đời 2015, Celeron 2 nhân), GPU AMD/NVIDIA làm encoder với input texture, Windows 11.
- Đổi cỡ cửa sổ khi đang cast trên đường GPU (video processor được tạo lại theo kích thước mới — đã viết, chưa thử tay), màn hình > 1080p co về 1080p bằng video processor, HDR/màn hình 10 bit.
- Ô chọn GPU/CPU trong Cài đặt GUI (hiện chỉ có biến môi trường).

## 2026-10-20 — Phase 4: Google Cast (`nanocast-cast`), Chromecast có sẵn trong TV

Máy thử: laptop Intel UHD + RTX 4050 (encoder dùng được: Intel QSV, input texture D3D11), Wi-Fi "Public". Thiết bị Cast trên mạng: chỉ có **Google Nest Mini** (loa, không màn hình); TV Android của người dùng không bật lúc thử.

### Đường A trong Chrome trên PC (`examples/cast_preview`, `http://127.0.0.1:47901/`)
| Đo | Kết quả |
|---|---|
| Hình | 1920×1080, H.264 Main QSV, GPU NV12, 55–61 khung/s khi màn hình chuyển động, 8–12 Mbps (CBR 12 Mbps) |
| Tiếng | AAC-LC 48 kHz stereo trong fMP4, Chrome decode đều (~100 kbps khi im lặng) |
| Độ trễ bộ đệm Chrome (`buffered.end − currentTime`) | **2,27–2,29 s**, đứng yên sau 10 s, 70 s, 100 s |
| Khung rớt (`getVideoPlaybackQuality`) | 0 / 3867 (một lần 1 ngay lúc mở trang) |
| Khởi động | init segment sẵn sàng < 1 s sau khi chạy (keyframe đầu tiên) |

Seek tới gần cuối bộ đệm để giảm trễ: trình phát lùi về 9 s và kẹt ở `HAVE_FUTURE_DATA` → không dùng. Trễ thực tế đường A trên TV = khoảng 2–3 s + mạng (chưa đo).

### Kênh Cast v2 trên Nest Mini (`examples/cast_tv`, tắt tiếng)
- mDNS tìm thấy sau ~1–4 s; TLS 1.2; kết nối + LAUNCH Default Media Receiver: **2,5 s** tới khi LOAD được trả lời.
- LAUNCH mirroring `0F5096E8` → `LAUNCH_ERROR SYSTEM_ERROR` (loa) → tự lùi về đường A trong < 0,1 s.
- LOAD → `LOAD_FAILED` sau ~10 s, không có kết nối HTTP nào tới PC: Wi-Fi ở chế độ Public nên tường lửa chặn cổng 47900 (có thể loa cũng không nhận `video/mp4`). Phiên báo lỗi tường lửa bằng tiếng Việt như thiết kế.

### Chưa kiểm chứng
- Hình thật trên TV Chromecast/Google TV qua cả hai đường; độ trễ đường B (mục tiêu 0,1–0,4 s, `targetDelay` 300 ms); 1080p60 trên receiver Cast; chạy 90 phút; mất gói thật.
