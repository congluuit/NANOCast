# Đăng NANOCast Receiver lên Google Play (Android TV)

Gói: `com.redharegames.nanocast.receiver`. Mã ở `receiver-androidtv/`.

## 1. Tạo keystore ký release

Làm một lần và **sao lưu file keystore + mật khẩu ở nơi an toàn** (mất là không cập nhật app được, trừ khi dùng Play App Signing, xem dưới). Không commit keystore.

```bash
keytool -genkeypair -v -keystore nanocast-upload.jks -alias nanocast -keyalg RSA -keysize 4096 -validity 10000
```
(`keytool` nằm trong JDK 17+ hoặc `Android Studio\jbr\bin`.)

Cho Gradle biết keystore, **một trong hai cách**:

**a) `receiver-androidtv/keystore.properties`** (đã nằm trong `.gitignore`):
```properties
storeFile=nanocast-upload.jks
storePassword=...
keyAlias=nanocast
keyPassword=...
```
`storeFile` tính từ thư mục `receiver-androidtv/`.

**b) Biến môi trường:** `NANOCAST_KEYSTORE` (đường dẫn), `NANOCAST_KEYSTORE_PASSWORD`, `NANOCAST_KEY_ALIAS`, `NANOCAST_KEY_PASSWORD`.

Không có cấu hình nào, `release` được ký bằng khoá debug để build thử (**Play Console sẽ từ chối** file đó, hoặc không nên dùng).

### Ký APK trên GitHub (để TV tự cập nhật)

APK trên GitHub Releases tự cập nhật trên TV (`Updater.kt`). Android chỉ cho cài đè khi **mọi bản đều ký cùng một khoá**, nên `release.yml` lấy keystore từ secrets của repo (Settings › Secrets and variables › Actions):

| Secret | Giá trị |
|---|---|
| `ANDROID_KEYSTORE_BASE64` | file `.jks` mã hoá base64: `base64 -w0 nanocast-upload.jks` (PowerShell: `[Convert]::ToBase64String([IO.File]::ReadAllBytes("nanocast-upload.jks"))`) |
| `ANDROID_KEYSTORE_PASSWORD` | `storePassword` |
| `ANDROID_KEY_ALIAS` | `keyAlias` (ví dụ `nanocast`) |
| `ANDROID_KEY_PASSWORD` | `keyPassword` |

Thiếu secret, workflow vẫn chạy nhưng ký bằng khoá debug tạm của máy CI (mỗi bản một khoá): TV không tự cập nhật được, chỉ báo "cài lại app từ máy tính". TV đang chạy bản ký khoá khác (bản 0.1.0, bản build tay) cũng vậy, cài lại **một lần** bằng nút "Cài app lên TV" trên PC (nó gỡ bản cũ trước), từ đó trở đi tự cập nhật.

## 2. Build

```bash
cd receiver-androidtv
# versionName = version trong Cargo.toml ([workspace.package]), versionCode = major*10000 + minor*100 + patch:
# tăng version trong Cargo.toml là đủ (mỗi lần upload versionCode phải lớn hơn)
gradlew.bat bundlePlay         # .aab cho Play: app/build/outputs/bundle/play/app-play.aab  (~0,7 MB)
gradlew.bat assembleRelease    # APK ký release để sideload / GitHub: app/build/outputs/apk/release/app-release.apk
```
Build type `play` = `release` nhưng **không tự cập nhật** và bỏ quyền `REQUEST_INSTALL_PACKAGES` (`src/play/AndroidManifest.xml`): Play cấm app tự cài bản mới ngoài Play. APK `release` (bản trên GitHub) thì tự cập nhật từ GitHub Releases (`Updater.kt`).

`isMinifyEnabled = false`: app nhỏ, không dùng reflection tới MediaCodec nên không cần R8/ProGuard (tránh rủi ro làm hỏng lớp codec).

Khuyến nghị bật **Play App Signing** (mặc định với app mới): Google giữ khoá ký cuối, bạn chỉ giữ *upload key* (keystore ở trên) và có thể reset nếu mất.

## 3. Yêu cầu Android TV (Leanback)

Đã có trong `AndroidManifest.xml`:
- `<uses-feature android:name="android.software.leanback" android:required="false"/>` — `false` để app **cài được cả trên box/điện thoại Android**; với Play Console chọn form factor *TV*.
- `<uses-feature android:name="android.hardware.touchscreen" android:required="false"/>` — TV không có cảm ứng.
- Activity có `intent-filter` `LEANBACK_LAUNCHER` (hiện trên launcher TV) và `LAUNCHER`.
- `android:banner="@drawable/banner"` (320×180 dp; file `res/drawable-xhdpi/banner.png` = 640×360 px).
- Điều khiển bằng D-pad: **OK** bật/tắt mã 4 số. Không cần chạm.
- Quyền (bản `play`) chỉ `INTERNET`, `ACCESS_NETWORK_STATE` (và `CHANGE_WIFI_MULTICAST_STATE` nếu thêm sau cho mDNS). Không cần quyền nhạy cảm.

Google duyệt app TV: chạy được hoàn toàn bằng remote, không có màn hình bắt buộc cảm ứng, banner đúng kích thước, không quảng cáo đè.

## 4. Store listing

Tài nguyên (đã có hoặc tạo từ `assets/generated/`):

