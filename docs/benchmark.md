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
