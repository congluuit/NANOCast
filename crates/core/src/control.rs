//! Control channel (PLAN.md section 4): one TCP connection from the sender to the
//! receiver on CONTROL_PORT, one JSON object per line, message type in field "t".
//! It runs beside the UDP video stream and never changes the video packet format.
//!
//!   sender -> receiver: hello {ver, name, pin?} | start {w, h, fps, codec, audio} | audio {on}
//!                     | pause {on} | blank {on} | stop
//!   receiver -> sender: caps {ver, name, max_w, max_h, codecs, low_latency}
//!                     | error {code, message} | keyframe_request {reason}
//!                     | stats {fps, lost, decode_ms}
//!
//! `stats.lost` is the number of video packets lost since the previous `stats` (one
//! report per second). `codec` / `codecs` use the names of `protocol::VideoCodec`
//! ("h264", "hevc"). `pause {on}`: keep showing the last picture (the sender stops
//! capturing); `blank {on}`: show black. Both end with `{on: false}`, a new `start` or `stop`.
//!
//! PIN (optional, off by default): a receiver with a 4-digit code advertises TXT `pin=1`
//! and shows the code on screen. It answers a `hello` without the right `pin` with
//! `error {code: "bad_pin"}` (no `caps`), closes the connection, and accepts video/audio
//! UDP only from the address of the sender that gave the right code. Unknown message types decode as `Message::Unknown` and unknown
//! fields are ignored, so either side can add messages without breaking the other.
//!
//! The Kotlin receiver mirrors these messages in `ControlServer.kt`.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use crate::protocol::CONTROL_PORT;

/// `error.code` when `hello` had no PIN or the wrong one.
pub const ERR_BAD_PIN: &str = "bad_pin";

/// Longest accepted line; anything longer is treated as garbage.
pub const MAX_LINE: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    // sender -> receiver
    Hello {
        ver: String,
        name: String,
        /// The 4-digit code shown on the TV, when it asks for one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pin: Option<String>,
    },
    Start { w: u32, h: u32, fps: u32, codec: String, audio: bool },
    Audio { on: bool },
    /// Freeze the last picture on the TV (the sender stops capturing meanwhile).
    Pause { on: bool },
    /// Show black on the TV.
    Blank { on: bool },
    Stop,
    // receiver -> sender
    Caps { ver: String, name: String, max_w: u32, max_h: u32, codecs: Vec<String>, low_latency: bool },
    /// Refusal; `code` is machine-readable (`ERR_BAD_PIN`), `message` is for logs.
    Error {
        code: String,
        #[serde(default)]
        message: String,
    },
    KeyframeRequest { reason: String },
    Stats { fps: f64, lost: u64, decode_ms: f64 },
    /// Any message type this build does not know.
    #[serde(other)]
    Unknown,
}

/// Writes one message as a single JSON line and flushes.
pub fn write_msg<W: Write>(w: &mut W, msg: &Message) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Line-oriented message reader over any byte stream (usually a `TcpStream`).
pub struct ControlReader<R> {
    inner: BufReader<R>,
    line: String,
}

impl<R: Read> ControlReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner: BufReader::new(inner), line: String::new() }
    }

    /// Next message; `Ok(None)` at end of stream. Blank lines are skipped. A line that is
    /// not a valid message (bad JSON, too long) is an `InvalidData` error the caller may skip.
    pub fn read(&mut self) -> io::Result<Option<Message>> {
        loop {
            self.line.clear();
            let n = (&mut self.inner).take(MAX_LINE as u64 + 1).read_line(&mut self.line)?;
            if n == 0 {
                return Ok(None);
            }
            if n > MAX_LINE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "control line too long"));
            }
            let text = self.line.trim();
            if text.is_empty() {
                continue;
            }
            return serde_json::from_str(text)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad control message {text:?}: {e}")));
        }
    }
}

