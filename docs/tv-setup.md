# Cài và tối ưu TV để cast NANOCast

Hướng dẫn cho giáo viên / người quản trị phòng học. Mục tiêu: TV nhận hình từ PC với độ trễ thấp nhất.

> Tên menu thay đổi theo đời TV và phiên bản phần mềm; dưới đây là đường dẫn phổ biến. Nếu không thấy, tìm "Game", "Picture", "Motion", "Input" trong menu hình ảnh.

## 1. Giảm trễ hiển thị của chính TV

Phần lớn độ trễ "không giải thích được" nằm ở xử lý hình của TV (khử nhiễu, nội suy khung hình). Bật **Game Mode** (hoặc PC Mode) và tắt **motion smoothing**.

| Hãng / nền tảng | Cách làm |
|---|---|
| **Sony (Google TV / Android TV)** | Bấm *Quick Settings* (nút ⚙) → *Picture* → *Picture mode* = **Game** (hoặc **Graphics**). Settings → Display & Sound → Picture → Advanced settings → *Motionflow* = **Off**. Một số đời có *Game mode* riêng ở Settings → Picture → *Game / Graphics mode*; bật cả *Auto Low Latency Mode* (ALLM). Nếu app NANOCast chạy ở cổng Smart, vẫn nên chọn chế độ Game cho *Home/app* picture mode. |
| **TCL (Google TV / Roku TV)** | Settings → Picture → *Picture mode* = **Game**; Advanced → *Action Smoothing / MEMC* = **Off**; *Dynamic Contrast / Noise reduction* = Off. Google TV: Settings → Display & Sound → Picture. |
| **Xiaomi (Mi TV / Google TV)** | Settings → Picture → *Picture mode* = **Game**; *MEMC* = **Off**; Settings → Picture → Advanced → tắt *Dynamic contrast*, *Noise reduction*. Mi TV Box: chỉ cần chọn chế độ Game trên TV phía sau. |
| **Samsung (Tizen)** | Tizen không chạy được APK. Dùng **cách khác**: cắm một Android TV box / Chromecast with Google TV / mini PC (chạy `nanocast-receiver`) vào HDMI. Trên TV: Settings → General → *External Device Manager* → **Game Mode Settings** → *Game Mode* = **On** (hoặc *Auto*); đổi tên Input thành **PC / Game console**. Tắt *Auto Motion Plus* (Picture → Expert Settings → *Auto Motion Plus* = Off). |
| **LG (webOS)** | webOS không chạy APK. Cắm box Android / mini PC vào HDMI, rồi: Settings → Picture → *Picture Mode Settings* → **Game** (hoặc đổi nhãn Input thành **PC** / **Console** để bật ALLM); *TruMotion* = **Off**; *Noise Reduction* = Off. Đời cũ: Settings → Picture → Picture Mode → *Game*. |
| **Android box / Fire TV** | Box (Mi Box, Nvidia Shield, Onn, Fire TV Stick...) cắm HDMI vào TV: bật Game Mode / ALLM **trên TV** ở cổng HDMI đó (xem các dòng trên). Trên box: Settings → Display → bật *Auto Low Latency Mode* nếu có; tắt *Match content frame rate* (tránh TV đổi chế độ giữa chừng, nháy đen); độ phân giải đặt 1080p60. Fire TV: Settings → Display & Sounds → Display → *Video Resolution* 1080p 60 Hz. |

Mẹo nhanh: nếu không thấy menu Game, đổi **tên input HDMI** thành "PC" hoặc "Game console" — nhiều TV tự bật chế độ trễ thấp.

## 2. Bật Developer options và Network debugging (để cài qua mạng)

Cần cho mục 3A (nút *Cài app lên TV* trong app máy tính) và 3B (`adb`). Cách không cần PC: xem mục 3C.

**Android TV / Google TV (Sony, TCL, Xiaomi, Philips, Nvidia Shield...):**
1. Settings → *System* (hoặc *Device Preferences*) → **About** → bấm **Build** (hoặc *Android TV OS build*) **7 lần** cho đến khi hiện "You are now a developer".
2. Quay lại → **Developer options** (trong System / Device Preferences).
3. Bật **USB debugging**. Nếu có **Network debugging** thì bật luôn. TV Android 11+ có thể bật **Wireless debugging** (Gỡ lỗi không dây) thay vào: lần đầu phải ghép nối bằng mã 6 số (mục 3A làm được việc này).
4. Xem IP của TV: Settings → Network & Internet → tên mạng đang kết nối (hoặc overlay của app NANOCast hiện IP).

**Fire TV:** Settings → My Fire TV → About → bấm tên thiết bị 7 lần → Developer Options → bật **ADB debugging** và **Apps from Unknown Sources**.

