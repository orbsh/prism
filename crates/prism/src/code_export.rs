//! Static code export (PLAN Phase 1.9, ADR-0027 §4): `GET /code/{sha256}`
//! on the SAME accept loop as the WS gateway — a plain immutable
//! download, NOT a WebSocket. Content lives at exactly one address (the
//! URL is self-verifying by construction); the sha256 rides the
//! remote-call FRAME as assertion (never parsed from the URL — §1), so
//! the response body is the bare bytes with no envelope.
//!
//! Same-process read: prism is an engine-bearing resident component, the
//! blob bytes are a direct meta-plane read (`aura_realm::meta::get_blob`),
//! never a proxied fetch. No auth by default: the hash IS the capability
//! and the URL only ever appears inside control-plane-signed frames — an
//! access-control surface here would extend ADR-0015, not mint a second
//! one. Confidentiality is a deployment option (private static source /
//! signed URLs with CDN cache-key normalization).
//!
//! Why the route decision is made at the raw-HTTP level: tungstenite
//! 0.24's handshake callback can only decorate a SUCCESSFUL upgrade (no
//! early-response mechanism exists until `HttpReturned`, 0.26+), so a
//! non-WS path cannot ride that machinery. The accept loop reads the
//! request head, serves `/code/...` inline, and REPLAYS the head into
//! the WS handshake (`Prefixed` below) so tokio-tungstenite re-reads
//! exactly the request it never saw arrive.

use aura_realm::{meta, mq::MqStore};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

/// The request head read by the accept loop before the route decision:
/// header bytes up to and including the terminating blank line, plus
/// the body when `Content-Length` announced one (admin POSTs). WS
/// upgrade requests carry neither — the replay for the WS path hands
/// back only `bytes`, and the body is consumed here so it can never
/// leak into the handshake stream.
pub(crate) struct Head {
    pub bytes: Vec<u8>,
    /// Request target (query stripped; the routes ignore query).
    pub path: String,
    pub method: String,
    pub body: Vec<u8>,
}

impl Head {
    pub fn is_get(&self) -> bool {
        self.method == "GET"
    }
}

/// Read the request head with a bound (header over 32 KiB aborts; a
/// live-WS client never sends one that large). Returns None on EOF.
pub(crate) async fn read_head(stream: &mut TcpStream) -> Option<Head> {
    const BOUND: usize = 32 * 1024;
    let mut bytes: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_head_end(&bytes) {
            let text = String::from_utf8_lossy(&bytes).to_string();
            let first = text.lines().next().unwrap_or("");
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let target = parts.next().unwrap_or("");
            let path = target.split('?').next().unwrap_or(target).to_string();
            let head_bytes = bytes[..pos].to_vec();
            // Body: whatever arrived past the header + the announced
            // Content-Length, read to completion (bounded by the same
            // ceiling — admin payloads are small JSON).
            let mut body = bytes[pos..].to_vec();
            let declared = content_length(&text);
            while body.len() < declared {
                if body.len() > BOUND {
                    return None;
                }
                let n = stream.read(&mut chunk).await.ok()?;
                if n == 0 {
                    return None;
                }
                body.extend_from_slice(&chunk[..n]);
            }
            return Some(Head { bytes: head_bytes, path, method, body });
        }
        if bytes.len() > BOUND {
            return None;
        }
    }
}

fn content_length(head_text: &str) -> usize {
    head_text
        .lines()
        .skip(1)
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
}

fn find_head_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// The export branch: `GET /code/{sha256}` answered as a plain HTTP
/// response, connection closed. Returns false for any other route (the
/// caller proceeds to the WS upgrade with the head replayed).
///
/// Responses (ADR-0027 §4): 200 + `Cache-Control: public,
/// max-age=31536000, immutable` on a hit — revalidation never lies: the
/// address cannot serve different bytes. 404 on absence (never a partial
/// guess); 400 on a malformed address (exactly 64 hex chars — a
/// truncated key is a client bug, not a miss).
pub(crate) async fn serve_code_export(mq: &MqStore, head: &Head, stream: &mut TcpStream) -> bool {
    let Some(hex) = head.path.strip_prefix("/code/") else {
        return false;
    };
    let body: Vec<u8> = match parse_sha(hex) {
        None => plain(400, "expected /code/<64-char-hex-sha256>"),
        Some(_) if !head.is_get() => plain(405, "GET only"),
        Some(sha) => match meta::get_blob(mq, &sha) {
            Some(bytes) => {
                let mut b = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                     Content-Length: {}\r\nCache-Control: public, max-age=31536000, immutable\r\n\
                     Connection: close\r\n\r\n",
                    bytes.len()
                )
                .into_bytes();
                b.extend_from_slice(&bytes);
                b
            }
            None => plain(404, "no blob under this address"),
        },
    };
    let _ = stream.write_all(&body).await;
    let _ = stream.shutdown().await;
    true
}

pub(crate) fn plain(status: u16, msg: &str) -> Vec<u8> {
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "OK",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{msg}",
        msg.len()
    )
    .into_bytes()
}

/// Hex-decode a 64-char sha256 (strict: wrong length or non-hex = None).
fn parse_sha(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// Replay adapter: reads the already-consumed request head first, then
/// the socket. tokio-tungstenite's handshake re-reads the request
/// through this, so the head peek costs the WS path nothing.
pub(crate) struct Prefixed {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: TcpStream,
}

impl Prefixed {
    pub(crate) fn new(head: Vec<u8>, inner: TcpStream) -> Self {
        Self { prefix: std::io::Cursor::new(head), inner }
    }
}

impl AsyncRead for Prefixed {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let remaining = (me.prefix.get_ref().len() - me.prefix.position() as usize) as u64;
        if remaining > 0 {
            return Pin::new(&mut me.prefix).poll_read(cx, buf);
        }
        Pin::new(&mut me.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Prefixed {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_parser_is_strict() {
        let good = "a".repeat(64);
        assert!(parse_sha(&good).is_some());
        assert!(parse_sha(&"a".repeat(63)).is_none());
        assert!(parse_sha(&"a".repeat(65)).is_none());
        assert!(parse_sha(&format!("{}z", "a".repeat(63))).is_none());
        // uppercase hex is a valid digest form
        assert!(parse_sha(&"AB".repeat(32)).is_some());
    }

    #[test]
    fn head_end_scan() {
        assert_eq!(find_head_end(b"GET /x HTTP/1.1\r\n\r\nrest"), Some(19));
        assert_eq!(find_head_end(b"GET /x\r\n"), None);
    }
}
