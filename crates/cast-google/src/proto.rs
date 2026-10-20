//! `cast_channel.proto` (Chromium `components/cast_channel/proto/cast_channel.proto`),
//! written out by hand so no `protoc` / build script is needed. Only `CastMessage` is used:
//! the device-auth messages are optional for a sender that trusts the LAN.
//!
//! Framing on the TLS socket: u32 big-endian length, then the encoded message.

use anyhow::{bail, Result};
use prost::Message;

/// `CastMessage.ProtocolVersion`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum ProtocolVersion {
    Castv210 = 0,
}

/// `CastMessage.PayloadType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum PayloadType {
    String = 0,
    Binary = 1,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CastMessage {
    #[prost(enumeration = "ProtocolVersion", required, tag = "1")]
    pub protocol_version: i32,
    #[prost(string, required, tag = "2")]
    pub source_id: String,
    #[prost(string, required, tag = "3")]
    pub destination_id: String,
    #[prost(string, required, tag = "4")]
    pub namespace: String,
    #[prost(enumeration = "PayloadType", required, tag = "5")]
    pub payload_type: i32,
    #[prost(string, optional, tag = "6")]
    pub payload_utf8: Option<String>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub payload_binary: Option<Vec<u8>>,
}

/// Largest message accepted from the device (the spec limit is 64 KiB).
pub const MAX_MESSAGE: usize = 64 * 1024;

impl CastMessage {
    pub fn json(source: &str, destination: &str, namespace: &str, payload: &str) -> Self {
        Self {
            protocol_version: ProtocolVersion::Castv210 as i32,
            source_id: source.into(),
            destination_id: destination.into(),
            namespace: namespace.into(),
            payload_type: PayloadType::String as i32,
            payload_utf8: Some(payload.into()),
            payload_binary: None,
        }
    }

    /// Length-prefixed wire form.
    pub fn to_frame(&self) -> Vec<u8> {
        let body = self.encode_to_vec();
        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    pub fn from_body(body: &[u8]) -> Result<Self> {
        if body.len() > MAX_MESSAGE {
            bail!("Cast message too large ({} B)", body.len());
        }
        Ok(Self::decode(body)?)
    }

    /// The JSON payload, parsed (`None` for binary payloads or invalid JSON).
    pub fn payload_json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(self.payload_utf8.as_deref()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_matches_hand_encoding() {
        let m = CastMessage::json("sender-0", "receiver-0", "urn:x-cast:com.google.cast.tp.heartbeat", r#"{"type":"PING"}"#);
        let frame = m.to_frame();
        let len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        let body = &frame[4..];
        // field 1 varint 0, field 2 len "sender-0"
        assert_eq!(&body[..4], &[0x08, 0x00, 0x12, 8]);
        assert_eq!(&body[4..12], b"sender-0");
        assert_eq!(&body[12..14], &[0x1a, 10]);
        let back = CastMessage::from_body(body).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.payload_json().unwrap()["type"], "PING");
    }

    #[test]
    fn decodes_binary_payload() {
        let m = CastMessage {
            protocol_version: 0,
            source_id: "a".into(),
            destination_id: "b".into(),
            namespace: "n".into(),
            payload_type: PayloadType::Binary as i32,
            payload_utf8: None,
            payload_binary: Some(vec![1, 2, 3]),
        };
        let back = CastMessage::from_body(&m.to_frame()[4..]).unwrap();
        assert_eq!(back.payload_binary.as_deref(), Some(&[1u8, 2, 3][..]));
        assert!(back.payload_json().is_none());
    }
}
