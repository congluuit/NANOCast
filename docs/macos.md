# NANOCast trên macOS (Phase 3)

Crate `crates/mac-capture` (`nanocast-mac`) là bản macOS của `nanocast-win`: cùng giao thức (PLAN.md mục 4), cùng API để GUI `crates/app` chỉ cần chọn crate bằng `cfg(target_os)`.

> **Trạng thái 2026-10-19: viết xong nhưng CHƯA KIỂM CHỨNG trên Mac thật.** Code được viết và type-check trên Windows. Lần đầu chạy trên Mac mini rất có thể còn lỗi lúc chạy (xem mục 7).

## 1. Pipeline

```
SCShareableContent (liệt kê display / cửa sổ)
  -> SCStream: NV12 (420YpCbCr8BiPlanarVideoRange), scale sẵn trên GPU vào khung cố định,
     minimumFrameInterval 1/fps, queueDepth 3, showsCursor, capturesAudio (macOS 13+)
  -> CVPixelBuffer (zero-copy, không đổi màu bằng CPU)
  -> VTCompressionSession H.264: RealTime, AllowFrameReordering=false (không B-frame),
     AverageBitRate + DataRateLimits (1,5x trong 1 s), Main AutoLevel, CABAC,
     MaxKeyFrameInterval, EnableLowLatencyRateControl nếu máy hỗ trợ (nếu không, mở session thường),
     ép IDR bằng kVTEncodeFrameOptionKey_ForceKeyFrame khi TV gửi keyframe_request
  -> AVCC (độ dài 4 byte) -> Annex B, chèn SPS/PPS từ CMFormatDescription trước mỗi keyframe
  -> nanocast_core::protocol::PacedSender (UDP 48322) + ControlLink (TCP 48323)
Âm thanh: SCStream audio (f32 planar 48 kHz 2 kênh) -> xen kẽ stereo -> Opus 20 ms
  (nanocast_core::audio, giống Windows) -> UDP 48324, header 0x4E41|seq|pts_ms, gói im lặng 4 kbps.
```

| Windows (`nanocast-win`) | macOS (`nanocast-mac`) |
|---|---|
| `capture.rs` WGC + đổi BGRA->NV12 trên CPU | `capture.rs` ScreenCaptureKit, NV12 + scale trên GPU (`convert_ms` luôn 0) |
| `mf_encoder.rs` Media Foundation | `vt_encoder.rs` VideoToolbox (`mf_encoder::hw_encoder_names` là bí danh) |
| `audio.rs` WASAPI loopback (cpal) | `audio.rs` ScreenCaptureKit audio (`AudioTap` trong SCStream + luồng Opus) |
| `sources.rs` HWND / HMONITOR | `sources.rs` CGWindowID (vẫn ở trường `hwnd`) / chỉ số display 1.. |
| `control_link.rs` | bản sao y nguyên (thuần std; nên chuyển vào `nanocast-core` sau) |
| `session.rs` | cùng `CastConfig`/`CastSession`/`CastStats`/`Phase`/`EndReason` |

Bindings: các crate `objc2-*` 0.3 (`objc2-screen-capture-kit`, `objc2-video-toolbox`, `objc2-core-media`, `objc2-core-video`, `objc2-core-graphics`, `objc2-core-foundation`, `objc2-core-audio-types`), cùng `block2` và `dispatch2`. Đây là bindings sinh tự động từ header của Apple nên có đủ `VTCompressionSession` và các key `kVT...`, không cần tự viết `extern "C"`. Không dùng crate `screencapturekit`: bản mới của nó build một cầu nối Swift trong `build.rs`, nên không check được từ Windows.

Không cần C toolchain cho Opus (`unsafe-libopus` thuần Rust), nhưng **cần Xcode Command Line Tools** để link (linker + SDK).

## 2. Những gì đã kiểm chứng và chưa

Đã làm trên máy Windows:
- `cargo build -p nanocast-mac`: sạch (trên Windows crate chỉ còn hai module thuần Rust `annexb`, `convert`).
- `cargo test -p nanocast-mac`: 5 test chạy qua, gồm AVCC->Annex B (SPS/PPS trước IDR, bỏ AUD và SPS/PPS trùng, prefix 1/2/4 byte, dữ liệu cụt) và kích thước Retina -> 1080p.
- `cargo check` và `cargo clippy --all-targets` cho cả `--target x86_64-apple-darwin` và `--target aarch64-apple-darwin`: sạch. Nghĩa là toàn bộ code macOS đã qua kiểm tra kiểu với bindings thật, chỉ chưa link và chưa chạy.

