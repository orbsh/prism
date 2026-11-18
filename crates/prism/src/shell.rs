//! The shell (ADR-0019): the frame a page lives in — navigation, the regions islands draw into,
//! and nothing else.
//!
//! It is a FILE, not a booth, and not a Rust function. Two reasons, both of them about who owns what:
//!
//! - A frame is DATA. An application that draws one in code has taken a layout decision that belongs
//!   to the operator (which regions exist, what the navigation says), and the operator can no longer
//!   change it without a rebuild.
//! - A frame is not per-application. Several booths share one page; none of them can see the others,
//!   so none of them can draw the thing they share. The venue draws it once, from a file, before any
//!   application speaks.
//!
//! The contract with those booths is one convention and no configuration: a region is declared as
//! `{"bind": {"value": {"kind": "source", "source": "island:<booth type>"}}}`, and the booth of that
//! type writes its surface into that slot. Nothing maps names to booths elsewhere — the slot name IS
//! the address, which is the same rule the rest of the page follows.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// The marker a region's slot carries. A booth's own slot name is `island:<its type>` — no table to
/// keep in step, and a deployment that renames a booth has exactly one place to rename it.
pub const ISLAND: &str = "island:";

/// One region the frame holds, and what the frame calls it on the page.
///
/// The LABEL lives here because the frame already knows it: a deployment that writes its own
/// navigation should not have to write the same words twice (once in the layout, once in whatever
/// builds the menu). An absent label falls back to the booth name — readable enough to notice, ugly
/// enough to fix.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub booth: String,
    pub label: String,
}

pub struct Shell {
    path: PathBuf,
    root: Value,
}

