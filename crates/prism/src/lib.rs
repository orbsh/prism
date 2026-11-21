//! prism — the connection plane (ADR-0017 §1/§4), first landing: a WS
//! gateway hosting the Aura echo booth in every embedded language.
//!
//! Wire shape (the reduced echo scope, documented in README):
//! - `GET /ws`: the codec is NEGOTIATED once, at the handshake, and fixed for
//!   the connection's lifetime (ADR-0017 §4). The ladder is
//!   [`prism_protocol::negotiate`]'s: `Sec-WebSocket-Protocol`
//!   (`accrete-json` / `accrete-cbor`, echoed back) → `?codec=` → the legacy
//!   `?protocol=` → JSON. A name we do not speak (an unknown `accrete-*`, a bad
//!   `?codec=`) is a 4xx refusal, never a silent fallback, and `PRISM_CODECS`
//!   pins the whole deployment to one codec above every rung. `?token=<uuid>`
//!   restores a binding (a device id authenticates nothing).
//! - The codec chosen here is the CLIENT leg's and nothing else's: no event,
//!   no dispatch and no booth ever sees it. The booth leg has its own codec,
//!   declared by the booth (`Body::Script::encoding` → `INDUCTION_ENCODING`),
//!   and prism transcodes between the two at ONE point — `ui::decode_content`
//!   (`ui.rs`), where a booth's frame enters the renderer's fan-out.
//! - ONE frame shape both directions: `{ev, args}`; the protocol does
//!   not encode direction, dispatch cannot branch on it.
//! - Every connect answers `{"ev": "connected", "args": {device_id,
//!   user_id|null, token}}` — the device assignment, the restored
//!   binding and the credential to present next time, one frame.
//! - Inbound `{ev: "<type>", args}` → `engine.call` the booth named by
//!   `ev` (instance key `"ws"`, handler = event name — the multi-entry
//!   model). Identity rides the PAYLOAD (§7 amendment): the handler
//!   receives `{"sender": {device, user}, "args": <original>}`; Ctx,
//!   Job, InstanceId stay identity-free. The result returns to the
//!   CALLING connection as `{ev: "<type>.result", args}`; failures are
//!   `{ev: "error"}` VALUES, never a close.
//! - `{"ev": "signup"|"login", args: {login_name, password, nickname?}}`
//!   — business events on the same socket (§4: login is an ordinary
//!   event), answered `signup.result`/`login.result {user_id}` and
//!   setting the connection's auth field (§3's per-connection field,
//!   one set — never a second collection).
//! - `{ev: "logout"}` — clears the device's user binding (§2's table,
//!   the cross-reconnect fact) AND the connection's auth field (§3,
//!   immediately — the next auth-gated event errors, the next
//!   broadcast excludes this connection). Idempotent; answered
//!   `logout.result {ok: true}` even when already anonymous.
//! - `{ev: "broadcast", args: {to: "all"|"auth", ...}}` — the frame
//!   fans out to every live connection (or the auth-filtered subset);
//!   the selector defaults to "all". Prism is the outbound bridge:
//!   the connection table is the gateway's, booths never address
//!   connections (modeling.md).
//!
//! `/admin/nodes...` (the ADR-0015 approval surface) and the
//! `/code/{sha256}` export (ADR-0027) ride the same accept loop, as do the
//! renderer's faces (ADR-0017 §10): `/assets/<rel>` (the static bundle,
//! `assets.rs`) and `/channel` (the renderer's leg, `ui.rs` — fluxen's
//! `Message`/`Outflow` wire; the layout is the APPLICATION's answer to the
//! `ui:connect` event, not a deployment fact). ONE SHAPE PER LEG: `/ws` is
//! the programmatic leg and keeps
//! `{ev, args}`; `/channel` carries the renderer's vocabulary. Booth upload is
//! the rest of ADR-0017 — deliberately not here; README states the gap so
//! it reads as scope, not as drift.


// Unit tests may use the unwrap/panic vocabulary the production targets are
// denied (a panicking test is a failing test). `cfg(test)` is on only for this
// crate's own test build; `tests/` crates set the same line at their own root.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn))]
pub mod assets;
pub mod auth;
pub mod booths;
pub mod admin;
pub mod code_export;
pub mod effectors;
pub mod identity;
pub mod mounts;
pub mod nodes;
pub mod resources_ui;
pub mod shell;
pub mod ui;

use aura_booth::{call::Waited, InstanceId, InstanceKey};
use aura_engine::Engine;
use futures_util::{SinkExt, StreamExt};
use prism_protocol::{Codec, Frame};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::Request;
use tokio_tungstenite::tungstenite::Message;

/// One live connection: its writer channel + its auth field (§3 — the
/// state is PER CONNECTION: a login writes the field, fan-out filters
/// on it; 0 = anonymous). Shared Arc so dispatch can set it while the
/// broadcast pass reads it.
struct Conn {
    tx: mpsc::UnboundedSender<Frame>,
    /// §3: the auth field is PER CONNECTION — a login writes it, fan-out filters on it. It holds a
    /// uuid now, so it is a mutex rather than an atomic; the write points are the same two.
    user: AuthField,
    device: identity::DeviceId,
}

/// One connection's live auth state: an `identity::UserId`, anonymous until a token or a login
/// fills it.
pub(crate) type AuthField = Arc<std::sync::Mutex<identity::UserId>>;

/// A text argument from either kind of caller. A PROGRAM sends `{"login_name": "ada"}`; a PAGE sends the
/// form's own envelope — fluxen builds every field as `{data, payload}` (`data` is the field's value,
/// `payload` an optional declared one). One unwrap at the entry keeps that difference out of the login
/// policy, exactly as gravity's `save` unwraps it for its own rows.
fn text_field(args: &serde_json::Value, key: &str) -> Option<String> {
    let value = args.get(key)?;
    let inner = match value {
        serde_json::Value::Object(field) => field.get("data").or_else(|| field.get("payload"))?,
        other => other,
    };
    match inner {
        serde_json::Value::String(s) => Some(s.clone()),
        other if other.is_number() || other.is_boolean() => Some(other.to_string()),
        _ => None,
    }
}

fn auth_field(user: identity::UserId) -> AuthField {
    Arc::new(std::sync::Mutex::new(user))
}

pub(crate) fn auth_get(field: &AuthField) -> identity::UserId {
    *field.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn auth_set(field: &AuthField, user: identity::UserId) {
    *field.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = user;
}

/// A device name, as the person typed it. Display only — it never takes part in a decision, so two
/// devices may share one; the bound is there to keep a list readable and a frame small. Empty is
/// refused here rather than stored, because an empty row already means "show the id".
pub(crate) fn clean_alias(raw: &str) -> anyhow::Result<String> {
    let alias = raw.trim();
    if alias.is_empty() {
        anyhow::bail!("`alias` must not be empty (an unnamed device shows its id)");
    }
    let width = alias.chars().count();
    if width > 64 {
        anyhow::bail!("`alias` is too long: {width} characters, at most 64");
    }
    Ok(alias.to_string())
}

/// The venue's optional face: the renderer's static bundle. A DEPLOYMENT FACT with a legal
/// absence — an install that serves identity/turns/peers and no UI is a real deployment, and
/// it says so at boot. The LAYOUT is not here: it is the application's answer to
/// `ui:connect`, so there is nothing for a deployment to configure.
#[derive(Default)]
pub struct Deployment {
    pub assets: Option<assets::AssetRoot>,
    /// The page frame (ADR-0019): navigation, the regions islands draw into. A FILE because a frame
    /// is data and its owner is the operator; absent means the old arrangement (the application that
    /// answers `ui:connect` draws the whole page).
    pub shell: Option<shell::Shell>,
}

impl Deployment {
    /// Read the fact from the environment. A declared-but-broken fact is a boot error (the
    /// operator must see it), never a silent downgrade to "no UI".
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            assets: assets::AssetRoot::from_env()?,
            shell: shell::Shell::from_env()?,
        })
    }

    /// One line per configured face, for the boot log.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(a) = &self.assets {
            lines.push(a.describe());
        }
        if let Some(s) = &self.shell {
            lines.push(s.describe());
        }
        lines
    }
}

