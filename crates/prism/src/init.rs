//! The `init` answer (ADR-0017 §1/§8 + its 2026-10-29 Update; PLAN Phase 1.10): what the
//! renderer gets when it asks for the application's layout.
//!
//! The source is a DEPLOYMENT FACT with two legal forms, and both produce the same thing —
//! a list of view frames, delivered as ordinary `Message` frames on the renderer's leg:
//!
//! - `file:<path>` — read at boot (YAML or JSON; YAML is a superset, so one parser serves
//!   both). The document is a frame list: `- action: create` / `data: …`, exactly what the
//!   examples ship.
//! - `event:<type>/<handler>` — submitted to the realm on the booth type's SINGLETON
//!   instance: a GLOBAL booth (one instance for the whole venue), because a layout is not
//!   per-client state. Whatever it returns is the layout: a list of frames, or one frame.
//!
//! `webhook:<url>` is REFUSED by ruling — fetching JSON over HTTP on the client's first
//! event is the previous generation's shape, adds a deployment dependency and a config
//! surface, and buys nothing a booth cannot do by making the call inside its own body.
//!
//! Absent `PRISM_INIT` is legal: `init` answers the standard error VALUE naming the config,
//! and every other face keeps serving.

use serde_json::Value;
use std::path::Path;

/// Where the layout comes from. Both variants yield the same wire artifact.
#[derive(Clone, Debug)]
pub enum InitSource {
    /// Frames read from a document at boot.
    File(Vec<Value>),
    /// Frames produced by a global booth on every `init`.
    Event { booth_type: String, handler: String },
}

impl InitSource {
    /// Parse one `PRISM_INIT` spec. Every form is spelled out in the error: a spec that is
    /// refused says WHY it is refused (the webhook case names the ruling), never just "bad".
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let spec = spec.trim();
        if let Some(path) = spec.strip_prefix("file:") {
            return Ok(Self::File(read_document(Path::new(path))?));
        }
        if let Some(rest) = spec.strip_prefix("event:") {
            let (booth_type, handler) = rest
                .split_once('/')
                .filter(|(t, h)| !t.is_empty() && !h.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("PRISM_INIT 'event:' wants `<booth_type>/<handler>`, got `{rest}`")
                })?;
            return Ok(Self::Event { booth_type: booth_type.to_string(), handler: handler.to_string() });
        }
        if spec.starts_with("webhook:") {
            anyhow::bail!(
                "PRISM_INIT 'webhook:' is refused (2026-10-29 ruling): a gateway fetch puts a \
                 network dependency and a config surface on the client's first event and buys \
                 nothing a booth cannot do — use `event:<type>/<handler>` and make the call \
                 inside the handler's body"
            );
        }
        anyhow::bail!(
            "PRISM_INIT must be `file:<path>` (YAML or JSON frame list, read at boot) or \
             `event:<booth_type>/<handler>` (a global booth's answer), got `{spec}`"
        )
    }

    /// Read the deployment's spec from the environment. Absent or empty is legal.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        match std::env::var("PRISM_INIT").ok().filter(|s| !s.trim().is_empty()) {
            Some(spec) => Ok(Some(Self::parse(&spec)?)),
            None => Ok(None),
        }
    }

    /// One line for the boot log (a deployment states what it serves).
    pub fn describe(&self) -> String {
        match self {
            Self::File(frames) => format!("prism: init layout from a file, {} frame(s)", frames.len()),
            Self::Event { booth_type, handler } => {
                format!("prism: init layout from booth `{booth_type}` handler `{handler}` (singleton)")
            }
        }
    }
}

/// Read a frame list from a document. YAML covers JSON (a superset), so one parser serves
/// both extensions; the shape is a LIST of frames or a single frame object.
fn read_document(path: &Path) -> anyhow::Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("PRISM_INIT document '{}' is unreadable: {e}", path.display()))?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("PRISM_INIT document '{}' is not YAML/JSON: {e}", path.display()))?;
    let value: Value = serde_json::to_value(yaml)
        .map_err(|e| anyhow::anyhow!("PRISM_INIT document '{}' did not convert: {e}", path.display()))?;
    normalize(value, &path.display().to_string())
}

/// A layout document is a frame list; a single frame object is accepted as a one-frame list
/// (a hand-written minimal layout). Anything else is refused, naming where it came from —
/// a file path or `booth:<type>/<handler>`.
pub fn normalize(value: Value, origin: &str) -> anyhow::Result<Vec<Value>> {
    match value {
        Value::Array(frames) => Ok(frames),
        Value::Object(_) => Ok(vec![value]),
        other => anyhow::bail!(
            "init document '{origin}' must be a list of frames or a single frame object, got {}",
            match other {
                Value::Null => "null",
                Value::Bool(_) => "a boolean",
                Value::Number(_) => "a number",
                Value::String(_) => "a string",
                _ => "something else",
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_is_refused_with_the_ruling_named() {
        let err = InitSource::parse("webhook:https://example/say").expect_err("refused");
        let text = err.to_string();
        assert!(text.contains("refused"), "{text}");
        assert!(text.contains("event:<type>/<handler>"), "{text}");
    }

    #[test]
    fn forms_parse() {
        assert!(matches!(
            InitSource::parse("event:chat/layout").expect("event form"),
            InitSource::Event { .. }
        ));
        assert!(InitSource::parse("event:chat").is_err(), "handler missing");
        assert!(InitSource::parse("event:/layout").is_err(), "type missing");
        assert!(InitSource::parse("nonsense").is_err(), "unknown form");
        assert!(InitSource::parse("file:/nonexistent/layout.yaml").is_err(), "unreadable file");
    }

    #[test]
    fn documents_normalize_lists_and_single_frames() {
        let dir = tempfile::tempdir().expect("tempdir");
        let list = dir.path().join("list.yaml");
        std::fs::write(&list, "- action: create\n  data: {type: case}\n- action: set\n  data: {}\n")
            .expect("write");
        let frames = read_document(&list).expect("list reads");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["action"], "create");

        let single = dir.path().join("single.json");
        std::fs::write(&single, r#"{"action":"create","data":{"type":"case"}}"#).expect("write");
        let frames = read_document(&single).expect("single frame reads");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["action"], "create");

        let scalar = dir.path().join("scalar.yaml");
        std::fs::write(&scalar, "just a string\n").expect("write");
        assert!(read_document(&scalar).is_err(), "a scalar is not a layout");
    }
}