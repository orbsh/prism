//! The echo actors — one per embedded language, the minimal script in
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
//! shapes + the wasmtime ABI). The wasm source is the compiled Rust
//! module in examples/echo-wasm, base64-embedded (build.sh regenerates).

use aura_actor::ActorType;

/// One echo entry: type name (the `ev` the gateway invokes; the
/// handler name equals it — dispatch's rule, so a type's source may
/// define ONLY the handler named after the type), carrier language,
/// and source.
pub struct Echo {
    pub type_name: &'static str,
    pub language: &'static str,
    pub source: String,
}

/// The registered set; carriers absent from this build are skipped at
/// registration with a note (`Gateway::with_echoes`), never silently.
pub fn echo_actors() -> Vec<Echo> {
    let mut v: Vec<Echo> = Vec::new();
    #[cfg(feature = "steel")]
    v.push(Echo {
        type_name: "echo_steel",
        language: "steel",
        source: "(define (echo_steel args) args)".into(),
    });
    // The identity acceptance actor (ADR-0017 §7 amendment): the
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
    #[cfg(feature = "python")]
    v.push(Echo {
        type_name: "echo_python",
        language: "python",
        source: "def echo_python(args):\n    return args\n".into(),
    });
    #[cfg(feature = "nushell")]
    v.push(Echo {
        type_name: "echo_nu",
        language: "nushell",
        source: "export def echo_nu [args] {\n    $args\n}\n".into(),
    });
    #[cfg(feature = "wasmtime")]
    v.push(Echo {
        type_name: "echo_wasm",
        language: "wasmtime",
        // The wasm module exports the handler as `echo_wasm` (see the
        // guest source); its base64 form is embedded by build.sh.
        source: WASM_B64.into(),
    });
    v
}

/// The Rust→wasm echo module (examples/echo-wasm), compiled
/// no_std+cdylib for wasm32-unknown-unknown, carried as base64 (the
/// carrier's binary-source form; WAT text is the other). The handler
/// export is `echo_wasm` (build.sh rewrites this file — the .b64 IS a
/// Rust string expression, `r#"..."#`).
#[cfg(feature = "wasmtime")]
const WASM_B64: &str = include_str!("../../../examples/echo-wasm/echo_wasm.b64");

/// Convenience for tests that need one ActorType directly.
pub fn echo_type(e: &Echo) -> ActorType {
    ActorType::script(e.type_name, e.language, e.source.clone())
}