/// Shared gateway state: the engine, prism's own account registry, the
/// declared trust posture (ADR-0015 §7 — constructed, never defaulted),
/// the two optional front-end facts, and the live-connection table.
pub struct Gateway {
    pub engine: Engine,
    registry: identity::Registry,
    posture: nodes::Posture,
    deployment: Deployment,
    conns: Arc<Mutex<HashMap<u64, Conn>>>,
    next_id: AtomicU64,
    /// The renderer's connections (ADR-0017 §10). A separate table from `conns` because
    /// the two legs write different frame types — but the SAME identity facts: a device,
    /// and the auth field a `login` writes. Keeping them apart is what lets one leg speak
    /// Accrete without teaching the peer leg about it.
    ui_conns: ui::UiConns,
    /// WHAT AN ABSENT ACCOUNT MISSED. A conversation is addressed to a person (ADR-0018 §3), and a
    /// person whose screens are all shut has nowhere to hear it. The venue is the only layer that
    /// knows both the address and who is connected, so it holds the tail for that account and hands
    /// it over when a device of theirs proves the account again. Bounded on purpose: a returning
    /// device needs where things STAND, not every step that led there — and a booth redraws its
    /// screen as a whole, so the newest frames supersede the oldest ones held here.
    missed: Arc<Mutex<HashMap<(identity::UserId, String), Held>>>,
    /// The mounts this venue holds: the declarations it replayed, which registered, and what a
    /// caller may therefore address (ADR-0021). A std mutex, not the realm's: written once at
    /// boot, read on a dial and on `/admin/mounts`.
    mounts: std::sync::Mutex<mounts::MountTable>,
}

/// How many increments the venue holds behind the last whole screen before the oldest go.
const MISSED_CAP: usize = 64;

/// One frame waiting for an account: the event it was sent as, and its operations.
type HeldFrame = (String, Vec<content::Content<serde_json::Value>>);

/// WHAT AN ABSENT ACCOUNT IS OWED, split where a screen begins. A frame that redraws a whole
/// screen or region (`create` at the root, `set` on an `island:` slot) already CONTAINS every
/// increment before it, so a replay starts there and drops what came earlier — replaying a screen
/// is replaying a state; replaying the log that led to it is guessing at one.
#[derive(Default)]
pub(crate) struct Held {
    /// DATA frames (an `inject` into a vals slot): a slot's value stands on its own, so these are
    /// owed as they came, whatever the screen did meanwhile.
    data: VecDeque<HeldFrame>,
    /// The newest frame that redrew a whole screen or region, when one was held.
    screen: Option<HeldFrame>,
    /// Layout increments after that screen.
    after: VecDeque<HeldFrame>,
}

impl Held {
    /// Does this frame redraw a whole screen or region? Its first operation says so: a `create`
    /// on the empty (root) slot, or a `set` on the slot an island occupies.
    fn redraws_whole(ops: &[content::Content<serde_json::Value>]) -> bool {
        match ops.first() {
            Some(content::Content::Create(influx)) => influx.slot.is_empty(),
            Some(content::Content::Set(influx)) => influx.slot.starts_with("island:"),
            _ => false,
        }
    }

    fn push(&mut self, name: &str, ops: Vec<content::Content<serde_json::Value>>) {
        // A page's payload that is not an operation list arrives as ONE `inject` — a slot value,
        // not a step in drawing the tree. It is owed whatever the layout did.
        let is_data = matches!(ops.as_slice(), [content::Content::Inject(_)]);
        if is_data {
            self.data.push_back((name.to_string(), ops));
            while self.data.len() > MISSED_CAP {
                self.data.pop_front();
                eprintln!(
                    "prism: an account's held data frames passed {MISSED_CAP}: the oldest is dropped"
                );
            }
            return;
        }
        if Self::redraws_whole(&ops) {
            self.screen = Some((name.to_string(), ops));
            self.after.clear();
            return;
        }
        self.after.push_back((name.to_string(), ops));
        while self.after.len() > MISSED_CAP {
            self.after.pop_front();
            eprintln!(
                "prism: an account's held layout frames passed {MISSED_CAP}: the oldest is dropped \
                 (a screen behind them still replays from itself)"
            );
        }
    }

    /// What the account is owed: the slot values as they came, then — only when a whole screen
    /// was held — where the screen stood and what moved since.
    fn frames(self) -> (Vec<HeldFrame>, Option<Vec<HeldFrame>>) {
        let data: Vec<HeldFrame> = self.data.into_iter().collect();
        let mut drawn: Vec<HeldFrame> = self.screen.into_iter().collect();
        drawn.extend(self.after);
        (data, if drawn.is_empty() { None } else { Some(drawn) })
    }
}

/// The handshake extras read in accept_hdr (before the upgrade
/// consumes the request): the negotiated codec and the device echo.
#[derive(Clone)]
struct Handshake {
    codec: Codec,
    /// The credential. `?device=` authenticates nothing now, so there is no device field here.
    token: Option<identity::TokenId>,
    /// Which leg this connection belongs to (ADR-0017 §4): `/channel` = the renderer's wire.
    ui: bool,
}

/// `PRISM_CODECS` — the deployment's ONE-SWITCH pin (ADR-0017 §4).
///
/// `json` or `cbor` makes the venue serve that codec and REFUSE every connection that asked for
/// the other one, whatever rung of the ladder it asked on. Absent (or empty) serves both and lets
/// each connection choose. One flag, one answer for the whole deployment: a venue that hands two
/// legs two codecs has two wires to debug, and nothing on the outside can tell them apart.
const PRISM_CODECS_ENV: &str = "PRISM_CODECS";

/// The pin's three answers, as `'static` slices because [`prism_protocol::negotiate`] takes
/// [`prism_protocol::Refusal::Pinned`]'s `allowed` by `'static` reference — a handshake must never
/// be able to hand itself a list that outlives its own borrow.
const BOTH_CODECS: [Codec; 2] = [Codec::Json, Codec::Cbor];
const JSON_ONLY: [Codec; 1] = [Codec::Json];
const CBOR_ONLY: [Codec; 1] = [Codec::Cbor];

/// The codecs this deployment serves, read ONCE (the environment is a boot fact).
///
/// The first read is the startup LOG (`serve` asks before the accept loop), so the operator learns
/// what is in force at boot and every handshake afterwards only reads the answer. A value nobody
/// recognizes is STATED and ignored — the venue keeps serving both — because a misspelled pin must
/// not take a deployment down, and the line is what tells its operator the pin is not in force.
fn allowed_codecs() -> &'static [Codec] {
    static ALLOWED: std::sync::OnceLock<&'static [Codec]> = std::sync::OnceLock::new();
    ALLOWED.get_or_init(|| match std::env::var(PRISM_CODECS_ENV) {
        Err(_) => &BOTH_CODECS,
        Ok(v) if v.trim().is_empty() => &BOTH_CODECS,
        Ok(v) => match Codec::parse(v.trim()) {
            Some(Codec::Json) => &JSON_ONLY,
            Some(Codec::Cbor) => &CBOR_ONLY,
            None => {
                eprintln!(
                    "prism: {PRISM_CODECS_ENV}=`{v}` is not `json` or `cbor` — serving both"
                );
                &BOTH_CODECS
            }
        },
    })
}

