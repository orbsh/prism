//! prism-protocol — the connection-plane wire contract (ADR-0017 §4).
//!
//! ONE frame shape, ONE field `ev`, used in BOTH directions: the
//! protocol does not encode direction, so dispatch cannot branch on it.
//! A client's `{"ev": "echo", ...}` and the server's
//! `{"ev": "echo.result", ...}` are the same struct. `emit`/`on` are
//! per-end implementation details (booth `@on` + `emit` on the aura
//! side; `ws.send` / `ws.on` on the client side).
//!
//! Two encodings over one model: CBOR and JSON. WHICH ONE a connection speaks is
//! negotiated ONCE, at the handshake, and fixed for the connection's lifetime
//! (ADR-0017 §4). The ladder, highest rung first:
//!
//! 1. the `Sec-WebSocket-Protocol` the client offered — `accrete-json` or
//!    `accrete-cbor` (echoed back, so the client can read its own choice);
//! 2. `?codec=json|cbor`;
//! 3. the legacy `?protocol=json|cbor`;
//! 4. absent everywhere = JSON.
//!
//! A rung that names something we do not speak is a 4xx refusal, never a silent
//! fallback: a client that asked for a wire it did not get has no way to notice,
//! and would read every frame thereafter as garbage. `PRISM_CODECS` is the
//! deployment's pin (one rung above all four): a choice outside it is refused the
//! same way.
//!
//! The frame's field set must stay shape-compatible across both (maintenance
//! rule: one model, two codec impls, a conformance round-trip over every variant).


// Unit tests may use the unwrap/panic vocabulary the production targets are
// denied (a panicking test is a failing test). `cfg(test)` is on only for this
// crate's own test build; `tests/` crates set the same line at their own root.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn))]
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

/// The subprotocol prefix both ends spell: `accrete-json`, `accrete-cbor`.
///
/// The prefix is the product's name on purpose, not the codec's: `Sec-WebSocket-Protocol`
/// is a list of APPLICATION protocols, and offering `json` alone would claim a registered
/// protocol nobody owns. A value with this prefix that we do not parse is OUR namespace and
/// a stranger's word — refused, never skipped (see [`negotiate`]).
pub const SUBPROTOCOL_PREFIX: &str = "accrete-";

impl Codec {
    /// The wire spelling — the one a negotiation stamps into a turn's context, a log line
    /// and a `?codec=` value all share.
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::Json => "json",
            Codec::Cbor => "cbor",
        }
    }

    /// The `Sec-WebSocket-Protocol` value for this codec, and the only spelling the ladder's
    /// first rung accepts.
    pub fn subprotocol(self) -> &'static str {
        match self {
            Codec::Json => "accrete-json",
            Codec::Cbor => "accrete-cbor",
        }
    }

    /// Parse a `Sec-WebSocket-Protocol` value: `Some` only for one of ours.
    ///
    /// `None` means "not a codec of ours" — the caller decides whether that is somebody
    /// else's protocol in the list (skip it) or an `accrete-` word we do not speak (refuse).
    pub fn from_subprotocol(v: &str) -> Option<Self> {
        match v.strip_prefix(SUBPROTOCOL_PREFIX)? {
            "json" => Some(Codec::Json),
            "cbor" => Some(Codec::Cbor),
            _ => None,
        }
    }

    /// Parse a `?codec=` / `?protocol=` value. Exact spellings only: these strings are a
    /// contract with a client's configuration, not a place to guess what was meant.
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "json" => Some(Codec::Json),
            "cbor" => Some(Codec::Cbor),
            _ => None,
        }
    }

    /// Parse the handshake query value (`?protocol=json`); anything
    /// else — including absence — is CBOR (the production default).
    ///
    /// The LEGACY rung only: kept because a client that already sends `?protocol=json` must
    /// keep working, and its historical default was CBOR. New clients use [`negotiate`].
    pub fn from_query(v: Option<&str>) -> Self {
        match v {
            Some("json") => Codec::Json,
            _ => Codec::Cbor,
        }
    }

    /// `Frame` is a plain tagged struct of strings and `serde_json::Value` —
    /// every shape serde can fail on (non-string map keys, NaN floats) is
    /// unrepresentable in it, so the codec is infallible by construction. A
    /// `Result` here would only push an unreachable error onto every caller.
    #[allow(clippy::expect_used)]
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