Chưa kiểm chứng (cần Mac):
1. Link được với SDK, chạy được với `MACOSX_DEPLOYMENT_TARGET=12.3` trên Intel và Apple Silicon.
2. SCK thật sự giao NV12 đúng kích thước khung, giữ tỉ lệ có viền đen khi cast cửa sổ (`preservesAspectRatio` chỉ đặt từ macOS 14; trên 12.3–13 dựa vào hành vi mặc định).
3. Đọc `SCStreamFrameInfoStatus` từ attachments (bỏ khung Idle/Blank) hoạt động như mong đợi.
4. VideoToolbox: low-latency rate control có nhận Main profile + CABAC không (nếu không, các thuộc tính đó chỉ bị bỏ qua và có cảnh báo trong log); `DataRateLimits` có làm encoder bỏ khung không; `ForceKeyFrame` sinh IDR kèm SPS/PPS.
5. Teardown: `CompleteFrames` + `Invalidate` không treo; `stopCapture` trả lời trong 2 s.
6. Âm thanh: định dạng SCK đúng là f32 planar 48 kHz như giả định; bật/tắt giữa chừng; im lặng -> gói 10 byte.
7. Cửa sổ bị đóng -> `SourceClosed` (qua `stream:didStopWithError:` hoặc `CGWindowListCopyWindowInfo` mỗi giây).
8. Độ trễ, fps thật (queueDepth 3 có đủ không), CPU.
9. Quyền Screen Recording / Local Network khi chạy từ `.app` ký ad-hoc.
10. GUI `crates/app` trên macOS (đã nối, chưa chạy; xem mục 4).

## 3. Build và chạy thử trên Mac mini

Chuẩn bị (một lần):

```bash
xcode-select --install
```

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

```bash
rustup target add x86_64-apple-darwin aarch64-apple-darwin
```

Kiểm tra nhanh (máy hiện tại, debug):

```bash
cargo test -p nanocast-mac
```

```bash
cargo run -p nanocast-mac --example mac_cast -- --list
```

Lần đầu, macOS hỏi quyền Screen Recording cho **ứng dụng terminal** (Terminal / iTerm / VS Code), không phải cho NANOCast. Bật quyền trong System Settings > Privacy & Security > Screen Recording, rồi **thoát hẳn và mở lại terminal**. Sau đó `--list` in ra display và cửa sổ (`window:<id>`).

Cast thử tới một receiver (TV Android, `receiver-desktop`, hoặc receiver giả trên PC):

```bash
cargo run --release -p nanocast-mac --example mac_cast -- 192.168.1.50 1 30 --audio
```

```bash
cargo run --release -p nanocast-mac --example mac_cast -- 192.168.1.50 window:1234 60
```

Mỗi 0,5 s in một dòng: encoder (hardware/software, low-latency), kích thước, fps, Mbps, thời gian encode, độ trễ từ capture tới lúc gửi, số keyframe / keyframe_request, số khung bị bỏ, âm thanh. Log chi tiết: thêm `RUST_LOG=debug`.

Build universal bằng tay:

```bash
MACOSX_DEPLOYMENT_TARGET=12.3 cargo build --release -p nanocast-mac --example mac_cast --target x86_64-apple-darwin
```

```bash
MACOSX_DEPLOYMENT_TARGET=12.3 cargo build --release -p nanocast-mac --example mac_cast --target aarch64-apple-darwin
```

```bash
lipo -create target/x86_64-apple-darwin/release/examples/mac_cast target/aarch64-apple-darwin/release/examples/mac_cast -output target/mac_cast-universal
```

```bash
lipo -info target/mac_cast-universal
```

Đóng gói `.app` cho công cụ thử `mac_cast` (không có GUI): `sh crates/mac-capture/macos/bundle.sh --example mac_cast`. Bản cài GUI cho người dùng thì dùng `installer/macos/build-mac.sh` (mục 3a).

## 3a. Đóng gói bản cài `.dmg` trên Mac mini (`installer/macos/build-mac.sh`)

> **Trạng thái 2026-10-20: CHƯA CHẠY trên Mac thật.** Script đã qua `bash -n`; `cargo check --release -p nanocast` cho `aarch64-apple-darwin` và `x86_64-apple-darwin` sạch trên Windows. Lần chạy đầu trên Mac mini có thể còn lỗi nhỏ (đường dẫn, `hdiutil`).

