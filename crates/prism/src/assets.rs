//! Static bundle serving (ADR-0017 §1/§8, PLAN Phase 1.10): the renderer's bundle is
//! served as plain downloads on the SAME accept loop as `/ws` and `/code/` — no event
//! semantics, no upgrade, no second protocol.
//!
//! Mount shape: the bundle IS the origin. `/` answers `<root>/index.html`; every other
//! unreserved path is `<root>/<path>` — so the artifact's own references (`/ui_leptos-*.js`,
//! `/assets/main.css`) work as trunk emitted them, with NO rebuild flag and no link
//! rewriting. That is also stage's shape (`crates/stage`: everything not a route is static),
//! and the same build serves reflux, which serves the bundle at its page root too. The
//! reserved prefixes are `/ws`, `/ui`, `/code/…`, `/admin/…` and `/effector/<alias>`; they
//! are routed before this branch, so a bundle can never shadow a verb.
//!
//! Path discipline: the root is canonicalized once at boot and every request path is
//! canonicalized before the read, so a `..` cannot escape by any spelling (doubled slash,
//! encoded segment, symlink). A path resolving outside the root — or to a directory — is a
//! 404, never a directory listing: the bundle carries exactly the files it ships.
//!
//! Caching: hashed assets (`*.js`, `*.wasm`, trunk names them by content) are immutable for
//! a year; everything else revalidates, so a rebuilt entry document never needs a
//! cache-busting URL.

use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::code_export::{plain, Head};

/// The mounted bundle directory (`PRISM_ASSETS`), canonicalized at boot.
pub struct AssetRoot {
    root: PathBuf,
}

impl AssetRoot {
    /// Open a declared bundle directory. A path that does not resolve to a directory is a
    /// boot error, not a silent 404-only deployment: a misconfigured mount is a deployment
    /// fact the operator must see.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        let root = dir
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("PRISM_ASSETS '{}' is unreadable: {e}", dir.display()))?;
        if !root.is_dir() {
            anyhow::bail!("PRISM_ASSETS '{}' is not a directory", root.display());
        }
        Ok(Self { root })
    }

    /// Read the mount from `PRISM_ASSETS`. Absent is legal — an install with no front end
    /// still serves identity, turns and the peer leg; the boot line says so.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        match std::env::var("PRISM_ASSETS").ok().filter(|s| !s.trim().is_empty()) {
            Some(dir) => Ok(Some(Self::open(Path::new(dir.trim()))?)),
            None => Ok(None),
        }
    }

    /// One line naming where the static face is mounted (the boot log's disclosure rule: a
    /// deployment states what it serves).
    pub fn describe(&self) -> String {
        format!("prism: renderer bundle at / ← {}", self.root.display())
    }

    /// The static branch: any GET that no route claimed. Returns false for a non-GET to an
    /// unknown path (the caller falls through to the upgrade replay, which answers nothing —
    /// the same as before this branch existed).
    pub(crate) async fn serve(&self, head: &Head, stream: &mut TcpStream) -> bool {
        // A plain GET is ours; an UPGRADE request belongs to the WS legs and must reach the
        // upgrade machinery untouched (answering it with a 404 would break /ws and /ui).
        if !head.is_get() || is_upgrade(head) {
            return false;
        }
        let Some(rel) = bundle_rel(&head.path) else {
            return false;
        };
        let body = if !head.is_get() {
            plain(405, "GET only")
        } else {
            match self.read(rel).await {
                Ok(Some((bytes, ctype, immutable))) => response_200(&bytes, ctype, immutable),
                Ok(None) => plain(404, "no such asset in the bundle"),
                Err(e) => plain(500, &format!("bundle read: {e}")),
            }
        };
        let _ = stream.write_all(&body).await;
        let _ = stream.shutdown().await;
        true
    }

    /// Resolve and read one bundle file. `Ok(None)` = not in the bundle (404): an escaping
    /// path, a directory and a missing file are the same answer.
    async fn read(&self, rel: &str) -> std::io::Result<Option<(Vec<u8>, &'static str, bool)>> {
        let Some(path) = self.resolve(rel) else {
            return Ok(None);
        };
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some((bytes, content_type(&path), immutable(&path))))
    }

    /// `/assets` and `/assets/` are the bundle's entry document; every other path is a
    /// bundle-relative file. A component that is empty, `.`, `..` or carries a backslash is
    /// refused outright (belt), then the canonical path must still sit under the root
    /// (braces) and be a file.
    fn resolve(&self, rel: &str) -> Option<PathBuf> {
        let rel = if rel.is_empty() { "index.html" } else { rel };
        if rel
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == ".." || c.contains('\\'))
        {
            return None;
        }
        let real = self.root.join(rel).canonicalize().ok()?;
        if !real.starts_with(&self.root) || !real.is_file() {
            return None;
        }
        Some(real)
    }
}