/// The startup line for the pin: what this venue will accept, and why.
fn codecs_line() -> String {
    let allowed = allowed_codecs();
    let names: Vec<&str> = allowed.iter().map(|c| c.as_str()).collect();
    let pinned = allowed.len() == 1;
    format!(
        "prism: codecs: [{}]{} — per connection: Sec-WebSocket-Protocol (`accrete-json`|`accrete-cbor`) \
         → ?codec= → ?protocol= → json; an unknown name is refused 4xx (ADR-0017 §4)",
        names.join(", "),
        if pinned { format!(" (pinned by {PRISM_CODECS_ENV})") } else { String::new() }
    )
}

/// A refusal as the handshake answer it has to be: a 4xx, with the reason as the body.
///
/// [`prism_protocol::negotiate`] refuses instead of falling through precisely so that this
/// response is possible: a client that asked for a wire it will not get finds out HERE, with a
/// status it can read, instead of reading every frame thereafter as garbage.
fn refusal_response(refusal: &prism_protocol::Refusal) -> tokio_tungstenite::tungstenite::handshake::server::ErrorResponse {
    use tokio_tungstenite::tungstenite::http::{header::CONTENT_TYPE, Response, StatusCode};
    match Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Some(format!("{refusal}\n")))
    {
        Ok(response) => response,
        // Unreachable: a literal status and one literal header cannot be invalid. A refusal must
        // still go out as a refusal, so the bare status is built directly rather than panicking.
        Err(_) => {
            let mut response = Response::new(Some(String::new()));
            *response.status_mut() = StatusCode::BAD_REQUEST;
            response
        }
    }
}

impl Gateway {
    /// Construct with a declared posture (ADR-0015 §7): the mode is an
    /// operator decision the type system refuses to invent — there is
    /// no `Default`, no optional; the binary reads `PRISM_IDENTITY`
    /// (`required` | `open`) and refuses to boot without it.
    /// Hand frames to every renderer connection of one device.
    ///
    /// Used by the SHELL, and by nothing else: the frame a page lives in is the venue's own speech
    /// (ADR-0019), drawn before any application says anything — and an application cannot draw it,
    /// because the frame holds regions belonging to booths it cannot see.
    pub(crate) fn ui_frames(
        &self,
        device: identity::DeviceId,
        frames: Vec<content::Content<serde_json::Value>>,
    ) {
        let conns = match self.ui_conns.lock() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        for conn in conns.values().filter(|c| c.device == device) {
            let _ = conn.tx.send(frames.clone());
        }
    }

    pub fn new(engine: Engine, registry: identity::Registry, posture: nodes::Posture) -> Arc<Self> {
        Self::with_deployment(engine, registry, posture, Deployment::default())
    }