Một Mac (Intel hay Apple Silicon đều được) build ra **một** file `.dmg` universal chạy trên cả hai loại máy: Rust biên dịch chéo hai target, `lipo` ghép lại. Không cần Xcode đầy đủ, chỉ cần Command Line Tools.

Chuẩn bị một lần (đã có ở mục 3 thì bỏ qua):

```bash
xcode-select --install
```

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Lấy mã nguồn (hoặc `git pull` nếu đã có):

```bash
git clone https://github.com/congluuit/NANOCast.git
```

```bash
cd NANOCast
```

Build và đóng gói:

```bash
bash installer/macos/build-mac.sh
```

Thêm bản riêng cho từng loại máy (file nhỏ hơn, tuỳ chọn):

```bash
bash installer/macos/build-mac.sh --per-arch
```

Script in từng bước (`==> ...`) và dừng ngay ở bước lỗi. Chạy lại bao nhiêu lần cũng được: mọi kết quả cũ bị xoá và tạo lại. Các bước:

1. Kiểm tra công cụ (`cargo`, `rustup`, `lipo`, `iconutil`, `codesign`, `hdiutil`, `plutil`, `ditto`) và đọc version từ `Cargo.toml` (`[workspace.package] version`).
2. `rustup target add aarch64-apple-darwin x86_64-apple-darwin` (chỉ tải khi thiếu).
3. `cargo build --release -p nanocast --target <target>` cho từng target, `MACOSX_DEPLOYMENT_TARGET=12.3`.
4. `lipo -create` ra binary universal.
5. `NANOCast.icns` bằng `iconutil` từ `assets/generated/icon-16.png` … `icon-1024.png` (sinh bằng `cargo run -p icongen`).
6. `NANOCast.app`: `Contents/MacOS/nanocast`, `Contents/Resources/NANOCast.icns`, `Contents/Info.plist` từ `installer/macos/Info.plist` (thay `__VERSION__`), `plutil -lint`.
7. `codesign --force --deep -s -` (ad-hoc) rồi `codesign --verify --deep --strict`.
8. `hdiutil create` (UDZO) chứa `NANOCast.app` và lối tắt `Applications`, rồi `hdiutil verify`.

Kết quả:

| File | Dùng cho |
|---|---|
| `installer/out/NANOCast-<version>-macos-universal.dmg` | Mọi Mac (Intel và Apple Silicon), macOS 12.3+. File phát hành chính. |
| `installer/out/NANOCast-<version>-macos-arm64.dmg` | Chỉ Apple Silicon (M1/M2/M3/M4…), với `--per-arch`. |
| `installer/out/NANOCast-<version>-macos-x86_64.dmg` | Chỉ Mac Intel, với `--per-arch`. |
| `target/macos-pkg/universal/NANOCast.app` | Chạy thử ngay không cần mở `.dmg`: `open target/macos-pkg/universal/NANOCast.app`. |

Tuỳ chọn khác: `--no-build` đóng gói lại binary đã có trong `target/` (sửa Info.plist/icon mà không build lại); `SIGN_IDENTITY="NANOCast Dev"` ký bằng chứng chỉ tự ký để giữ quyền Screen Recording qua các lần build (mục 5).

Về Info.plist (`installer/macos/Info.plist`):
- `CFBundleIdentifier` = `com.redharegames.nanocast`, `LSMinimumSystemVersion` = 12.3 (ScreenCaptureKit), `LSUIElement` = false (app thường, có icon Dock).
- Screen Recording: macOS không đọc key Info.plist nào cho quyền này, hộp thoại do hệ thống viết. `NSScreenCaptureUsageDescription` chỉ để mô tả, bị bỏ qua cũng không sao.
- **Không** có `NSMicrophoneUsageDescription`: tiếng hệ thống lấy qua ScreenCaptureKit (`capturesAudio`), thuộc quyền Screen Recording. NANOCast không mở micro, nên không có lý do hỏi quyền micro.
- `NSLocalNetworkUsageDescription` + `NSBonjourServices` (`_nanocast._udp`, `_googlecast._tcp`): macOS 15+ hỏi quyền Local Network để tìm TV và Chromecast.
- `NSSupportsAutomaticGraphicsSwitching`: MacBook Intel hai GPU không bị ép sang GPU rời chỉ vì cửa sổ OpenGL.

Nền `.dmg` để mặc định (không ảnh nền): đặt nền và vị trí icon cần AppleScript điều khiển Finder trong phiên đăng nhập có màn hình, runner CI không có.

