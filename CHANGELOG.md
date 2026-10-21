# Changelog

Mọi thay đổi đáng chú ý của NANOCast. Số phiên bản dùng chung cho app máy tính và app TV (`version` trong `Cargo.toml`, tag `v<version>`).
Mục của mỗi phiên bản được dùng làm nội dung GitHub Release (`.github/workflows/release.yml`).

## [0.2.0] - 2026-10-21

### Mới
- **Cài app lên TV qua Wi-Fi ngay từ app máy tính, không cần USB, không cần `adb.exe`.** TV có Chromecast hiện nút **Cài app lên TV** trên thẻ của nó; TV / box Android khác (Fire TV, box không có Chromecast, mạng chặn mDNS) dùng link **Cài app lên TV qua Wi-Fi (gõ địa chỉ IP)**. Hộp thoại đi từng bước có dấu tích: tải app → kết nối → *Cho phép* trên TV → cài → mở app → **tắt gỡ lỗi** lại cho an toàn. Lỗi báo bằng lời kèm cách sửa và nút *Thử lại*.
  - ADB client tự viết, nói thẳng với TV qua cổng 5555; khoá RSA của máy lưu ở `%APPDATA%\NANOCast\adbkey.pem` (TV nhớ khi chọn *Luôn cho phép*).
  - TV Android 11+ chỉ bật **Gỡ lỗi không dây**: nhập mã ghép nối 6 số trong app (tự tìm cổng ghép nối / cổng gỡ lỗi qua mDNS, có ô nhập địa chỉ thủ công), ghép một lần là đủ.
  - Tự tải đúng APK của phiên bản từ GitHub Releases (có cache); TV đang có bản khác chữ ký thì gỡ bản cũ rồi cài lại.
  - Ô IP nhận kiểu gõ dễ dãi (`192,168,1,50`, có khoảng trắng, `:5555`, `http://`…) và nhớ IP lần trước.
- **Tự cập nhật**, vài giờ kiểm tra GitHub Releases một lần:
  - **Windows (bản cài)**: tự tải bản mới; bấm **Cập nhật** trên thông báo (app đóng, cài, tự mở lại) hoặc để bản mới tự cài khi tắt NANOCast. Bản cài cho mọi người dùng (Program Files) cần bấm Cập nhật để hỏi quyền admin.
  - **Windows (bản portable)**: tự thay `nanocast.exe` bằng bản mới.
  - **macOS**: báo có bản mới và mở trang tải `.dmg`.
  - Bật / tắt và *Kiểm tra ngay* trong **Cài đặt → Cập nhật**.
  - **App trên TV**: tự tải APK mới; lúc không có máy nào đang cast thì hiện hộp thoại cài của Android (Android 12+ từ lần thứ hai cài luôn không hỏi). Bỏ qua thì bấm **▲** trên điều khiển để cài sau.
- File phát hành mới `NANOCast-<version>-portable-win64.exe`: chính `nanocast.exe` của bản portable, không nén (bản portable tự cập nhật bằng file này).

### Thay đổi
- APK trên GitHub Releases giờ là bản **release ký bằng khoá cố định** của dự án (trước là bản debug), để TV cập nhật đè được. Bản Google Play bỏ quyền tự cài APK (Play tự cập nhật).
- Số phiên bản APK lấy từ `Cargo.toml`, luôn trùng với app máy tính.
- Hướng dẫn cài app TV ([docs/tv-setup.md](https://github.com/congluuit/NANOCast/blob/main/docs/tv-setup.md)) viết lại, cách cài từ app máy tính lên đầu.

### Lưu ý khi nâng cấp
- **TV đang chạy bản 0.1.0** (ký bằng khoá khác) không tự cập nhật được: cài lại **một lần** bằng nút **Cài app lên TV** trên máy tính (app tự gỡ bản cũ). Từ 0.2.0 trở đi TV tự cập nhật.
- App máy tính 0.1.0 chưa có tự cập nhật: tải `NANOCast-Setup-0.2.0.exe` (hoặc bản portable) cài đè một lần.

### Chưa kiểm chứng
- Cài app TV qua Wi-Fi mới thử trên emulator Android (cổng 5555 và Gỡ lỗi không dây), chưa thử trên TV thật.
- Bản macOS `.dmg` vẫn chưa chạy thử trên máy Mac thật.

## [0.1.0] - 2026-10-20

Bản phát hành đầu tiên.

- App máy tính `nanocast.exe` (egui, tiếng Việt / English): tìm TV trong mạng, chiếu **toàn màn hình** hoặc **một ứng dụng**, kèm hoặc không kèm âm thanh.
- Windows: chụp màn hình WGC, mã hoá phần cứng Media Foundation H.264/HEVC, Full HD 60 fps qua UDP, bitrate thích nghi, âm thanh WASAPI → Opus.
- App TV *NANOCast Receiver* cho Android TV / Google TV / Fire TV / box Android; receiver desktop cho PC cũ / Raspberry Pi cắm HDMI.
- Chiếu lên TV có **Chromecast built-in** không cần cài app (trễ hơn).
- Tạm dừng, màn hình đen, mã 4 số trên TV, tự kết nối lại khi rớt mạng, thông báo lỗi dễ hiểu, hướng dẫn lần đầu, khay hệ thống và phím tắt Ctrl+Shift+P.
- Bộ cài Windows (không cần admin), bản portable, APK cho TV, `.dmg` universal cho macOS 12.3+ (chưa kiểm chứng).

[0.2.0]: https://github.com/congluuit/NANOCast/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/congluuit/NANOCast/releases/tag/v0.1.0