| Mục | Kích thước | Ghi chú |
|---|---|---|
| Icon app | **512×512** PNG | `assets/generated/icon-512.png` |
| Banner TV | **1280×720**(Play, feature graphic) và **320×180** (banner trong app) | Banner trong app đã có (`banner.png`); feature graphic 1024×500 cho trang store |
| Ảnh chụp TV | **1920×1080** (tối thiểu 2, tối đa 8) | Chụp từ emulator: `adb exec-out screencap -p > shot1.png`; chụp: màn chờ có IP, đang nhận hình, overlay, mã 4 số, tạm dừng |

Phân loại nội dung: Utilities / Tools; đối tượng: mọi lứa tuổi; không quảng cáo; miễn phí.

### Mô tả mẫu — Tiếng Việt
**Tên:** NANOCast — Nhận màn hình PC lên TV

**Mô tả ngắn (≤ 80 ký tự):** Nhận hình & tiếng từ máy tính qua mạng LAN, độ trễ thấp, miễn phí.

**Mô tả đầy đủ:**
NANOCast là ứng dụng mã nguồn mở, miễn phí, giúp cast màn hình máy tính (Windows, macOS, Linux) lên TV cùng mạng LAN với độ trễ thấp, phù hợp để dạy học và trình chiếu.

• Quảng bá tự động trong mạng (mDNS), PC tìm thấy TV chỉ bằng một cú bấm; có thể nhập IP thủ công.
• Giải mã phần cứng H.264 / HEVC, 1080p60, tiếng Opus.
• Mã 4 số tuỳ chọn: bấm OK trên remote để bật; chỉ PC nhập đúng mã mới cast được.
• Tạm dừng / màn hình đen từ máy tính; bảng số liệu fps, bitrate, gói mất, độ trễ giải mã.
• Không cần tài khoản, không thu thập dữ liệu, không quảng cáo.

Cần cài phần mềm NANOCast cho PC để gửi hình (tải tại trang dự án).

### Description — English
**Title:** NANOCast — PC screen receiver

**Short:** Low-latency PC screen & audio receiver over your local network. Free.

**Full:**
NANOCast is a free, open-source receiver that turns your TV or Android box into a low-latency wireless display for your Windows, macOS or Linux PC on the same LAN. Built for classrooms and presentations.

• Automatic discovery (mDNS) — pick the TV on your PC; manual IP entry also works.
• Hardware H.264 / HEVC decoding up to 1080p60, Opus audio.
• Optional 4-digit code: press OK on the remote; only a PC with the right code can cast.
• Pause / blank screen from the PC; live stats (fps, bitrate, packet loss, decode time).
• No account, no data collection, no ads.

Requires the free NANOCast sender app on your PC.

## 5. Chính sách quyền riêng tư mẫu (đăng lên một URL công khai, ví dụ GitHub Pages)

> **NANOCast — Chính sách quyền riêng tư / Privacy Policy**
>
> NANOCast Receiver **không thu thập, lưu trữ hay chia sẻ dữ liệu cá nhân**. App không có tài khoản, không có quảng cáo, không dùng dịch vụ phân tích hay báo lỗi từ bên thứ ba và không gửi dữ liệu lên Internet.
>
> App chỉ nhận hình ảnh/âm thanh từ máy tính trong **cùng mạng nội bộ (LAN)** mà bạn chủ động cast tới, giải mã và hiển thị ngay; không ghi lại, không lưu. Mã 4 số tuỳ chọn và tên hiển thị chỉ lưu trên thiết bị. Quyền `INTERNET` / `ACCESS_NETWORK_STATE` chỉ để nhận dữ liệu qua mạng nội bộ.
>
> NANOCast Receiver does not collect, store or share personal data. It has no accounts, ads, analytics or crash reporting, and does not send data to the Internet. It only receives video/audio from a PC on the same local network that you choose to cast from, decodes and shows it immediately, and never records it. The optional 4-digit code is stored locally on the device only.
>
> Liên hệ / Contact: <email của bạn>. Mã nguồn / Source: <URL repo>.

## 6. Form trong Play Console

- **Data safety:** "No data collected / No data shared". Mã hoá khi truyền: không (LAN, không có TLS) — khai đúng là dữ liệu không rời thiết bị ra Internet.
- **Content rating:** hoàn thành bảng câu hỏi (không bạo lực, v.v.) → Everyone.
- **Target audience:** 13+ / mọi lứa tuổi trừ khi cần chương trình *Designed for Families* (không cần).
- **App access:** không cần đăng nhập.
- **Ads:** không có.
- **Form factor:** thêm *Android TV*, upload ảnh TV + banner. Có thể kiểm tra bằng *Pre-launch report* nếu có thiết bị.
- Chọn *Internal testing* trước (vài thiết bị TV thật), rồi *Closed*, rồi *Production*. Tài khoản developer mới (cá nhân) có thể bị yêu cầu thử nghiệm kín 12 người trong 14 ngày trước khi lên Production.
- Phí đăng ký tài khoản developer là một lần (25 USD).

## 7. Checklist trước khi upload

- [ ] `versionCode` tăng, `versionName` đúng.
- [ ] `bundlePlay` ký bằng upload key thật.
- [ ] Thử APK `assembleRelease` trên TV/emulator: mở app, nhận hình, bật mã 4 số.
- [ ] Ảnh chụp 1920×1080, icon 512, banner, mô tả VI + EN, URL chính sách riêng tư.
- [ ] Data safety + content rating đã điền.