/// Opens the control connection to a receiver (Nagle off: messages are tiny and latency-sensitive).
pub fn connect(addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn all() -> Vec<Message> {
        vec![
            Message::Hello { ver: "0.1".into(), name: "Teacher PC".into(), pin: None },
            Message::Hello { ver: "0.1".into(), name: "Teacher PC".into(), pin: Some("0427".into()) },
            Message::Start { w: 1920, h: 1080, fps: 60, codec: "h264".into(), audio: false },
            Message::Audio { on: true },
            Message::Pause { on: true },
            Message::Blank { on: false },
            Message::Error { code: ERR_BAD_PIN.into(), message: "wrong code".into() },
            Message::Stop,
            Message::Caps {
                ver: "0.1".into(),
                name: "Phòng 12 TV".into(),
                max_w: 1920,
                max_h: 1080,
                codecs: vec!["h264".into()],
                low_latency: true,
            },
            Message::KeyframeRequest { reason: "packet_loss".into() },
            Message::Stats { fps: 59.5, lost: 3, decode_ms: 8.25 },
        ]
    }

    #[test]
    fn roundtrip_every_message() {
        for msg in all() {
            let mut buf = Vec::new();
            write_msg(&mut buf, &msg).unwrap();
            assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 1, "exactly one line: {msg:?}");
            assert_eq!(*buf.last().unwrap(), b'\n');
            let back = ControlReader::new(buf.as_slice()).read().unwrap().unwrap();
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn wire_format() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &Message::KeyframeRequest { reason: "decoder_backlog".into() }).unwrap();
        write_msg(&mut buf, &Message::Stop).unwrap();
        write_msg(&mut buf, &Message::Start { w: 1280, h: 720, fps: 30, codec: "h264".into(), audio: true }).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "{\"t\":\"keyframe_request\",\"reason\":\"decoder_backlog\"}\n\
             {\"t\":\"stop\"}\n\
             {\"t\":\"start\",\"w\":1280,\"h\":720,\"fps\":30,\"codec\":\"h264\",\"audio\":true}\n"
        );
    }

    #[test]
    fn tolerant_reading() {
        // What the Kotlin receiver (org.json) may produce: other key order, integer
        // floats, extra fields, CRLF, blank lines, unknown types.
        let input = "\r\n{\"lost\":0,\"fps\":60,\"t\":\"stats\",\"decode_ms\":7}\r\n\
                     {\"t\":\"stop\",\"extra\":1}\n\
                     {\"t\":\"zoom\",\"on\":true}\n\
                     not json\n\
                     {\"t\":\"audio\",\"on\":false}";
        let mut r = ControlReader::new(input.as_bytes());
        assert_eq!(r.read().unwrap(), Some(Message::Stats { fps: 60.0, lost: 0, decode_ms: 7.0 }));
        assert_eq!(r.read().unwrap(), Some(Message::Stop));
        assert_eq!(r.read().unwrap(), Some(Message::Unknown));
        assert_eq!(r.read().unwrap_err().kind(), io::ErrorKind::InvalidData);
        // Last line without a trailing newline still counts.
        assert_eq!(r.read().unwrap(), Some(Message::Audio { on: false }));
        assert_eq!(r.read().unwrap(), None);
    }

    #[test]
    fn hello_pin_and_pause_wire_format() {
        // No `pin` key at all when there is none: older receivers see the same hello as before.
        let mut buf = Vec::new();
        write_msg(&mut buf, &Message::Hello { ver: "0.1".into(), name: "pc".into(), pin: None }).unwrap();
        write_msg(&mut buf, &Message::Hello { ver: "0.1".into(), name: "pc".into(), pin: Some("1234".into()) }).unwrap();
        write_msg(&mut buf, &Message::Pause { on: true }).unwrap();
        write_msg(&mut buf, &Message::Blank { on: false }).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "{\"t\":\"hello\",\"ver\":\"0.1\",\"name\":\"pc\"}\n\
             {\"t\":\"hello\",\"ver\":\"0.1\",\"name\":\"pc\",\"pin\":\"1234\"}\n\
             {\"t\":\"pause\",\"on\":true}\n\
             {\"t\":\"blank\",\"on\":false}\n"
        );
        // org.json error without `message`.
        let mut r = ControlReader::new("{\"t\":\"error\",\"code\":\"bad_pin\"}\n".as_bytes());
        assert_eq!(r.read().unwrap(), Some(Message::Error { code: ERR_BAD_PIN.into(), message: String::new() }));
    }

    #[test]
    fn rejects_overlong_line() {
        let input = format!("{}\n", "x".repeat(MAX_LINE + 10));
        let err = ControlReader::new(input.as_bytes()).read().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = ControlReader::new(stream);
            let mut got = Vec::new();
            while let Some(msg) = reader.read().unwrap() {
                if let Message::Hello { ver, .. } = &msg {
                    let caps = Message::Caps {
                        ver: ver.clone(),
                        name: "tv".into(),
                        max_w: 1920,
                        max_h: 1080,
                        codecs: vec!["h264".into()],
                        low_latency: false,
                    };
                    write_msg(&mut writer, &caps).unwrap();
                }
                got.push(msg);
            }
            got
        });

        let mut stream = connect(addr, Duration::from_secs(2)).unwrap();
        let mut reader = ControlReader::new(stream.try_clone().unwrap());
        let hello = Message::Hello { ver: "0.1".into(), name: "pc".into(), pin: None };
        write_msg(&mut stream, &hello).unwrap();
        match reader.read().unwrap() {
            Some(Message::Caps { ver, name, .. }) => assert_eq!((ver.as_str(), name.as_str()), ("0.1", "tv")),
            other => panic!("expected caps, got {other:?}"),
        }
        write_msg(&mut stream, &Message::Stop).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(server.join().unwrap(), vec![hello, Message::Stop]);
    }
}