/// The bundle-relative path for a request: `/` is the entry document, anything else is that
/// file inside the bundle. `None` for a path this branch does not answer (a WS upgrade on a
/// route the caller already ruled out).
fn bundle_rel(path: &str) -> Option<&str> {
    if path == "/" || path.is_empty() {
        return Some("");
    }
    path.strip_prefix('/')
}

/// Does this head ask for a WebSocket upgrade? Checked on the raw head bytes: the handshake
/// callback cannot answer early, and this branch runs before it.
fn is_upgrade(head: &Head) -> bool {
    String::from_utf8_lossy(&head.bytes).to_ascii_lowercase().contains("upgrade: websocket")
}

fn response_200(bytes: &[u8], ctype: &str, immutable: bool) -> Vec<u8> {
    let cache = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: {cache}\r\nConnection: close\r\n\r\n",
        bytes.len()
    )
    .into_bytes();
    out.extend_from_slice(bytes);
    out
}

/// Content type by extension — the bundle's file kinds, nothing guessed: an unknown
/// extension is `application/octet-stream` (a download, not a mislabeled document).
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        Some("glb") => "model/gltf-binary",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Which bundle files may be cached forever: the ones whose NAME is their content (trunk's
/// hash in the filename). Everything else revalidates, so a rebuilt entry document is not
/// served from a year-old cache.
fn immutable(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()), Some("js") | Some("wasm"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(dir: &Path) -> AssetRoot {
        AssetRoot::open(dir).expect("temp bundle opens")
    }

    #[test]
    fn entry_document_and_relative_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), "<!doctype html>").expect("html");
        std::fs::write(dir.path().join("app.js"), "console.log(1)").expect("js");
        let root = root(dir.path());
        assert_eq!(bundle_rel("/"), Some(""));
        assert_eq!(bundle_rel("/app.js"), Some("app.js"));
        assert_eq!(bundle_rel("/assets/main.css"), Some("assets/main.css"));
        // The path is only the bundle-relative half; the reserved prefixes never reach here.
        assert_eq!(bundle_rel(""), Some(""));
        assert!(root.resolve("app.js").is_some());
        assert!(root.resolve("../Cargo.toml").is_none());
        assert!(root.resolve("nope.js").is_none());
        // A directory is not an asset.
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        assert!(root.resolve("sub").is_none());
        // Empty remainder resolves to the entry document.
        let entry = root.resolve("").and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        assert_eq!(entry.as_deref(), Some("index.html"));
    }

    #[test]
    fn hashed_assets_are_immutable_and_others_revalidate() {
        assert!(immutable(Path::new("ui_leptos-abc.js")));
        assert!(immutable(Path::new("ui_leptos-abc_bg.wasm")));
        assert!(!immutable(Path::new("index.html")));
        assert_eq!(content_type(Path::new("x.wasm")), "application/wasm");
        assert_eq!(content_type(Path::new("x.unknown")), "application/octet-stream");
    }
}