CI: `.github/workflows/ci.yml` (job `macos`) build `nanocast` cho cả hai target ở mỗi push; `release.yml` chạy `build-mac.sh` trên `macos-latest` khi push tag `v*` và đưa `NANOCast-<version>-macos-universal.dmg` lên GitHub Release cùng bản Windows và APK. Nếu riêng job macOS lỗi, Release vẫn được tạo (thiếu `.dmg`).

## 3b. Cài trên Mac và mở lần đầu (bản ký ad-hoc, chưa có Developer ID)

1. Mở `NANOCast-<version>-macos-universal.dmg`, kéo **NANOCast** vào **Applications**.
2. Lần đầu mở, macOS báo *"NANOCast" không thể mở vì Apple không thể kiểm tra phần mềm độc hại* (hoặc *"is damaged"*). Đây là do app chưa ký Developer ID / chưa notarize, không phải app hỏng. Chọn một trong các cách:
   - **Chuột phải** (hoặc Control + bấm) vào NANOCast trong Applications > **Open** > **Open**. Chỉ cần làm một lần.
   - macOS 15 (Sequoia) trở lên không còn cho chuột phải > Open: mở thử một lần, rồi vào **System Settings > Privacy & Security**, kéo xuống, bấm **Open Anyway** cạnh dòng về NANOCast, nhập mật khẩu.
   - Hoặc trong Terminal (bỏ cờ quarantine, cách chắc nhất):
     ```bash
     xattr -dr com.apple.quarantine /Applications/NANOCast.app
     ```
3. Cấp quyền **Screen Recording**: lần đầu NANOCast hiện dòng nhắc và nút cấp quyền (hoặc macOS tự hỏi). Vào **System Settings > Privacy & Security > Screen Recording** (macOS 15: *Screen & System Audio Recording*), bật **NANOCast**. Sau đó **thoát hẳn NANOCast (Cmd+Q) và mở lại**, quyền mới có hiệu lực.
4. macOS 15+: khi được hỏi *"NANOCast muốn tìm thiết bị trong mạng cục bộ"*, chọn **Allow**. Từ chối thì danh sách TV sẽ rỗng (bật lại ở *Privacy & Security > Local Network*).
5. Cài bản mới đè lên: bản ad-hoc có chữ ký khác mỗi lần build, nên macOS có thể hỏi lại quyền Screen Recording. Nếu công tắc đã bật mà vẫn không có hình: tắt rồi bật lại, hoặc `tccutil reset ScreenCapture com.redharegames.nanocast` rồi cấp lại.

## 4. GUI `crates/app` trên macOS (đã nối 2026-10-19, chưa chạy trên Mac)

Đã làm đúng các bước dưới đây (`cargo clippy -p nanocast --target aarch64-apple-darwin` / `x86_64-apple-darwin` sạch). Thêm so với Windows: tạm dừng / màn hình đen / mã 4 số dùng chung; bitrate thích nghi chỉ đổi bitrate (`VtEncoder::set_bitrate`, fps giữ nguyên vì đổi fps phải khởi động lại SCStream); chọn HEVC thì vẫn gửi H.264 (`CodecFallback::NoEncoder`, HEVC qua VideoToolbox chưa làm). `ControlLink` nay nằm trong `nanocast_core::control_link` (hai crate chỉ re-export).

Các bước (để tham khảo):

1. `crates/app/Cargo.toml`: thêm khối sau (eframe có thể chuyển lên dependency chung cho cả hai OS); thêm `nanocast-mac = { path = "crates/mac-capture" }` vào `[workspace.dependencies]` của `Cargo.toml` gốc.
   ```toml
   [target.'cfg(target_os = "macos")'.dependencies]
   nanocast-mac.workspace = true
   eframe = { version = "0.37", default-features = false, features = ["glow", "default_fonts"] }
   ```
2. `crates/app/src/main.rs`: đổi `#[cfg(windows)]` thành `#[cfg(any(windows, target_os = "macos"))]` cho `mod gui` và `fn main`, và đổi `not(windows)` tương ứng.
3. `crates/app/src/gui.rs`: thay bốn dòng `use nanocast_win::...` bằng một bí danh:
   ```rust
   #[cfg(windows)]
   use nanocast_win as platform;
   #[cfg(target_os = "macos")]
   use nanocast_mac as platform;
   use platform::capture::Source;
   use platform::mf_encoder;
   use platform::session::{CastConfig, CastSession, CastStats, EndReason, Phase};
   use platform::sources::{self, MonitorInfo, WindowInfo};
   ```
   Test trong `gui.rs` cũng vậy: dùng `platform::control_link::{ReceiverReport, TvStats}`.
