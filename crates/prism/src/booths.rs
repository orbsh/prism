//! The echo booths — one per embedded language, the minimal script in
//! each registered form (the carrier surfaces, mirroring aura's own
//! test shapes so this repo reads as a consumer, not a fork).
//!
//! Naming follows the multi-entry model's discipline: the handler name
//! IS the event the gateway invokes (dispatch rule in lib.rs), so each
//! language's echo names type == handler — carrier name with the
//! `echo_` prefix (hyphens are not identifiers; one obvious mapping,
//! no convention table): `{"ev": "echo_steel", ...}` → type
//! `echo_steel`, handler `echo_steel`.
//!
//! Sources mirror aura's test fixtures (echo.rs steel/python/nushell
//! shapes).

use aura_booth::BoothType;

/// One echo entry: type name (the `ev` the gateway invokes; the
/// handler name equals it — dispatch's rule, so a type's source may
/// define ONLY the handler named after the type), carrier language,
/// and source.
pub struct Echo {
    pub type_name: &'static str,
    pub language: &'static str,
    pub source: String,
}

/// The bgi spawn spec source (ADR-0035): `nu <author.nu>` — the nushell
/// PTY carrier retired; nu rides the framed two-fifo adapter, so the
/// booth's source IS the argv, pointing at the shared fixture.
fn fixture_bgi_nu() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../effector/crates/bgi-guest/examples/bgi_nu.nu").to_string()
}

/// The registered set; carriers absent from this build are skipped at
/// registration with a note (`Gateway::with_echoes`), never silently.
pub fn echo_booths() -> Vec<Echo> {
    let mut v: Vec<Echo> = Vec::new();
    #[cfg(feature = "steel")]
    v.push(Echo {
        type_name: "echo_steel",
        language: "steel",
        source: "(define (echo_steel args) args)".into(),
    });
    // The identity acceptance booth (ADR-0017 §7 amendment): the
    // gateway delivers every handler an envelope {"sender": {device,
    // user}, "args": ...}; echo_sender answers with the sender half,
    // so the wire proves who the plane believes is talking. Its own
    // type (the handler-name rule above), steel carrier.
    #[cfg(feature = "steel")]
    v.push(Echo {
        type_name: "echo_sender",
        language: "steel",
        source: r#"(define (echo_sender args) (hash-ref args "sender"))"#.into(),
    });
    // The per-event auth enforcement booth (ADR-0017 §2): the
    // persisted interface_schema carries an `auth` block keyed by
    // handler name — presence means the event requires a bound user.
    // The gateway enforces it BEFORE the call (the event never
    // reaches the booth from an anonymous sender); the handler itself
    // is the sender proof again. Value is reserved (presence is the
    // rule; a future form axis would ride it).
    #[cfg(feature = "steel")]
    v.push(Echo {
        type_name: "echo_priv",
        language: "steel",
        source: r#"(define (interface_schema args)
  (hash "auth" (hash "echo_priv" "required")))
(define (echo_priv args) (hash-ref args "sender"))"#
            .into(),
    });
    #[cfg(feature = "python")]
    v.push(Echo {
        type_name: "echo_python",
        language: "python",
        source: "def echo_python(args):\n    return args\n".into(),
    });
    v.push(Echo {
        type_name: "echo_nu",
        language: "bgi",
        source: format!("nu {}", fixture_bgi_nu()),
    });
    v
}

/// Convenience for tests that need one BoothType directly.
pub fn echo_type(e: &Echo) -> BoothType {
    BoothType::script(e.type_name, e.language, e.source.clone())
}