    /// Construct with the front-end faces declared (ADR-0017 §1/§10, PLAN Phase 1.10).
    pub fn with_deployment(
        engine: Engine,
        registry: identity::Registry,
        posture: nodes::Posture,
        deployment: Deployment,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            registry,
            posture,
            deployment,
            conns: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            ui_conns: Arc::new(Mutex::new(HashMap::new())),
            missed: Arc::new(Mutex::new(HashMap::new())),
            // Empty until `install_mounts` runs: a gateway that has not replayed a manifest
            // holds no mounts, and says so rather than pretending to hold the deployment's.
            mounts: std::sync::Mutex::new(mounts::MountTable::default()),
        })
    }

    /// The node registry over prism's own instance (the approval
    /// surface + the handshake's future verdict source).
    /// The declared trust posture (ADR-0015 §7). Read by the effector mount: the handshake
    /// VERIFIES a signature in both modes, and only `required` consults the registry.
    pub(crate) fn posture(&self) -> nodes::Posture {
        self.posture
    }

    pub fn nodes(&self) -> nodes::NodeStore {
        self.registry.nodes()
    }

    /// Register a renderer connection; returns its id and the auth field the leg writes.
    pub(crate) fn register_ui(
        &self,
        tx: mpsc::UnboundedSender<Vec<content::Content<serde_json::Value>>>,
        device: identity::DeviceId,
        user0: identity::UserId,
    ) -> (u64, AuthField) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let user = auth_field(user0);
        self.ui_conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(
            id,
            ui::UiConn { tx, device, user: Arc::clone(&user) },
        );
        (id, user)
    }

    /// Which island a page opens on: the one this account last chose, else the shell's FIRST island
    /// (document order is a deployment's statement of the default). `None` means this deployment has
    /// no shell — the booth that answers `ui:connect` draws the whole page, and there is no island to
    /// name.
    pub(crate) fn active_island(&self, user: identity::UserId) -> Option<String> {
        let islands = self.deployment.shell.as_ref().map(|s| s.islands()).unwrap_or_default();
        if islands.is_empty() {
            return None;
        }
        if !identity::is_anonymous(user) {
            if let Some(chosen) = self.registry.last_island(user) {
                if islands.contains(&chosen) {
                    return Some(chosen);
                }
                // SILENT FALLBACKS ARE INDISTINGUISHABLE FROM LOST STATE. The person left this
                // account on an island this deployment no longer declares (renamed, removed, a
                // different frame): they get the default, and the log says why, so "it opened on
                // the wrong page" has an answer instead of a shrug.
                eprintln!(
                    "prism: this account last stood on island `{chosen}`, which this frame does not \
                     declare ({islands:?}) — opening the first"
                );
            }
        }
        islands.first().cloned()
    }

    /// Say `ui:connect` to ONE device: the shell's arrangement, and which island it is opening on.
    /// An application that cannot tell has to guess between "draw the whole page" and "fill my
    /// region", and guessing wrong draws nothing — or erases every other island with it.
    pub(crate) async fn connect_event(&self, device: identity::DeviceId, island: Option<String>) {
        let connect = format!("{}connect", ui::UI_PREFIX);
        let mut data = serde_json::json!({
            "device": device.to_string(),
            "islands": self.deployment.shell.as_ref().map(|s| s.islands()).unwrap_or_default(),
        });
        if let Some(island) = &island {
            data["island"] = serde_json::Value::String(island.clone());
        }
        let envelope = serde_json::json!({
            "name": connect,
            "device": device.to_string(),
            "shell": self.deployment.shell.is_some(),
            "island": island,
            "data": data,
        });
        eprintln!("prism: renderer leg: device {device} connect -> emit `{connect}`");
        self.emit(&connect, envelope).await;
    }

    /// Hand an account's held frames to the screens of that account which are awake now.
    /// The held queue is the ACCOUNT's, not the device's: every frame in it was deferred because
    /// none of their screens was awake, so whoever is awake at this moment missed them all — and
    /// once one screen has them the debt is settled (a booth draws its next screen whole).
    pub(crate) async fn flush_missed(&self, device: identity::DeviceId, user: identity::UserId) {
        let held: Vec<Held> = {
            let mut missed = self.missed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let booths: Vec<String> = missed
                .keys()
                .filter(|(who, _)| *who == user)
                .map(|(_, booth)| booth.clone())
                .collect();
            booths
                .into_iter()
                .filter_map(|booth| missed.remove(&(user, booth)))
                .collect()
        };
        if held.is_empty() {
            return;
        }
        let mut data: Vec<HeldFrame> = Vec::new();
        let mut drawn: Vec<HeldFrame> = Vec::new();
        for held in held {
            let (frames, screen) = held.frames();
            data.extend(frames);
            if let Some(screen) = screen {
                drawn.extend(screen);
            }
        }
        eprintln!(
            "prism: handing {} held frames to the account's device ({} drawn from a whole screen)",
            data.len() + drawn.len(),
            drawn.len()
        );
        {
            let conns = self.ui_conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for c in conns.values() {
                if *c.user.lock().unwrap_or_else(std::sync::PoisonError::into_inner) != user {
                    continue;
                }
                for (_name, ops) in data.iter().chain(drawn.iter()) {
                    let _ = c.tx.send(ops.clone());
                }
            }
        }
        // LAYOUT WITHOUT A SCREEN BEHIND IT. The frames are replayed as they came — that is what
        // the absent screen would have drawn in order — but nothing here can promise the result
        // equals the live screen, because the booth never said where the screen started. A booth
        // that wants an exact replay emits its page as ONE whole-region frame (`set` on its
        // `island:` slot, or `create` at the root) instead of a bag of appends; `ask_to_redraw` is
        // here for a deployment that would rather redraw than replay.
        let _ = device;
    }

    /// Emit into the default realm, fire and forget. A failure is a LOG, not a reply: the
    /// application's readiness is not the renderer's business (its frames are), and a
    /// connect that dies because an emit failed would be a worse venue than one that says so.
    pub(crate) async fn emit(&self, event: &str, data: serde_json::Value) {
        if let Err(e) = aura_realm::Realm::emit(&self.engine.realm, None, event, data).await {
            eprintln!("prism: emit `{event}` failed: {e}");
        }
    }

    pub(crate) fn unregister_ui(&self, id: u64) {
        self.ui_conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
    }

    /// The account/device facts (the same registry the peer leg uses — identity has one home).
    pub(crate) fn registry(&self) -> &identity::Registry {
        &self.registry
    }

    /// Resolve a connection's identity. A LIVE TOKEN names the (device, user) and slides its
    /// expiry; with no usable token the connection is a FRESH anonymous device that gets a token of
    /// its own, so the client always leaves with something to present next time. An id the client can
    /// produce — a device id, an application's session id, any guessed value — authenticates
    /// nothing (ADR-0018 / the token split).
    /// What this connect resolves to. A presented token names its device; whether that name still
    /// carries permission is the ACTIVITY CLOCK's answer, not the token's — a device silent past the
    /// window connects as anonymous even though the token is alive, and keeps its id (so its alias
    /// survives a timeout). An unknown token mints a new device, which is the only way to get one.
    fn identify(&self, token: Option<identity::TokenId>) -> anyhow::Result<identity::Identified> {
        let now = identity::now_ms();
        if let Some(t) = token {
            if let Some((device, user)) = self.registry.resolve_token(t)? {
                let idle = !identity::is_anonymous(user) && self.registry.idle(device, now)?;
                return Ok(identity::Identified {
                    device,
                    user: if idle { identity::anonymous() } else { user },
                    token: t,
                    idle,
                });
            }
        }
        let device = self.registry.new_device()?;
        let user = identity::anonymous();
        let token = self.registry.issue_token(device, user)?;
        Ok(identity::Identified { device, user, token, idle: false })
    }

    /// Register the shipped auth booth (`auth.rs`): the venue's verbs need a place to ASK whether a
    /// credential names a user, and that place is a booth so a deployment can replace the policy
    /// (an external IdP, an LDAP bind) without prism moving. A venue with no such booth is legal: it
    /// serves the anonymous plane, and every login answers the standard error naming the missing
    /// type.
    pub async fn with_auth(engine: &Engine, registry: &identity::Registry) -> anyhow::Result<()> {
        auth::install(engine, registry.store()).await?;
        println!(
            "prism: auth booth live (`{}`: accounts ns 50, verifiers ns 55)",
            auth::AUTH_BOOTH
        );
        Ok(())
    }

    /// Register the language echo booths (steel / python / nushell
    /// embedded sources). A language carrier absent from this build
    /// skips its booth with a
    /// note — the server still boots with the rest (the demo shape
    /// mirrors aura's feature-forwarded tests).
    pub async fn with_echoes(engine: &Engine) -> anyhow::Result<()> {
        let mut registered: Vec<&str> = Vec::new();
        for e in booths::echo_booths() {
            match engine.register(booths::echo_type(&e)).await {
                Ok(()) => registered.push(e.type_name),
                Err(err) => eprintln!("prism: echo [{}] not registered: {err}", e.type_name),
            }
        }
        println!("prism: echo booths live: {registered:?}");
        // The turn-executor (gravity's real turn + provider booths).
        // Its absence (no python carrier in this build) is a startup
        // note, not a boot failure — turn.submit then answers
        // turn.error per call. The deployment facts (TurnConfig) are
        // read per submit, not here: a boot without them still serves
        // echo/identity; turn verbs report the missing fact.
        match gravity_booth::register(engine).await {
            Ok(()) => println!("prism: turn executor live (gravity booths)"),
            Err(err) => eprintln!("prism: turn executor not registered: {err}"),
        }
        Ok(())
    }

    /// Derive, register and KEEP the node mounts (ADR-0001, ADR-0022): the deployment's
    /// vocabulary (`PRISM_LANGUAGES`) crossed with the approved registry rows, replayed at every
    /// boot because mount definitions are not persisted — and the table stays on the gateway so
    /// the venue can say what a caller may address (ADR-0021) instead of every client carrying a
    /// copy of it.
    ///
    /// A method rather than a constructor: a second place that registers mounts would be a
    /// second answer to "what is callable".
    pub async fn install_mounts(&self) -> anyhow::Result<()> {
        let languages = mounts::vocabulary_from_env()?;
        let mut table = mounts::MountTable::with_vocabulary(languages);
        // One mount per (APPROVED alias × vocabulary entry): the alias list is the node registry's,
        // the vocabulary is the deployment's, and a mount is the product (ADR-0022). A pending or
        // revoked row yields nothing — approval is what makes a machine callable.
        let aliases: Vec<String> = self
            .nodes()
            .list()?
            .into_iter()
            .filter(|row| row.status == "approved")
            .map(|row| row.alias)
            .collect();
        for alias in &aliases {
            table.add_alias(alias);
        }
        let todo = table.decls.clone();
        table.register(&self.engine, todo).await;
        if table.decls.is_empty() {
            println!(
                "prism: no node mounts (PRISM_LANGUAGES unset or no approved node) — no node is callable"
            );
        } else {
            println!(
                "prism: node mounts live: {:?}{}",
                table.live.iter().collect::<Vec<_>>(),
                if table.refused.is_empty() {
                    String::new()
                } else {
                    format!(" ({} refused — see /admin/mounts)", table.refused.len())
                }
            );
        }
        *self
            .mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = table;
        Ok(())
    }

    /// Make one alias callable, deriving its mounts from the vocabulary the venue installed.
    ///
    /// Called when a node registers in `open` posture (there is no registry, so the dial IS the
    /// alias list — the network is the boundary, and the log says the mounts were minted from it)
    /// and, in `required` posture, never: an approved row was already turned into mounts at boot.
    /// Returns the types that became live now, so the caller can say so.
    pub async fn ensure_alias_mounts(&self, alias: &str) -> Vec<String> {
        let todo = self.with_mount_table_mut(|table| table.add_alias(alias));
        if todo.is_empty() {
            return Vec::new();
        }
        // Registration is async and the table is behind a std mutex: take the declarations out,
        // register them, then book the outcomes together (never hold a lock across an await).
        let mut outcomes = Vec::with_capacity(todo.len());
        for decl in todo {
            let outcome = self.engine.register(decl.booth_type()).await;
            outcomes.push((decl.type_name, decl.node_alias, outcome));
        }
        let names: Vec<String> = outcomes.iter().map(|(n, _, _)| n.clone()).collect();
        self.with_mount_table_mut(|table| {
            for (name, alias, outcome) in outcomes {
                table.record(name, alias, outcome);
            }
        });
        names
    }

    /// Mutate the installed table in place (mount derivation for a newly seen alias).
    pub(crate) fn with_mount_table_mut<R>(
        &self,
        f: impl FnOnce(&mut mounts::MountTable) -> R,
    ) -> R {
        let mut table = self
            .mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut table)
    }

    /// A snapshot of the declared mounts: the venue's own answer to "what may be called".
    pub fn mounts(&self) -> Vec<mounts::MountDecl> {
        self.mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .decls
            .clone()
    }

    /// Declarations that did NOT register, in the words the failure used — the other half of
    /// the answer, because "declared" and "callable" are different facts.
    pub fn refused_mounts(&self) -> Vec<String> {
        self.mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refused
            .clone()
    }

    /// Read the installed table (the dial path's carrier-drift note).
    pub(crate) fn with_mount_table<R>(&self, f: impl FnOnce(&mounts::MountTable) -> R) -> R {
        let table = self
            .mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&table)
    }

    /// Which declared types are live, for the listing.
    pub fn live_mounts(&self) -> Vec<String> {
        self.mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live
            .iter()
            .cloned()
            .collect()
    }

    /// Serve forever on `listener`: accept, route, drive. The startup
    /// line states the trust posture where it is visible (ADR-0015 §7's
    /// disclosure rule) — an `open` deployment says so in its own log.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> anyhow::Result<()> {
        // The venue is also a subscriber: an application's view events become renderer
        // frames. Installed here so every path that serves (tests, `main`) gets it.
        // Two sinks: a renderer draws an operation list, a program reads the data shape.
        let peer_conns = std::sync::Arc::clone(&self.conns);
        // The peer leg takes the SAME predicate as the renderers: a `user` reaches every live
        // connection of that account on both legs, or a browser and a CLI would disagree about
        // who "that person" is.
        let peers: ui::PeerDeliver = std::sync::Arc::new(
            move |reach: Option<&ui::Reach>, sender: Option<&str>, name: &str, data: serde_json::Value| {
                let Ok(conns) = peer_conns.lock() else {
                    return;
                };
                for c in conns.values() {
                    let here = c.device.to_string();
                    let user = *c
                        .user
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if sender == Some(here.as_str()) || !ui::reached(reach, c.device, user) {
                        continue;
                    }
                    let _ = c.tx.send(Frame::new(name, data.clone()));
                }
            },
        );
        let missed = std::sync::Arc::clone(&self.missed);
        // The queue is the ACCOUNT's and the BOOTH's: a frame is owed to the person, but it can
        // only be drawn by the booth that owns the region it targets — and a booth's screen is what
        // says where things stand. Keying by both is what lets one booth's screen arrive without
        // flushing another's frames onto a screen that is not there yet.
        let defer: ui::Defer = std::sync::Arc::new(move |booth, user, name, ops| {
            let mut held = missed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            held.entry((user, booth.to_string())).or_default().push(name, ops);
        });
        // The islands the frame declares are handed to the view leg: that is the whole of what an
        // application needs to be namespaced, and the venue is the only party that knows the list.
        let islands = self
            .deployment
            .shell
            .as_ref()
            .map(|s| s.islands())
            .unwrap_or_default();
        ui::install(
            &self.engine,
            std::sync::Arc::clone(&self.ui_conns),
            peers,
            defer,
            islands,
        )
        .await;
        // The grant surface is the PLANE's own page (ADR-0049): a resource belongs to the venue, so
        // no application can list what this venue offers — and none should be able to.
        resources_ui::install(&self.engine).await;
        match self.posture {
            nodes::Posture::Required => {
                eprintln!(
                    "prism: identity posture: `required` (a node token on the dial gates effectors — ADR-0015)"
                )
            }
            nodes::Posture::Open => {
                eprintln!(
                    "prism: identity posture: `open` — registrations are unauthenticated, \
                     the network is the boundary (ADR-0015 §7)"
                )
            }
        }
        // THE TWO LEGS' CODECS, STATED AT BOOT (ADR-0013): what clients may negotiate here, and
        // what the booth leg will produce regardless. The second line is the declaration read
        // back — `Body::Script::encoding` at registration, `INDUCTION_ENCODING` in the guest, and
        // this constant, all one value — so an operator sees at boot where the transcode happens
        // (between the two lines) and that a client's choice never crosses it.
        println!("{}", codecs_line());
        println!(
            "prism: booth leg codec: {} (declared by the booth; a client's negotiated codec stops \
             at the socket — the transcode is `ui::decode_content`)",
            gravity_booth::content_codec().as_str()
        );
        loop {
            let (stream, _) = listener.accept().await?;
            let gw = Arc::clone(&self);
            tokio::spawn(async move {
                let _ = gw.handle_conn(stream).await;
            });
        }
    }

    /// Serve one accepted connection to completion. The accept loop is
    /// SHARED with the static code export (ADR-0027 §4, PLAN 1.9): the
    /// request head is read raw first; `/code/...` is answered as a
    /// plain HTTP download and the socket closes; every other path
    /// replays the head into the WS handshake (tungstenite 0.24's
    /// callback cannot emit an early non-upgrade response, so the
    /// branch lives below the upgrade machinery, not inside it).
    async fn handle_conn(self: Arc<Self>, mut stream: TcpStream) -> anyhow::Result<()> {
        let Some(head) = code_export::read_head(&mut stream).await else {
            return Ok(()); // client died mid-head: nothing to serve
        };
        // The revocation face needs the WHOLE gateway (registry + live-connection
        // table), not just the node store, and it must be asked FIRST: `serve_admin`
        // claims every path under `/admin` (answering 404 for the ones it does not
        // know), so routing it first made `POST /admin/revoke` unreachable — a dead
        // route behind a live-looking handler (found by the renderer-leg suite).
        if let Some(response) = admin::serve_revoke(self.as_ref(), &head.method, &head.path, &head.body).await {
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        // Resources before the `serve_admin` prefix: that one claims every `/admin` path it does
        // not know and answers 404, so a later mount would be unreachable (the same ordering bug
        // `/admin/revoke` hit).
        if let Some(response) =
            admin::serve_resources(self.as_ref(), &head.method, &head.path, &head.body).await
        {
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        if let Some(response) = admin::serve_dead(self.as_ref(), &head.method, &head.path).await {
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        if let Some(response) = admin::serve_admin(&self, &head.method, &head.path, &head.body) {
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        let mq = self.engine.realm.lock().await.mq.clone();
        if code_export::serve_code_export(&mq, &head, &mut stream).await {
            return Ok(());
        }
        // The renderer's static face (ADR-0017 §10): plain downloads on the same accept
        // loop, no upgrade, no event semantics.
        if let Some(assets) = &self.deployment.assets {
            if assets.serve(&head, &mut stream).await {
                return Ok(());
            }
        }
        // Effector dial-in (ADR-0017 §8): `/effector/<alias>` mounts the
        // realm's remote-booth gateway on THIS accept loop. The head is
        // still raw here — the effector's WS carries no `?protocol=`
        // query, so it takes a bare accept over the prefixed stream
        // (the upgrade replaying its head), not the client handshake.
        if let Some(alias) = head.path.strip_prefix("/effector/").filter(|a| !a.is_empty() && !a.contains('/')) {
            // The token rides the dial (ADR-0003): read it off the query before the socket is
            // served, and let the mount decide (a `required` plane refuses a tokenless dial).
            let token = head
                .query
                .split('&')
                .find_map(|kv| kv.strip_prefix("token="))
                .map(str::to_string);
            let stream = code_export::Prefixed::new(head.bytes, stream);
            let ws = tokio_tungstenite::accept_async(stream).await?;
            return effectors::serve_mounted(self.as_ref(), alias.to_string(), token, ws).await;
        }
        let stream = code_export::Prefixed::new(head.bytes, stream);

        let hs_slot: Arc<Mutex<Option<Handshake>>> = Arc::default();
        let slot = Arc::clone(&hs_slot);
        let ws = tokio_tungstenite::accept_hdr_async(
            stream,
            move |req: &Request,
                  mut resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
            // THE LADDER, ONCE PER CONNECTION (ADR-0017 §4, `prism_protocol::negotiate`):
            // subprotocol → `?codec=` → the legacy `?protocol=` → json. Every rung is read here,
            // in the handshake, because the choice is fixed for the connection's lifetime — a
            // later read would be a second answer, and two codecs on one socket is a broken socket.
            let query = req.uri().query().unwrap_or("");
            // `Sec-WebSocket-Protocol` is a COMMA-SEPARATED list in the client's own preference
            // order; the first value we understand wins, and a foreign protocol in it is somebody
            // else's and is skipped. An `accrete-` word we do not implement is a refusal.
            let offered: Vec<&str> = req
                .headers()
                .get(tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL)
                .and_then(|value| value.to_str().ok())
                .map(|list| {
                    list.split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let negotiated = match prism_protocol::negotiate(
                &offered,
                query_param(query, "codec"),
                query_param(query, "protocol"),
                allowed_codecs(),
            ) {
                Ok(negotiated) => negotiated,
                // NO SILENT FALLBACK: the answer is a 4xx carrying the reason, so a client that
                // asked for a wire it will not get learns it here instead of reading every frame
                // thereafter as garbage.
                Err(refusal) => {
                    eprintln!("prism: refused handshake ({}): {refusal}", req.uri().path());
                    return Err(refusal_response(&refusal));
                }
            };
            // ECHO the subprotocol when the client's own offer won: the answer to a handshake that
            // offered `accrete-cbor` is where the client reads which of its offers was taken, and
            // silence would leave it guessing (tungstenite does not echo one for us).
            if let Some(echo) = negotiated.echo {
                resp.headers_mut().insert(
                    tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL,
                    tokio_tungstenite::tungstenite::http::HeaderValue::from_static(echo),
                );
            }
            *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Handshake {
                codec: negotiated.codec,
                token: query_param(query, "token")
                    .and_then(|v| v.parse::<identity::TokenId>().ok()),
                ui: req.uri().path() == ui::UI_PATH,
            });
            Ok(resp)
        })
        .await?;

        // The callback ran during the handshake; `unwrap_or` only
        // guards a handshake that never reached `accept`.
        let Handshake { codec, token, ui } =
            hs_slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone().unwrap_or(Handshake { codec: Codec::Json, token: None, ui: false });
        let identified = self.identify(token)?;
        let (device, user0, token) = (identified.device, identified.user, identified.token);

        // The renderer's leg (ADR-0017 §10): its own wire, its own writer. It is routed
        // BEFORE the peer leg's connection table: a renderer is not a caller, and its
        // identity/questions land here, not on the booth plane.
        if ui {
            let cc = content::codec::ActiveCodec::new(match codec {
                Codec::Json => content::codec::CodecType::Json,
                Codec::Cbor => content::codec::CodecType::Cbor,
            });
            return ui::serve(Arc::clone(&self), ws, cc, identified).await;
        }

        let (mut write, mut read) = ws.split();

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel::<Frame>();

        let user = auth_field(user0);
        self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(
            id,
            Conn { tx: tx.clone(), user: Arc::clone(&user), device },
        );

        // The connect answer: the device assignment frame AND the
        // (possibly restored) binding — one frame, no separate
        // handshake protocol. Sent through the map's copy BEFORE the
        // local sender is dropped (below): after this point the map's
        // clone is the ONLY live sender — "the table owns the sender"
        // — so removing the entry (revocation kick) actually closes
        // the channel and the pump sees the shutdown.
        tx.send(Frame::new(
            "connected",
            // The connect answer: the device, the binding (null = anonymous) and the TOKEN — the
            // one thing the client must keep. Ids travel as text (a uuid is 16 bytes; JSON numbers
            // are f64 in the browser).
            serde_json::json!({
                "device_id": device.to_string(),
                "user_id": (!identity::is_anonymous(user0)).then(|| user0.to_string()),
                "token": token.to_string(),
            }),
        ))
        .ok();
        drop(tx);

        // Outbound pump: the single socket writer — replies and
        // fan-out share it. Removal on ANY exit path lives here (the
        // presence lesson: an aborted task never runs epilogues, so
        // the map cleanup rides the sender's death: the table prunes
        // closed senders on every broadcast, and this remove covers
        // normal/abort exits).
        let conns = Arc::clone(&self.conns);
        let pump = tokio::spawn(async move {
            let mut rx = rx;
            while let Some(frame) = rx.recv().await {
                if write.send(Message::Binary(codec.encode(&frame))).await.is_err() {
                    break;
                }
            }
            // Channel closed (the table dropped our sender — revocation
            // kick or normal removal): close the socket properly so the
            // client's read side SEES the close instead of hanging on a
            // dropped write half.
            let _ = write.send(Message::Close(None)).await;
            let _ = write.flush().await;
            conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
        });

        let result = self.inbound(id, &mut read, codec).await;
        pump.abort();
        let _ = pump.await;
        self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
        result
    }

    async fn inbound(
        &self,
        id: u64,
        read: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
              + Unpin),
        codec: Codec,
    ) -> anyhow::Result<()> {
        while let Some(msg) = read.next().await {
            let bytes = match msg? {
                Message::Binary(b) => b,
                Message::Text(t) => t.into_bytes(),
                Message::Close(_) => break,
                _ => continue, // ping/pong stay inside tungstenite
            };
            let frame = match codec.decode(&bytes) {
                Ok(f) => f,
                Err(e) => {
                    self.send_to(id, Frame::new("error", serde_json::json!({"message": e.to_string()})));
                    continue;
                }
            };
            self.dispatch(id, frame).await;
        }
        Ok(())
    }

    /// Dispatch rules (the echo scope): business events first
    /// (`signup`/`login` answered by the registry, §2/§4), then
    /// gateway verbs (`ping`, `broadcast`), then the booth invoke with
    /// the sender envelope mounted in args (§7 amendment: identity is
    /// payload, never Ctx).
    async fn dispatch(&self, id: u64, frame: Frame) {
        let Frame { ev, args } = frame;

        match ev.as_str() {
            "signup" | "login" => {
                self.auth_event(id, &ev, args).await;
                return;
            }
            "logout" => {
                let device = self.identities(id).0;
                // §3: the per-connection field drives fan-out — clear it
                // so the predicate takes effect on THIS connection
                // immediately; the device-table binding is the
                // cross-reconnect fact and is cleared with it. A refused
                // unbind leaves BOTH facts alone (the row still binds,
                // so the live field must not claim otherwise) and reports
                // itself as an error value — the socket stays up.
                match self.registry.logout(device) {
                    Ok(()) => {
                        if let Some(conn) = self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&id) {
                            auth_set(&conn.user, identity::anonymous());
                        }
                        self.send_to(id, Frame::new("logout.result", serde_json::json!({"ok": true})));
                    }
                    Err(e) => self.send_to(
                        id,
                        Frame::new("error", serde_json::json!({"ev": "logout", "message": e.to_string()})),
                    ),
                }
                return;
            }
            // ACCOUNT VERBS THE VENUE OWNS (ADR-0017 §7): a device list belongs to the account, and
            // no booth can answer it — a booth cannot enumerate devices. `devices` reads the
            // `by_user` index (uuid7 order = first-seen order); `alias` names one of the caller's
            // own devices, and it is display only.
            "devices" => {
                let (device, user) = self.identities(id);
                match self.device_rows(device, user) {
                    Ok(rows) => self.send_to(
                        id,
                        Frame::new("devices.result", serde_json::json!({ "devices": rows })),
                    ),
                    Err(e) => self.send_to(
                        id,
                        Frame::new(
                            "error",
                            serde_json::json!({ "ev": "devices", "message": e.to_string() }),
                        ),
                    ),
                }
                return;
            }
            "alias" => {
                let device = self.identities(id).0;
                let named = clean_alias(args.get("alias").and_then(|v| v.as_str()).unwrap_or_default())
                    .and_then(|alias| self.registry.set_alias(device, &alias).map(|()| alias));
                match named {
                    Ok(alias) => self.send_to(
                        id,
                        Frame::new(
                            "alias.result",
                            serde_json::json!({ "device_id": device.to_string(), "alias": alias }),
                        ),
                    ),
                    Err(e) => self.send_to(
                        id,
                        Frame::new(
                            "error",
                            serde_json::json!({ "ev": "alias", "message": e.to_string() }),
                        ),
                    ),
                }
                return;
            }
            "broadcast" => {
                let to_auth = args.get("to").and_then(|v| v.as_str()) == Some("auth");
                self.broadcast_frame(Frame::new("broadcast", args), to_auth);
                return;
            }
            "ping" => {
                self.send_to(id, Frame::new("ping.result", serde_json::json!({"pong": true})));
                return;
            }
            _ => {}
        }

        // A PROGRAM SPEAKS THE VIEW VOCABULARY TOO (a turn has one home, and it is the
        // application's booth): an `ev` that already carries the marker is an EMIT to the plane,
        // not a booth name — the venue adds nothing to it, and the recipient is the calling
        // device. A bare `ev` keeps the dispatch rule (`ev` names the booth type), so which one a
        // client means stays explicit instead of guessed.
        if ev.starts_with(ui::UI_PREFIX) {
            let (device, user) = self.identities(id);
            if identity::is_anonymous(user) && self.requires_auth(&ev).await {
                self.send_to(
                    id,
                    Frame::new(
                        "error",
                        serde_json::json!({"ev": ev, "message": "authentication required"}),
                    ),
                );
                return;
            }
            let envelope = serde_json::json!({
                "name": ev,
                "device": device.to_string(),
                "sender": self.sender_json(device, user),
                "data": args,
            });
            if let Err(e) = aura_realm::Realm::emit(&self.engine.realm, None, &ev, envelope).await {
                eprintln!("prism: `{ev}` did not reach the plane: {e}");
                self.send_to(
                    id,
                    Frame::new("error", serde_json::json!({"ev": ev, "message": e.to_string()})),
                );
            }
            return;
        }

        let (device, user) = self.identities(id);
        // Per-event auth ENFORCEMENT (ADR-0017 §2): the booth's own DECLARATION says which events
        // need a bound user (`@on(..., auth=True)`, `(on event opts h)`, `.on_as(...).gated()`),
        // and the route carries that flag — one lookup on the table the delivery itself uses, no
        // second ACL, no schema parse. Runs BEFORE the call so the event never reaches the booth
        // from an anonymous sender, and answers with the same `error` VALUE shape as any other
        // failure (the socket stays up).
        if identity::is_anonymous(user) && self.requires_auth(&ev).await {
            self.send_to(
                id,
                Frame::new(
                    "error",
                    serde_json::json!({"ev": ev, "message": "authentication required"}),
                ),
            );
            return;
        }
        // The envelope: the handler receives the sender + the original
        // args. Existing echo handlers pass args through — their
        // results now carry the envelope (the ADR's shape, not a
        // compatibility surface).
        let envelope = serde_json::json!({
            "sender": self.sender_json(device, user),
            "args": args,
        });
        let target = InstanceId { booth_type: ev.clone(), key: InstanceKey::Named("ws".into()) };
        let payload = match self.engine.call(target, &ev, envelope).await {
            Ok(Waited::Done(Ok(v))) => Ok(v),
            Ok(Waited::Done(Err(e))) => Err(e.to_string()),
            // echo booths are hot by construction; Pending = a cold
            // target mounted under a live name (contract violation).
            Ok(Waited::Pending(_)) => Err("cold call not supported on the echo plane".into()),
            Err(e) => Err(e.to_string()),
        };
        match payload {
            Ok(v) => self.send_to(id, Frame::new(format!("{ev}.result"), v)),
            Err(message) => {
                self.send_to(id, Frame::new("error", serde_json::json!({"ev": ev, "message": message})))
            }
        }
    }

    /// Server-side revocation (Phase 1 tail): kill a device's binding
    /// and kick its live connections. Not a client event — the entry
    /// is the admin HTTP face (`POST /admin/revoke`), same accept
    /// loop, same curl-first discipline. Three facts move:
    /// the registry row (binding + trust stamp, `Registry::revoke_device`),
    /// each live connection's auth field (cleared so the fan-out
    /// predicate and per-event auth drop it immediately), and the
    /// socket itself (closed with a `revoked` frame — the client
    /// learns WHY it was disconnected). Returns the number of live
    /// connections kicked.
    pub fn revoke_device(&self, device_id: identity::DeviceId) -> anyhow::Result<usize> {
        self.registry.revoke_device(device_id)?;
        let mut kicked = 0;
        let mut dead = Vec::new();
        let mut conns = self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (id, c) in conns.iter() {
            if c.device == device_id {
                auth_set(&c.user, identity::anonymous());
                let _ = c.tx.send(Frame::new(
                    "revoked",
                    serde_json::json!({
                        "device_id": device_id.to_string(),
                        "reason": "revoked by server",
                    }),
                ));
                dead.push(*id);
                kicked += 1;
            }
        }
        // Drop the senders: the channel closes, the pump exits its
        // recv loop, the socket write half drops (the client sees the
        // close after the `revoked` frame), and the pump's own
        // remove-on-exit has nothing left to remove (we dropped the
        // entries here — the pump's cleanup is idempotent).
        for id in dead {
            conns.remove(&id);
        }
        drop(conns);
        // The renderer's leg carries the same auth field, so it is cleared and kicked by the
        // same act: clearing only the peer table would leave a revoked device watching a
        // browser tab that still believes it is logged in.
        let mut ui = self.ui_conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ui_dead = Vec::new();
        for (id, c) in ui.iter() {
            if c.device == device_id {
                auth_set(&c.user, identity::anonymous());
                ui_dead.push(*id);
                kicked += 1;
            }
        }
        for id in ui_dead {
            ui.remove(&id);
        }
        Ok(kicked)
    }



    /// One login, from both legs: ASK the auth booth, then do the venue's own act. The ask is a CALL,
    /// never an event — an emit is persisted as an MQ row (aura `realm/src/events.rs`) and a payload
    /// that reaches no route is served by the unauthenticated `/admin/dead`, so a password must not
    /// travel that way. The venue holds no password and no verifier; it holds the binding and the
    /// token the proven user id earns.
    pub(crate) async fn login_op(
        &self,
        op: &str,
        device: identity::DeviceId,
        args: &serde_json::Value,
    ) -> anyhow::Result<(identity::UserId, identity::TokenId)> {
        let mut payload = serde_json::Map::new();
        payload.insert("op".into(), serde_json::Value::String(op.to_string()));
        for key in ["login_name", "password", "nickname"] {
            if let Some(v) = text_field(args, key) {
                payload.insert(key.into(), serde_json::Value::String(v));
            }
        }
        let target = InstanceId {
            booth_type: auth::AUTH_BOOTH.to_string(),
            key: InstanceKey::Singleton,
        };
        let answer = match self.engine.call(target, op, serde_json::Value::Object(payload)).await {
            Ok(Waited::Done(Ok(v))) => v,
            Ok(Waited::Done(Err(e))) => return Err(anyhow::anyhow!("{e}")),
            Ok(Waited::Pending(_)) => {
                return Err(anyhow::anyhow!(
                    "auth booth `{}` is cold; no login can be answered here",
                    auth::AUTH_BOOTH
                ))
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "no auth booth `{}` is registered: {e}",
                    auth::AUTH_BOOTH
                ))
            }
        };
        // The booth names the user; the venue does not second-guess an installed component (its code
        // already runs in this realm) and never accepts an anonymous answer as a login.
        let user = answer
            .get("user")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("auth booth answered without a user"))?
            .parse::<identity::UserId>()
            .map_err(|e| anyhow::anyhow!("auth booth answered a malformed user id: {e}"))?;
        let token = self.registry.bind_login(device, user)?;
        Ok((user, token))
    }

    /// signup/login: the booth's answer names the user, then the auth FIELD of this connection is
    /// written (§3 one set — no move, no second collection) and the token rotates. Both answers are
    /// ordinary `<ev>.result` frames; failures are error values.
    async fn auth_event(&self, id: u64, ev: &str, args: serde_json::Value) {
        let device = self.identities(id).0;
        let op = if ev == "signup" { auth::OP_REGISTER } else { auth::OP_VERIFY };
        match self.login_op(op, device, &args).await {
            Ok((user_id, token)) => {
                if let Some(conn) = self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&id) {
                    auth_set(&conn.user, user_id);
                }
                // A signature-in makes its screen owed whatever the account missed while nobody
                // of theirs was awake.
                self.flush_missed(device, user_id).await;
                self.send_to(
                    id,
                    Frame::new(
                        format!("{ev}.result"),
                        serde_json::json!({
                            "user_id": user_id.to_string(),
                            "token": token.to_string(),
                        }),
                    ),
                );
            }
            Err(e) => {
                self.send_to(id, Frame::new("error", serde_json::json!({"ev": ev, "message": e.to_string()})))
            }
        }
    }

    /// The account's devices, shaped for the wire — the one answer no booth can produce, because a
    /// booth cannot enumerate devices. Ids travel as TEXT (a 128-bit id does not fit a JSON
    /// number), `alias` is the name the person gave (empty means "show the id"), and `current`
    /// marks the connection that asked. Anonymous has no list: a device list is a property of an
    /// account, and a login is what binds one.
    pub(crate) fn device_rows(
        &self,
        device: identity::DeviceId,
        user: identity::UserId,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        if identity::is_anonymous(user) {
            anyhow::bail!("authentication required: a device list belongs to a bound user");
        }
        let rows = self.registry.devices_of(user)?;
        Ok(rows
            .into_iter()
            .map(|(id, row)| {
                serde_json::json!({
                    "device": id.to_string(),
                    "alias": row.alias,
                    "first_seen": row.first_seen,
                    "current": id == device,
                })
            })
            .collect())
    }

    /// Does the event require a sender with a bound user? The DECLARATION answers, and the answer
    /// travelled with it: `ReceiveDecl.auth` becomes `Route.auth` at registration, so this is one
    /// route lookup and no schema parse. The event name lives in exactly one place — the `on` /
    /// `@on` that subscribes to it — which is why there is no second table to disagree with.
    /// A name that reaches no route is NOT gated: a public event must never be blocked by an
    /// unresolvable declaration.
    pub(crate) async fn requires_auth(&self, name: &str) -> bool {
        let realm = self.engine.realm.lock().await;
        realm
            .router
            .matches(name)
            .into_iter()
            .any(|route| route.auth)
    }


    /// WHO SENT IT, as the venue knows it: the device, the account, and the account's NAME.
    ///
    /// The name belongs here and only here. A booth writes rows into a memory store that has its own
    /// member rows; if the booth had to copy a login name into that store it would be inventing a
    /// fact it does not own (and would go stale on a rename). The venue is the one layer holding the
    /// account registry, so it says the name in the envelope and the application passes it on —
    /// `display_name` is absent for an anonymous sender, and absent when the registry has no row for
    /// the account (a booth swapped for an external IdP), which the readers treat as "show the id".
    pub(crate) fn sender_json(
        &self,
        device: identity::DeviceId,
        user: identity::UserId,
    ) -> serde_json::Value {
        let mut sender = serde_json::json!({
            "device": device.to_string(),
            "user": (!identity::is_anonymous(user)).then(|| user.to_string()),
        });
        if !identity::is_anonymous(user) {
            if let Ok(Some(name)) = auth::AuthStore::new(self.registry.store()).display_name(user) {
                sender["display_name"] = serde_json::json!(name);
            }
        }
        sender
    }

    fn identities(&self, id: u64) -> (identity::DeviceId, identity::UserId) {
        let guard = self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.get(&id) {
            Some(c) => (c.device, auth_get(&c.user)),
            None => (identity::DeviceId::default(), identity::anonymous()),
        }
    }

    /// Queue one frame to every live connection (optionally only the
    /// authenticated ones — §3's predicate over the ONE set); a closed
    /// sender is pruned in the same pass. Sync: an unbounded send
    /// never blocks.
    fn broadcast_frame(&self, frame: Frame, auth_only: bool) {
        if let Ok(mut conns) = self.conns.lock() {
            // Selection is a predicate READ; retain is only for dead
            // senders. (Conflating them evicted anonymous connections
            // on the first auth-only fan-out.)
            for (_, c) in conns.iter() {
                if !auth_only || !identity::is_anonymous(auth_get(&c.user)) {
                    let _ = c.tx.send(frame.clone());
                }
            }
            conns.retain(|_, c| !c.tx.is_closed());
        }
    }

    fn send_to(&self, id: u64, frame: Frame) {
        if let Some(c) = self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&id) {
            let _ = c.tx.send(frame);
        }
    }
}

/// Parse `k=v&k2=v2` query strings.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::query_param;

    #[test]
    fn query_parsing() {
        assert_eq!(query_param("protocol=json", "protocol"), Some("json"));
        assert_eq!(query_param("a=1&protocol=json", "protocol"), Some("json"));
        assert_eq!(query_param("a=1", "protocol"), None);
        assert_eq!(query_param("device=42&protocol=json", "device"), Some("42"));
    }
}
