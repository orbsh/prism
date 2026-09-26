//! prism-protocol — the connection-plane wire contract (ADR-0017 §4).
//!
//! ONE frame shape, ONE field `ev`, used in BOTH directions: the
//! protocol does not encode direction, so dispatch cannot branch on it.
//! A client's `{"ev": "echo", ...}` and the server's
//! `{"ev": "echo.result", ...}` are the same struct. `emit`/`on` are
//! per-end implementation details (booth `@on` + `emit` on the aura
//! side; `ws.send` / `ws.on` on the client side).
//!
//! Two encodings over one model: CBOR (default) and JSON (debug path,
//! `?protocol=json` at handshake, fixed for the connection lifetime).
//! The codec choice is a per-connection property; the frame's field set
//! must stay shape-compatible across both (maintenance rule: one model,
//! two codec impls, a conformance round-trip over every variant).

use serde::{Deserialize, Serialize};

/// The one wire frame. `args` is the event payload — the same object a
/// realm event carries. Always present (null = empty payload): an
/// `Option` would round-trip `Some(null)` to `None` under serde's
/// null-as-absence rule, breaking the dual-codec equality contract for
/// the most common empty-payload shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub ev: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

impl Frame {
    pub fn new(ev: impl Into<String>, args: serde_json::Value) -> Self {
        Self { ev: ev.into(), args }
    }
}

/// The codec a connection fixed at handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Json,
    Cbor,
}

impl Codec {
    /// Parse the handshake query value (`?protocol=json`); anything
    /// else — including absence — is CBOR (the production default).
    pub fn from_query(v: Option<&str>) -> Self {
        match v {
            Some("json") => Codec::Json,
            _ => Codec::Cbor,
        }
    }

    pub fn encode(&self, frame: &Frame) -> Vec<u8> {
        match self {
            Codec::Json => serde_json::to_vec(frame).expect("Frame always serializes"),
            Codec::Cbor => {
                let mut buf = Vec::new();
                // serde_json::Value round-trips into CBOR unambiguously
                // (the model, not the text, is shared across codecs).
                ciborium::into_writer(frame, &mut buf).expect("Frame always serializes");
                buf
            }
        }
    }

    pub fn decode(&self, bytes: &[u8]) -> Result<Frame, ProtocolError> {
        match self {
            Codec::Json => {
                serde_json::from_slice(bytes).map_err(|e| ProtocolError(e.to_string()))
            }
            Codec::Cbor => {
                ciborium::from_reader(bytes).map_err(|e| ProtocolError(e.to_string()))
            }
        }
    }
}

#[derive(Debug)]
pub struct ProtocolError(pub String);

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protocol error: {}", self.0)
    }
}
impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dual-encoding maintenance contract (ADR-0017 §4): every frame
    /// shape round-trips through BOTH codecs byte-model-identically.
    /// This single test is the whole cost of keeping two codecs.
    #[test]
    fn every_frame_shape_round_trips_through_both_codecs() {
        let frames = vec![
            Frame::new("echo", serde_json::json!({"msg": "hi"})),
            Frame::new("echo", serde_json::json!(null)),
            Frame::new("order.created", serde_json::json!([1, 2, {"u": "a"}])),
            Frame { ev: "ping".into(), args: serde_json::Value::Null },
        ];
        for f in &frames {
            for codec in [Codec::Json, Codec::Cbor] {
                let back = codec.decode(&codec.encode(f)).expect("round trip");
                assert_eq!(&back, f, "{codec:?} round-trip of {f:?}");
            }
        }
    }

    /// Cross-codec reading: JSON bytes decoded by a debug client must
    /// equal the CBOR view of the same model (shape-compatibility, the
    /// rule protocol evolution must not break).
    #[test]
    fn json_and_cbor_views_agree() {
        let f = Frame::new("x", serde_json::json!({"a": 1, "b": [true, "s"]}));
        let json_bytes = Codec::Json.encode(&f);
        let via_cbor: Frame = Codec::Cbor.decode(&Codec::Cbor.encode(&f)).unwrap();
        let via_json: Frame = Codec::Json.decode(&json_bytes).unwrap();
        assert_eq!(via_cbor, via_json);
    }

    #[test]
    fn handshake_default_is_cbor() {
        assert_eq!(Codec::from_query(None), Codec::Cbor);
        assert_eq!(Codec::from_query(Some("binary")), Codec::Cbor);
        assert_eq!(Codec::from_query(Some("json")), Codec::Json);
    }
}
