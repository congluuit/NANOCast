# Cast tới Chromecast có sẵn trong TV (Phase 4, crate `nanocast-cast`)

Đường dự phòng khẩn cấp (cáp HDMI hỏng, TV chưa cài receiver NANOCast): TV nào có
**Chromecast built-in / Google TV / Android TV** hoặc cắm dongle Chromecast đều nhận được hình,
**không phải cài gì trên TV**. Trễ hơn receiver riêng của NANOCast, nhưng chạy ngay.

So với nút "Truyền màn hình" có sẵn trong Chrome: NANOCast gửi **1080p 60 khung/s** (Chrome:
1080p30 hoặc thấp hơn), bitrate mặc định **12 Mbps** (Chrome khoảng 5 Mbps), dùng encoder phần
cứng không có B-frame, chia gói RTP đều theo tốc độ đường truyền (không dồn cả khung một lúc),
và tự giữ hình ổn định khi màn hình đứng yên (khung lặp mỗi 250 ms). Tất cả đổi được qua `CastConfig`.

## Cách hoạt động

```
nanocast.exe ──mDNS _googlecast._tcp──▶ tìm TV (TXT fn = tên, md = model, id)
     │
     ├─ TLS tới TV:8009, khung = u32 big-endian + protobuf CastMessage (cast_channel.proto)
     │    tp.connection CONNECT/CLOSE · tp.heartbeat PING/PONG 5 s · receiver LAUNCH/STOP/GET_STATUS
     │
     ├─ Đường B (mặc định): Cast Streaming, giống Chrome mirroring
     │    LAUNCH 0F5096E8 → OFFER (H.264 + Opus, SSRC, aesKey/aesIvMask) → ANSWER (cổng UDP)
     │    khung mã hoá AES-128-CTR → gói RTP kiểu Cast → UDP tới TV
     │    RTCP: PC gửi Sender Report 0,5 s; TV gửi Cast feedback (ACK/NACK → gửi lại gói), PLI → keyframe
     │
     └─ Đường A (dự phòng): Default Media Receiver (CC1AD845)
          PC mở HTTP :47900, phục vụ /live.mp4 = fMP4 live (ftyp+moov, rồi moof+mdat từng khung)
          LOAD contentId=http://<ip-PC>:47900/live.mp4, video/mp4, streamType LIVE
```

- `CastSession::start` thử đường B; nếu TV không mở được ứng dụng mirroring, không trả
  ANSWER, hoặc không gửi RTCP nào trong 5 s sau khi nhận hình thì dừng ứng dụng đó và chuyển
  sang đường A. Ép một đường bằng `NANOCAST_CAST_MODE=mirroring` hoặc `media`.
- Hình: capture WGC + chuyển NV12 trên GPU + Media Foundation H.264 (Main, CBR, không B-frame)
  lấy từ `nanocast-win`. Keyframe: đường A ít nhất mỗi 1 s (trình phát cần keyframe để bắt đầu);
  đường B mỗi 2 s, hoặc ngay khi TV báo mất hình (PLI, checkpoint đứng yên).
- Tiếng: WASAPI loopback (cpal). Thời điểm của từng khối âm thanh do đồng hồ của phiên quyết
  định (không theo card âm thanh), nên dòng thời gian không bao giờ dừng khi máy im lặng; tắt
  tiếng = gửi khối im lặng. Đường A: AAC-LC 48 kHz stereo 160 kbps (bộ mã hoá AAC có sẵn trong
  Windows qua Media Foundation). Đường B: Opus 20 ms 128 kbps.
- fMP4 do NANOCast tự ghép (`fmp4.rs`, không cần ffmpeg): `avc1` + `avcC` lấy SPS/PPS từ keyframe
  đầu tiên, `mp4a` + `esds`, `mvex/trex`; mỗi khung hình là một fragment `moof` (`tfhd`
  default-base-is-moof, `tfdt` v1 = thời điểm capture, `trun` có duration/size/flags) + `mdat`
  AVCC; tiếng 3 khung AAC một fragment. HTTP chunked, không Content-Length. Khách mới nhận init
  segment rồi bắt đầu từ keyframe kế tiếp; khách chậm (hàng đợi đầy) nhảy tới keyframe sau.
- `stats()`: fps, Mbps, đường đang dùng, `latency_ms` = encode + (độ trễ phát mà TV báo khi
  mirroring, hoặc khoảng cách giữa khung mới nhất và vị trí trình phát khi dùng đường A, hỏi
  `GET_STATUS` mỗi 2 s), `connected`, thông báo tiếng Việt.

## Hạn chế

- **Đường A trễ khoảng 2–3 s**: trình phát (Chrome trên PC, trình phát Cast trên TV) tự giữ bộ đệm
  rồi phát đúng tốc độ thực, nên độ trễ đứng yên ở mức bộ đệm lúc bắt đầu. Không seek được để
  đuổi kịp: luồng live không có chỉ mục, seek làm trình phát rối (đã thử trong Chrome).