4. Font: `install_fonts` đọc `%WINDIR%\Fonts\segoeui.ttf`. Trên macOS hãy đọc `/System/Library/Fonts/Supplemental/Arial.ttf` (đủ dấu tiếng Việt). Trường hợp không có thì giữ font mặc định của egui.
5. Quyền: khi mở app, nếu `sources::has_screen_recording_permission()` trả về false thì hiện một dòng hướng dẫn và nút "Cấp quyền" gọi `sources::request_screen_recording_permission()`. Đừng chờ tới lúc bấm Cast, vì danh sách cửa sổ sẽ rỗng khi chưa có quyền.
6. Nút Âm thanh: `sources::audio_supported()` là false trên macOS 12, khi đó disable nút. Nếu vẫn bật, `CastStats::audio_error` sẽ báo "system sound needs macOS 13 or later".
7. `crates/app/build.rs` chỉ nhúng icon khi `cfg(windows)`, nên không cần đổi. Icon macOS do `installer/macos/build-mac.sh` thêm vào.

## 5. Quyền (TCC) và mạng

**Screen Recording** (bắt buộc, cả hình lẫn tiếng):
- macOS tự hỏi ở lần đầu app gọi ScreenCaptureKit (`SCShareableContent`), hoặc khi gọi `CGRequestScreenCaptureAccess()` (`sources::request_screen_recording_permission`). `CGPreflightScreenCaptureAccess()` (`has_screen_recording_permission`) chỉ kiểm tra, không hiện hộp thoại.
- Sau khi người dùng bật quyền, **phải khởi động lại app** thì quyền mới có hiệu lực.
- Không có key Info.plist nào cho quyền này. Hộp thoại do hệ thống viết.
- macOS 15 (Sequoia) trở lên định kỳ hỏi lại ("NANOCast muốn tiếp tục ghi màn hình"), và hiện chỉ báo ghi màn hình màu tím trên thanh menu. Đây là hành vi bình thường.
- Quyền gắn với chữ ký code. Bản ký **ad-hoc** (`codesign -s -`) đổi danh tính sau mỗi lần build, nên macOS coi đó là app mới và phải cấp quyền lại. Để giữ quyền qua các lần build, tạo một chứng chỉ code-signing tự ký trong Keychain Access (Certificate Assistant > Create a Certificate, loại "Code Signing", ví dụ tên "NANOCast Dev"), rồi build với:
  ```bash
  SIGN_IDENTITY="NANOCast Dev" bash installer/macos/build-mac.sh
  ```
- Xoá quyền cũ khi bị kẹt (ví dụ hộp thoại không hiện nữa mà quyền vẫn tắt):
  ```bash
  tccutil reset ScreenCapture com.redharegames.nanocast
  ```
- Chạy từ terminal (`cargo run`) thì quyền thuộc về terminal, không phải NANOCast.

**Local Network** (macOS 15+): tìm TV qua mDNS và gửi UDP/TCP trong LAN cần quyền Local Network. Info.plist mẫu đã có `NSLocalNetworkUsageDescription` và `NSBonjourServices` (`_nanocast._udp`). Nếu người dùng từ chối, danh sách TV sẽ rỗng và gói tin không đi (lỗi `No route to host` / `EHOSTUNREACH`). Bật lại trong System Settings > Privacy & Security > Local Network.

**Firewall**: sender chỉ gửi ra, không mở cổng nghe, nên firewall của macOS không chặn.

## 6. Ghi chú thiết kế

- Khung encoder cố định như Windows: display giữ tỉ lệ thật, tối đa 1080p (Retina 2560×1600 thành 1728×1080). Cửa sổ dùng khung 1920×1080 cố định, SCK co giãn và thêm viền đen, nên đổi cỡ cửa sổ không phải tạo stream mới.
- `queueDepth` = 3, kênh capture->encoder chứa 1 khung. Nếu SCK giữ lại khung, cùng lắm có 1 khung trong kênh và 1–2 khung trong VideoToolbox. Nếu fps thấp hơn mong đợi mà `frames_dropped` không tăng, thử `QUEUE_DEPTH` = 5 trong `capture.rs`.
- Âm thanh được tap ngay trong SCStream suốt buổi cast (macOS 13+). `set_audio(false)` chỉ ngắt luồng Opus, nên bật/tắt không phải cấu hình lại stream. `excludesCurrentProcessAudio` = true.
- Màn hình đứng yên thì SCK không gửi khung (giống WGC), và TV giữ hình cuối. Keyframe định kỳ chỉ đến khi có khung mới. Khi có kênh control, TV vào sau sẽ xin keyframe.
- Nếu encoder lỗi trước khung đầu tiên khi đang ở chế độ low-latency, session thử lại một lần với rate control thường (giống cách Windows thử MFT kế tiếp).
- Khi `VTCompressionSessionEncodeFrame` báo lỗi đồng bộ, metadata của khung đó bị bỏ lại (leak vài byte) thay vì giải phóng, để tránh double free. Lỗi này kết thúc buổi cast.