**Xiaomi Mi TV (PatchWall):** Settings → Device Preferences → About → bấm *Build* 7 lần → Developer options → *USB debugging*.

## 3. Cài app NANOCast lên TV / box Android

### A. Từ app NANOCast trên máy tính, qua Wi-Fi (khuyên dùng)
1. Trên TV bật **Gỡ lỗi USB** (mục 2), hoặc **Gỡ lỗi không dây** trên TV Android 11+. Máy tính và TV cùng mạng.
2. Mở `nanocast.exe`: TV có Chromecast hiện thành thẻ *Không cần cài, hơi trễ* với nút **Cài app lên TV** → **Bắt đầu cài**.
3. TV hỏi *Cho phép gỡ lỗi?* → tick **Luôn cho phép từ máy tính này** → **Cho phép** (trong 90 giây). TV nào hỏi Play Protect thì chọn *Vẫn cài đặt*.
   - TV chỉ bật *Gỡ lỗi không dây* (Android 11+): hộp thoại báo **Ghép nối TV bằng mã** và hiện ô nhập mã. Trên TV vào *Tuỳ chọn cho nhà phát triển → Gỡ lỗi không dây → Ghép nối thiết bị bằng mã ghép nối*, để bảng đó mở, gõ mã 6 số vào ô rồi bấm **Ghép nối và cài**. App tự tìm cổng ghép nối và cổng gỡ lỗi của TV qua mDNS (`_adb-tls-pairing._tcp`, `_adb-tls-connect._tcp`); không tìm thấy thì mở *Nhập địa chỉ thủ công* và gõ "Địa chỉ IP và cổng" TV hiện (trong bảng ghép nối, và ở màn hình Gỡ lỗi không dây). Ghép nối một lần là đủ: lần sau TV nhận ra máy tính, không hỏi mã nữa (TV liệt kê máy là `nanocast@<tên máy>` trong *Thiết bị đã ghép nối*).
4. App tự làm tiếp: tải APK đúng phiên bản (lưu cache cho TV sau), cài, mở app NANOCast trên TV, rồi **tắt gỡ lỗi** (`adb_enabled = 0`) cho an toàn. Vài giây sau TV hiện lại với nhãn *Nhanh nhất*.

