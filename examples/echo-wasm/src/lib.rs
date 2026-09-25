//! echo-wasm — the Rust-authored wasm echo actor (ADR-0026 §4: wasm is
//! the full-power path, okm compiles INTO the module when storage is
//! needed; a pure echo needs none of it, so this ships zero deps).
//!
//! Carrier ABI (probe-runtime `carrier/wasmtime.rs`):
//! - export `memory` (the cdylib provides it) + `aura_alloc(len)->ptr`
//!   (the host writes handler args through it — a bump allocator here);
//! - handler exports named after their events: `echo(ptr,len)->i64`,
//!   packed `(ptr<<32)|len` return. The carrier writes CBOR args into
//!   guest memory via `aura_alloc`, then reads the packed reply after
//!   the call — an echo IS the same bytes: return the pair unchanged.
//! - no `aura_host` imports = no host bridge, nothing to satisfy at
//!   instantiation; `interface_schema` is absent so the carrier derives
//!   the receives half from the export list (`echo`).
//!
//! no_std: no allocator, no std::sync — the whole module is a few
//! dozen bytes of code, static-linkable to nothing.
#![no_std]

/// no_std wasm needs an explicit panic landing. No reachable panic
/// exists in this module (no indexing, no unwraps); the handler exists
/// to satisfy the panic lang-item. An unreachable trap = a broken
/// build, so trap loudly.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

/// Guest bump allocator the carrier REQUIRES (its host imports and the
/// arg write both go through it). Heap starts after a 1 KiB guard page
/// so a null-offset bug is visible, never silently corrupts data.
static mut HEAP_NEXT: usize = 1024;

#[no_mangle]
pub extern "C" fn aura_alloc(len: i32) -> i32 {
    // Single-threaded by the carrier's contract (per-instance resident
    // session, calls serialized by the slot lock) — a plain static
    // mut is sound here; Sync would be a lie the type system cannot
    // check either way.
    // `--target wasm32-unknown-unknown` + no build-std; the artifact is
    // a few hundred bytes (the whole guest, base64-embedded by actors.rs).
    unsafe {
        let p = &raw mut HEAP_NEXT;
        let r = (*p) as i32;
        *p = (*p).wrapping_add(len as usize);
        r
    }
}

/// The echo handler: event name = export name. The carrier CBOR-writes
/// the args into linear memory and calls `(ptr, len)`; the reply is
/// read back from the returned packed pointer — so returning the pair
/// unchanged IS the echo (zero copy, no CBOR parse).
#[no_mangle]
pub extern "C" fn echo_wasm(ptr: i32, len: i32) -> i64 {
    ((ptr as u32 as i64) << 32) | (len as u32 as i64)
}