- **Đường A cần TV kết nối ngược về PC** (HTTP cổng TCP 47900, hoặc cổng ngẫu nhiên nếu 47900
  bận). Wi-Fi để chế độ **Public** thì Tường lửa Windows chặn: phải đặt mạng là **Riêng tư
  (Private)** và bấm "Cho phép" khi Windows hỏi lần đầu. Lỗi này được báo rõ ("TV không tải
  được hình từ máy tính… Tường lửa Windows"). Đường B không cần mở cổng: TV chỉ trả RTCP về
  đúng cổng UDP mà PC đã gửi đi.
- Phải có keyframe định kỳ (đường A 1 s): bitrate tốn hơn một chút so với receiver NANOCast (ở
  đó keyframe chỉ được gửi khi TV yêu cầu).
- Cast session bắt đầu với `audio: false` thì đường A không có track âm thanh; bật lại giữa
  chừng không có tác dụng cho tới lần cast sau. Bắt đầu với âm thanh rồi tắt thì ổn (gửi im lặng).
- Chỉ có trên Windows (cần `nanocast-win`); macOS/Linux trả lỗi rõ ràng.
- Không xác thực thiết bị (device auth): bỏ qua kiểm tra chứng chỉ TLS tự ký của TV. Trên mạng
  LAN, kẻ giả làm "TV" cũng chỉ nhận được hình mà người dùng đã chọn cast tới nó.
- Loa thông minh (Nest Mini…) xuất hiện trong danh sách nhưng không có màn hình: mirroring báo
  `LAUNCH_ERROR`, đường A báo `LOAD_FAILED`. GUI nên lọc theo model hoặc hiện thông báo đó.

## Đã / chưa kiểm chứng (2026-10-20)

Đã kiểm chứng:
- `cargo build --release -p nanocast-cast`, `cargo test -p nanocast-cast` (17 test: protobuf
  khớp mã hoá tay, TXT mDNS, cấu trúc hộp init segment/fragment và data offset, Annex B → AVCC,
  ASC AAC, HTTP chunked bắt đầu từ keyframe, OFFER/ANSWER, AES-CTR khớp vector chuẩn, gói RTP
  Cast, Sender Report/NTP, phân tích Cast feedback + PLI, **loopback UDP**: ráp gói, giải mã ra
  đúng khung, NACK → gửi lại đúng gói), `cargo clippy` sạch, cả workspace build release.
- **Đường A trong Chrome trên PC** (`cargo run --release -p nanocast-cast --example cast_preview`,
  mở `http://127.0.0.1:47901/`): 1920×1080, 60 khung/s khi màn hình chuyển động, 8–12 Mbps,
  hình + tiếng AAC đều được decode, độ trễ bộ đệm **2,28 s ổn định suốt hơn 100 s, 0 khung rớt**;
  `currentTime` của Chrome giữ đúng `tfdt` (không quy về 0), khớp cách ước lượng trễ.
- **Kênh Cast v2 trên thiết bị thật** (Google Nest Mini cùng mạng, `cast_probe --status`, `cast_tv`):
  mDNS tìm thấy, TLS 1.2, CONNECT, heartbeat, GET_STATUS, LAUNCH Default Media Receiver, LOAD
  được nhận; LAUNCH mirroring bị từ chối (`SYSTEM_ERROR`, loa không có màn hình) → **tự chuyển
  sang đường A đúng thiết kế**; LOAD_FAILED vì Wi-Fi của máy thử ở chế độ Public (tường lửa
  chặn cổng 47900) → báo lỗi tường lửa như mong muốn; STOP + CLOSE khi dừng.

Chưa kiểm chứng (không có TV Chromecast/Google TV bật trên mạng lúc làm):
- Hình thật trên TV qua cả hai đường; độ trễ đường A trên TV; đường B (mirroring) trên thiết bị thật:
  định dạng OFFER (tên trường theo openscreen `offer_messages.cc`), payload type 96/127, nonce
  AES, có cần RTCP XR hay `rtpExtensions` không, TV có nhận 1080p60 không (nếu TV chỉ nhận 30
  khung/s thì có thể đặt `CastConfig::fps = 30`).
- Âm thanh thật trên TV (đường A: AAC trong fMP4; đường B: Opus).
- Chạy dài (90 phút), Wi-Fi yếu (NACK/keyframe khi mất gói thật).

## Thử nhanh

```bash
cargo run --release -p nanocast-cast --example cast_probe -- 5 --status
```

```bash
cargo run --release -p nanocast-cast --example cast_preview -- 120
```

```bash
cargo run --release -p nanocast-cast --example cast_tv -- "Tên TV" 60
```

Biến môi trường: `NANOCAST_CAST_MODE=mirroring|media` (ép đường), `NANOCAST_CAST_DELAY_MS`
(độ trễ phát đề nghị cho mirroring, mặc định 300), `NANOCAST_CAST_MUTE=1` (bắt đầu với âm thanh
tắt, để thử trên loa), `NANOCAST_CONVERT=cpu` (chuyển NV12 trên CPU).