/// Why a handshake was refused. Every variant is a 4xx at the venue: the ladder never
/// falls through to a lower rung because a higher one named something we cannot speak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The client offered an `accrete-` subprotocol we do not implement (`accrete-msgpack`).
    UnknownSubprotocol(String),
    /// `?codec=` or `?protocol=` carried a value that is not `json`/`cbor`.
    BadQuery { name: &'static str, value: String },
    /// A valid choice, refused by the deployment's `PRISM_CODECS` pin.
    Pinned { codec: Codec, allowed: &'static [Codec] },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::UnknownSubprotocol(v) => write!(
                f,
                "subprotocol `{v}` is not one of {SUBPROTOCOL_PREFIX}json|{SUBPROTOCOL_PREFIX}cbor"
            ),
            Refusal::BadQuery { name, value } => {
                write!(f, "`{name}=` must be `json` or `cbor`, not `{value}`")
            }
            Refusal::Pinned { codec, allowed } => {
                let allowed: Vec<&str> = allowed.iter().map(|c| c.as_str()).collect();
                write!(
                    f,
                    "this deployment serves only [{}] (PRISM_CODECS), so `{}` is refused",
                    allowed.join(","),
                    codec.as_str()
                )
            }
        }
    }
}

impl std::error::Error for Refusal {}

/// The outcome of the ladder: the codec, and the subprotocol to ECHO when the client's offer
/// won (a client that offered one reads the answer off the handshake response, so silence
/// would leave it guessing which of its offers was taken).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    pub codec: Codec,
    pub echo: Option<&'static str>,
}

/// THE LADDER, in one function: subprotocol → `?codec=` → legacy `?protocol=` → json.
///
/// `offered` is the client's `Sec-WebSocket-Protocol` list in its own order (the first
/// value we understand wins — a client that prefers CBOR says so by listing it first);
/// `codec`/`protocol` are the raw query values, `None` when absent; `allowed` is the
/// deployment's pin ([`Pinned`]).
///
/// The precedence is the specification: a client that both offers `accrete-cbor` and asks
/// `?codec=json` gets CBOR, because the rung it expressed most specifically to the transport
/// is the subprotocol. A rung that names something unknown REFUSES: falling through would
/// hand a peer a wire it cannot decode while the handshake still says 101.
pub fn negotiate(
    offered: &[&str],
    codec: Option<&str>,
    protocol: Option<&str>,
    allowed: &'static [Codec],
) -> Result<Negotiated, Refusal> {
    let mut unknown: Option<&str> = None;
    let mut chosen: Option<Codec> = None;
    for value in offered {
        if let Some(parsed) = Codec::from_subprotocol(value) {
            chosen = Some(parsed);
            break;
        }
        if unknown.is_none() && value.starts_with(SUBPROTOCOL_PREFIX) {
            unknown = Some(value);
        }
    }
    let (codec, echo) = match chosen {
        Some(codec) => (codec, Some(codec.subprotocol())),
        None => {
            if let Some(v) = unknown {
                return Err(Refusal::UnknownSubprotocol(v.to_string()));
            }
            match (codec_param(codec)?, protocol_param(protocol)?) {
                (Some(c), _) => (c, None),
                (None, Some(c)) => (c, None),
                // NOTHING SAID: json, the debuggable one — unless the pin excludes it, in
                // which case the pin IS the deployment's answer and there is no other.
                (None, None) => (
                    if allowed.contains(&Codec::Json) {
                        Codec::Json
                    } else {
                        allowed.first().copied().unwrap_or(Codec::Json)
                    },
                    None,
                ),
            }
        }
    };
    if !allowed.contains(&codec) {
        return Err(Refusal::Pinned { codec, allowed });
    }
    Ok(Negotiated { codec, echo })
}

/// `?codec=`: absent is `None`; present-but-empty is still a value, and a bad one.
fn codec_param(v: Option<&str>) -> Result<Option<Codec>, Refusal> {
    match v {
        None => Ok(None),
        Some(v) => Codec::parse(v)
            .map(Some)
            .ok_or_else(|| Refusal::BadQuery { name: "codec", value: v.to_string() }),
    }
}