## 7. Lỗi có thể gặp

| Triệu chứng | Nguyên nhân có thể | Cách xử lý |
|---|---|---|
| `--list` rỗng, log "ScreenCaptureKit: ... grant Screen Recording" | Chưa có quyền, hoặc vừa cấp mà chưa khởi động lại | Bật quyền rồi thoát hẳn app/terminal; nếu cần thì `tccutil reset ScreenCapture ...` |
| Crash `CGS_REQUIRE_INIT` khi chạy CLI | Tiến trình không có NSApplication | Code đã gọi `CGMainDisplayID()` trước SCK; nếu vẫn lỗi, chạy từ `.app` hoặc khởi tạo `NSApplication` |
| Lỗi link `ld: framework not found ScreenCaptureKit` | SDK cũ / thiếu Command Line Tools | `xcode-select --install`, cần SDK macOS 12.3+ |
| Crash "unrecognized selector setCapturesAudio:" | macOS < 13 | Code đã kiểm tra `audio_supported()`; nếu vẫn gặp thì báo lỗi kèm phiên bản macOS |
| `VTCompressionSessionCreate failed (-12902/-12908)` | Thông số không hỗ trợ (kích thước lẻ, low-latency trên Intel cũ) | Đã tự lùi về session thường; kiểm tra kích thước chẵn trong log |
| Log "VideoToolbox ProfileLevel/H264EntropyMode not applied" | Low-latency rate control ép profile riêng | Thường vô hại; nếu TV không giải mã được thì thử bỏ low-latency (`VtEncoder::new(cfg, false)`) |
| TV đen, log có "frame too large to packetize" | Keyframe quá to so với `MAX_PACKETS_PER_FRAME` | Giảm bitrate |
| TV xanh lá / sai màu | Buffer không phải NV12 video-range, hoặc ma trận màu khác | Kiểm tra `CVPixelBufferGetPixelFormatType`; đặt `colorMatrix` = ITU_R_709_2 trong `capture.rs` |
| fps thấp, `frames_dropped` = 0 | `queueDepth` quá nhỏ, SCK phải chờ | Tăng `QUEUE_DEPTH` lên 5 |
| Cast cửa sổ báo "window not found" | Cửa sổ ở Space khác / đã đóng / app ẩn | Làm mới danh sách; cửa sổ thu nhỏ không cast được (giống Windows) |
| Không có tiếng, `audio_kbps` ≈ 4 | Không có gì đang phát, hoặc tiếng đi ra thiết bị không được tap | SCK tap bản trộn hệ thống; thử phát nhạc trong Safari / Music |
| Lỗi "unexpected audio format" | SCK giao int16 hoặc kiểu khác | Thêm nhánh chuyển đổi trong `audio.rs::convert` |
| Không thấy TV, macOS 15 | Từ chối quyền Local Network | System Settings > Privacy & Security > Local Network |
| App bị chặn "is damaged / unidentified developer" | Quarantine, chưa notarize | Chuột phải > Open hoặc `xattr -dr com.apple.quarantine` |

## 8. Việc tiếp theo

1. Chạy `mac_cast` trên Mac mini (Apple Silicon và nếu được thì cả Intel), sửa lỗi lúc chạy, ghi số đo vào `docs/benchmark.md`.
2. Chạy `installer/macos/build-mac.sh` trên Mac mini (mục 3a), cài `.dmg` trên một Mac Intel và một Mac Apple Silicon, ghi kết quả vào mục 2.
3. Chuyển `control_link.rs` (đang trùng với Windows) vào `nanocast-core`.
4. Tuỳ chọn: ký bằng Developer ID + notarize (`codesign --options runtime` + `xcrun notarytool`) để bỏ bước mở lần đầu ở mục 3b. CI đã build `.dmg` (mục 3a).