Ghi chú:
- Thử ADB qua cổng **5555** trước (kiểu cũ, có trên Android TV / Google TV / Fire TV / box Android), rồi tới cổng *Wireless debugging* TV quảng bá qua mDNS. Với *Wireless debugging*, app tự ghép nối (SPAKE2 trong TLS 1.3, như `adb pair`) rồi nói ADB trong TLS với chứng chỉ làm từ chính khoá RSA dưới đây — không cần `adb.exe`. Mã sai / hết hạn → app báo *Mã ghép nối không đúng*, mở lại bảng ghép nối trên TV và nhập mã mới. Cuối cùng app tắt cả *Gỡ lỗi không dây* (`adb_wifi_enabled = 0`).
- Khoá RSA của máy tính nằm ở `%APPDATA%\NANOCast\adbkey.pem` (TV nhớ khoá này khi chọn *Luôn cho phép*). APK tải về nằm ở `%LOCALAPPDATA%\NANOCast\` (Windows); muốn dùng file APK riêng thì đặt biến môi trường `NANOCAST_APK=<đường dẫn>` hoặc để file `NANOCast-Receiver-<version>-androidtv.apk` cạnh `nanocast.exe`.
- TV đã có bản NANOCast khác chữ ký (ví dụ bản Play Store) thì app gỡ bản đó rồi cài bản này.
- Lần sau muốn cài lại / cập nhật: bật lại *Gỡ lỗi USB* hoặc *Gỡ lỗi không dây* (Developer options vẫn còn, không phải bấm 7 lần nữa; TV đã ghép nối thì không hỏi mã lại).

### B. Bằng `adb` (cần PC cùng mạng)
```bash
adb connect <ip-tv>:5555            # Android 11+ wireless debugging: adb pair <ip>:<port> rồi adb connect <ip>:<port>
adb install -r nanocast-receiver.apk
adb shell am start -n com.redharegames.nanocast.receiver/.MainActivity
```
TV hiện hộp thoại "Allow USB debugging?" lần đầu → chọn *Always allow*.

### C. Không cần PC (tải APK ngay trên TV)
1. Bật cài app ngoài Store: Settings → Apps (Security & restrictions) → **Install unknown apps** → cho phép trình duyệt hoặc **Downloader**. Fire TV: Developer Options → *Apps from Unknown Sources*.
2. Cài app **Downloader** (by AFTVnews) từ Google Play / Amazon Appstore trên TV.
3. Mở Downloader → ô URL → nhập đường dẫn tải APK (ví dụ link Releases của dự án trên GitHub, hoặc link rút gọn bạn tạo) → *Go* → *Install*.
4. Hoặc dùng trình duyệt của TV (Puffin TV Browser / trình duyệt trên box) tải file `.apk` rồi mở từ *Downloads*.
5. Hoặc copy APK vào USB, cắm vào TV, mở bằng file manager (File Commander, X-plore) → *Install*.
6. Sau khi cài, bật lại cài đặt "unknown apps" về tắt nếu muốn.

Mở app *NANOCast* trên launcher (nếu không thấy, tìm trong *Apps* / *Cài đặt → Ứng dụng*). Góc trái hiện IP của TV; bấm **OK** trên remote để bật/tắt mã 4 số.

Nếu bản trên Play Store đã phát hành: tìm "NANOCast" trên Google Play của TV (xem [play-store.md](play-store.md)).

## 4. Mẹo mạng

- **Ethernet cho TV/box** nếu có cổng LAN; Wi-Fi là nguồn trễ và mất gói lớn nhất. PC cast nên cũng dùng dây nếu được.
- Nếu phải dùng Wi-Fi: **5 GHz** (hoặc 6 GHz), đặt TV gần router, kênh ít nhiễu, **tắt chế độ tiết kiệm điện Wi-Fi** của TV. Tránh 2,4 GHz (trễ và giật khi nhiều thiết bị).
- Bitrate thích nghi sẽ tự hạ khi mất gói (ghi ở overlay/ GUI); nếu hình vẫn vỡ → thử hạ bitrate trần trong cài đặt PC.
- **PC và TV phải cùng mạng con** (cùng dải 192.168.x.x). Mạng khách (Guest) và Wi-Fi mesh có thể khác dải.
- **Tắt AP isolation / Client isolation / Wireless isolation** trên router (hay bị bật ở mạng Wi-Fi khách, mạng trường học), nếu không các thiết bị Wi-Fi không nhìn thấy nhau.
- **Tường lửa Windows**: lần đầu chạy `nanocast.exe`, chọn *Allow* cho cả Private network. Cổng dùng: UDP 48322 (video), 48324 (âm thanh), TCP 48323 (điều khiển), mDNS UDP 5353.
- **mDNS bị chặn** (mạng trường học, VLAN khác nhau, switch lọc multicast): PC không thấy TV trong danh sách. Trong `nanocast.exe` dùng ô **nhập IP thủ công** (IP xem trên overlay của TV) và cast như thường.
- Không bật VPN trên PC khi cast (VPN thường đổi route LAN).
- Tránh tải nặng khác (tải file, họp video) qua cùng Wi-Fi trong lúc cast.

## 5. Checklist đo độ trễ glass-to-glass bằng điện thoại

Đo tổng trễ từ PC đến mặt kính TV, không cần thiết bị đặc biệt.

1. Điện thoại quay **slow-motion 240 fps** (hoặc 120 fps; càng cao càng chính xác). Cố định tripod, bật đèn đủ sáng.
2. Trên PC mở bộ đếm mili-giây (ví dụ trang *time.is/milliseconds* hoặc app stopwatch có chữ to), đặt cửa sổ ở **giữa màn hình**, bật cast **toàn màn hình** hoặc cast đúng cửa sổ đó.
3. Đặt **màn hình PC và TV cạnh nhau**, cùng vào một khung hình của điện thoại.
4. Quay 10–20 s. Tua từng khung: đọc số trên PC (T_pc) và số trên TV (T_tv) trong **cùng một khung** → trễ = T_pc − T_tv.
5. Lấy ít nhất 10 mẫu, ghi trung bình và lớn nhất. Màn hình PC 60 Hz cũng có trễ ~8–16 ms cần trừ vào kết quả nếu muốn chính xác (hoặc bỏ qua, chỉ so sánh tương đối).
6. Ghi lại điều kiện: Ethernet/Wi-Fi 5 GHz, codec (H.264/HEVC), bitrate, chế độ TV (Game / Standard), độ phân giải. Thử **Game Mode bật/tắt** — chênh lệch thường 50–100 ms.
7. Mục tiêu: PC → TV dưới ~100 ms qua Ethernet, dưới ~150 ms qua Wi-Fi 5 GHz tốt.
8. Overlay trên TV hiện fps, Mbps, gói mất, decode ms: nếu trễ cao mà decode ms thấp → thủ phạm là mạng hoặc xử lý hình TV (kiểm tra mục 1 và 4).

Nếu muốn so sánh thêm với số liệu dự án, xem [benchmark.md](benchmark.md).