/// The legacy `?protocol=`: same ladder rule, one stricter heart — the old codec's
/// `from_query` turned every unrecognized value into CBOR, which is exactly the silent
/// fallback this ladder exists to remove.
fn protocol_param(v: Option<&str>) -> Result<Option<Codec>, Refusal> {
    match v {
        None => Ok(None),
        Some(v) => Codec::parse(v)
            .map(Some)
            .ok_or_else(|| Refusal::BadQuery { name: "protocol", value: v.to_string() }),
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

    /// The ladder's four rungs, in priority order, and that the subprotocol is ECHOED only
    /// when the client's offer won.
    #[test]
    fn the_ladder_is_subprotocol_then_codec_then_protocol_then_json() {
        const BOTH: &[Codec] = &[Codec::Json, Codec::Cbor];
        let one = |offered: &[&str], codec: Option<&str>, protocol: Option<&str>| {
            negotiate(offered, codec, protocol, BOTH)
        };

        // 1. subprotocol, echoed.
        assert_eq!(
            one(&["accrete-cbor"], None, None),
            Ok(Negotiated { codec: Codec::Cbor, echo: Some("accrete-cbor") })
        );
        // …and it outranks both query rungs.
        assert_eq!(
            one(&["accrete-json"], Some("cbor"), Some("cbor")),
            Ok(Negotiated { codec: Codec::Json, echo: Some("accrete-json") }),
            "the highest rung wins, not the most specific one"
        );
        // 2. `?codec=`.
        assert_eq!(
            one(&[], Some("cbor"), Some("json")),
            Ok(Negotiated { codec: Codec::Cbor, echo: None })
        );
        // 3. the legacy `?protocol=`.
        assert_eq!(
            one(&[], None, Some("json")),
            Ok(Negotiated { codec: Codec::Json, echo: None })
        );
        // 4. nothing: json, the debuggable one.
        assert_eq!(
            one(&[], None, None),
            Ok(Negotiated { codec: Codec::Json, echo: None })
        );
        // A foreign protocol in the list is somebody else's and is skipped, not refused.
        assert_eq!(
            one(&["chat", "accrete-cbor"], None, None),
            Ok(Negotiated { codec: Codec::Cbor, echo: Some("accrete-cbor") })
        );
        // A client that lists ours twice in both orders gets its first.
        assert_eq!(
            one(&["accrete-cbor", "accrete-json"], None, None),
            Ok(Negotiated { codec: Codec::Cbor, echo: Some("accrete-cbor") })
        );
    }

    /// NEVER a silent fallback: an `accrete-` word we do not speak, or a bad query value, is
    /// a refusal — not a downgrade to JSON and not the old "anything means CBOR".
    #[test]
    fn an_unspoken_name_is_refused_rather_than_downgraded() {
        const BOTH: &[Codec] = &[Codec::Json, Codec::Cbor];
        assert_eq!(
            negotiate(&["accrete-msgpack"], None, None, BOTH),
            Err(Refusal::UnknownSubprotocol("accrete-msgpack".into()))
        );
        assert_eq!(
            negotiate(&[], Some("msgpack"), None, BOTH),
            Err(Refusal::BadQuery { name: "codec", value: "msgpack".into() })
        );
        assert_eq!(
            negotiate(&[], Some(""), None, BOTH),
            Err(Refusal::BadQuery { name: "codec", value: String::new() }),
            "present but empty is a value the client wrote"
        );
        assert_eq!(
            negotiate(&[], None, Some("binary"), BOTH),
            Err(Refusal::BadQuery { name: "protocol", value: "binary".into() }),
            "the legacy rung stopped meaning `anything = CBOR`"
        );
    }

    /// ONE FLAG, ONE CODEC: a deployment pinned to `json` refuses a peer that can only speak
    /// CBOR instead of accepting a connection neither side can read.
    #[test]
    fn a_pin_refuses_a_choice_outside_it_and_is_the_default_otherwise() {
        const JSON_ONLY: &[Codec] = &[Codec::Json];
        const CBOR_ONLY: &[Codec] = &[Codec::Cbor];

        assert_eq!(
            negotiate(&["accrete-cbor"], None, None, JSON_ONLY),
            Err(Refusal::Pinned { codec: Codec::Cbor, allowed: JSON_ONLY })
        );
        assert_eq!(
            negotiate(&[], Some("cbor"), None, JSON_ONLY),
            Err(Refusal::Pinned { codec: Codec::Cbor, allowed: JSON_ONLY })
        );
        assert_eq!(
            negotiate(&[], None, None, JSON_ONLY),
            Ok(Negotiated { codec: Codec::Json, echo: None })
        );
        // Absent everything under a CBOR-only pin: the pin IS the answer (otherwise the flag
        // could never take effect for a client that asks for nothing).
        assert_eq!(
            negotiate(&[], None, None, CBOR_ONLY),
            Ok(Negotiated { codec: Codec::Cbor, echo: None })
        );
    }

    /// The spellings both rungs share, and the ones they must not accept.
    #[test]
    fn the_subprotocol_spellings_round_trip_and_nothing_else_parses() {
        for codec in [Codec::Json, Codec::Cbor] {
            assert_eq!(Codec::from_subprotocol(codec.subprotocol()), Some(codec));
            assert_eq!(Codec::parse(codec.as_str()), Some(codec));
            assert_eq!(codec.subprotocol(), format!("accrete-{}", codec.as_str()));
        }
        assert_eq!(Codec::from_subprotocol("json"), None, "the prefix is not optional");
        assert_eq!(Codec::from_subprotocol("accrete-CBOR"), None);
        assert_eq!(Codec::parse("CBOR"), None);
    }
}