impl Shell {
    /// Open a declared frame. A path that does not resolve to one accrete node is a boot error:
    /// a deployment that meant to have navigation and got a page with none is worse than one that
    /// said nothing, because it looks finished.
    pub fn open(file: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(file).map_err(|e| {
            anyhow::anyhow!("PRISM_SHELL '{}' is unreadable: {e}", file.display())
        })?;
        let root: Value = serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!("PRISM_SHELL '{}' is not JSON: {e}", file.display())
        })?;
        if !root.is_object() {
            anyhow::bail!(
                "PRISM_SHELL '{}' must be ONE accrete node (an object), not a list of actions",
                file.display()
            );
        }
        Ok(Self {
            path: file.to_path_buf(),
            root,
        })
    }

    /// Read the frame from `PRISM_SHELL`. Absent is legal and means the old arrangement: whichever
    /// application answers `ui:connect` draws the whole page.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        match std::env::var("PRISM_SHELL").ok().filter(|s| !s.trim().is_empty()) {
            Some(path) => Ok(Some(Self::open(Path::new(path.trim()))?)),
            None => Ok(None),
        }
    }

    /// The frame as the renderer takes it: one `create`, the document as its data. A `create` and
    /// not a `patch`: this is the page's root, and it exists before anything has drawn into it.
    pub fn create(&self) -> Value {
        json!({ "action": "create", "slot": "", "data": self.root })
    }

    /// The regions this frame declares, in document order. The venue needs them for two things it
    /// cannot do without: naming an application's slots (ADR-0019), and telling the page what its
    /// navigation should contain.
    pub fn regions(&self) -> Vec<Region> {
        // A frame DECLARES its regions in one place (`regions: [{booth, label}]`), and nothing is
        // rendered from that declaration: the pager's rows are supplied by the venue, because a
        // page's rows can only come from a list slot (fluxen \`pages\`) — a static child would be a
        // second way to put something on screen and the wrong one.
        //
        // The older spelling — an actual node bound to \`island:<booth>\` — is still read, so a frame
        // written before this field existed keeps working.
        if let Some(list) = self.root.get("regions").and_then(Value::as_array) {
            return list
                .iter()
                .filter_map(|r| {
                    let booth = r.get("booth")?.as_str()?.to_string();
                    let label = r
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or(&booth)
                        .to_string();
                    Some(Region { booth, label })
                })
                .collect();
        }
        fn walk(node: &Value, out: &mut Vec<Region>) {
            match node {
                Value::Object(map) => {
                    let source = map
                        .get("bind")
                        .and_then(|b| b.get("value"))
                        .and_then(|v| v.get("source"))
                        .and_then(Value::as_str);
                    if let Some(booth) = source.and_then(|s| s.strip_prefix(ISLAND)) {
                        if !out.iter().any(|r| r.booth == booth) {
                            let label = map
                                .get("attrs")
                                .and_then(|a| a.get("label"))
                                .and_then(Value::as_str)
                                .unwrap_or(booth);
                            out.push(Region {
                                booth: booth.to_string(),
                                label: label.to_string(),
                            });
                        }
                    }
                    for value in map.values() {
                        walk(value, out);
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        walk(item, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }

    /// The booth names alone — what the view leg needs to namespace an application's slots.
    pub fn islands(&self) -> Vec<String> {
        self.regions().into_iter().map(|r| r.booth).collect()
    }

    /// One boot-log line: the file, and what it promises to hold. A operator reading a start-up
    /// should be able to answer "does this deployment have a frame, and whose regions are in it"
    /// without opening the file.
    pub fn describe(&self) -> String {
        let islands = self.islands();
        let regions = if islands.is_empty() {
            "no islands".to_string()
        } else {
            format!("islands: {}", islands.join(", "))
        };
        format!(
            "prism: page frame ← {} ({regions})",
            self.path.display()
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn write(dir: &Path, body: &str) -> PathBuf {
        let file = dir.join("shell.json");
        std::fs::write(&file, body).unwrap();
        file
    }

    /// The slot name is the address: an island is discovered by reading the frame, never by a table
    /// that could disagree with it.
    #[test]
    fn islands_are_found_by_reading_the_frame() {
        let dir = std::env::temp_dir().join(format!("prism-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = write(
            &dir,
            r#"{"type": "frame","children":[
                {"type": "frame","bind":{"value":{"kind":"source","source":"auth"}}},
                {"type": "frame","bind":{"value":{"kind":"source","source":"island:gravity_turn"}}},
                {"type": "frame","children":[
                    {"type": "frame","attrs":{"label":"资源"},
                     "bind":{"value":{"kind":"source","source":"island:admin"}}}
                ]}
            ]}"#,
        );
        let shell = Shell::open(&file).unwrap();
        assert_eq!(shell.islands(), vec!["gravity_turn", "admin"]);
        // The navigation's words live in the frame too: a deployment that writes its own labels
        // should not write them twice, and a region without one falls back to its booth name —
        // readable enough to notice, ugly enough to fix.
        let regions = shell.regions();
        assert_eq!(regions[0].label, "gravity_turn", "no label: the booth's own name");
        assert_eq!(regions[1].label, "资源");
        let frame = shell.create();
        assert_eq!(frame["action"], "create");
        assert_eq!(frame["slot"], "");
        assert!(
            frame["data"]["children"].as_array().unwrap().len() == 3,
            "the document travels unchanged — the venue wraps it, never rewrites it"
        );
    }

    /// A list of actions is not a frame. Accepting one would put a page on screen whose root is an
    /// operation, which the renderer draws as nothing and reports as nothing.
    #[test]
    fn a_file_of_actions_is_refused() {
        let dir = std::env::temp_dir().join(format!("prism-shell-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = write(&dir, r#"[{"action":"create","slot":"","data":{}}]"#);
        let err = match Shell::open(&file) {
            Ok(_) => panic!("a list of actions is not a frame"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("ONE accrete node"), "{err}");
    }

    /// The declared form: `regions: [{booth, label}]`, which renders nothing. The pager's rows come
    /// from a list slot (fluxen `pages`), so a declaration that also drew something would be a
    /// second way to put a page on the screen — and the one that cannot be switched away from.
    #[test]
    fn a_frame_can_declare_its_regions_without_drawing_them() {
        let dir = std::env::temp_dir().join(format!("prism-shell-regions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = write(
            &dir,
            r#"{"type": "frame","regions":[{"booth":"gravity-model","label":"对话"}],
                "children":[{"type":"pages","bind":{"value":{"kind":"source","source":"pages"}}}]}"#,
        );
        let shell = Shell::open(&file).unwrap();
        assert_eq!(
            shell.regions(),
            vec![Region { booth: "gravity-model".into(), label: "对话".into() }]
        );
        assert_eq!(shell.islands(), vec!["gravity-model"]);
    }

    /// The SHIPPED frame opens. It is the file a deployment is told to point `PRISM_SHELL` at, so a
    /// typo in it is not a documentation problem — it is the reference deployment failing to boot.
    #[test]
    fn the_reference_frame_opens() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/shell.json");
        let shell = Shell::open(&path).expect("the reference frame is valid");
        let islands = shell.islands();
        assert!(!islands.is_empty(), "the reference frame declares a region");
        assert!(shell.describe().contains("page frame"));
    }
}