//! The `browser` kind: one headless page as an instance, spoken to in
//! computer-use vocabulary — `goto`, `a11y`, `screenshot`, `click`,
//! `type`, `key`. The accessibility tree is the reference surface:
//! reads hand out stable node refs, gestures take them back, which is
//! the contract agents already know from computer-use tooling — and the
//! `text` read flattens the same tree, so the escape-hatch rule (every
//! instance greppable) holds for a page too.
//!
//! Driver law does real work here: the gesture verbs are `driven`, so a
//! human and an agent negotiate one seat for one page — the same
//! authority story as a tty's keys. Reads stay free, as always.
//!
//! Launching is state, not a constructor: `create` returns immediately
//! with status "launching" and a side-feed brings the process up (the
//! host kind's dial pattern) — a browser that cannot start is a readable
//! failure on an instance you can remove, not a failed create.

mod cdp;

use std::sync::{Arc, Mutex};

use myco_instance::{CreateCtx, Instance, Kind, KindSpec, Principal, Shared, VerbError, VerbSpec};
use myco_runtime::Signals;
use serde_json::{Value, json};

static BROWSER_SPEC: KindSpec = KindSpec {
    kind: "browser",
    version: 1,
    doc: "a headless page in computer-use vocabulary: read the a11y tree \
          or a screenshot, drive it by node ref",
    verbs: &[
        VerbSpec::read("about", "status, url, title"),
        VerbSpec::read("screenshot", "the page as {png} (base64)"),
        VerbSpec::read(
            "a11y",
            "the accessibility tree: [{ref, role, name, value?}]",
        ),
        VerbSpec::read("text", "the accessibility tree, flattened to plain text"),
        VerbSpec::driven("goto", "navigate to {url}"),
        VerbSpec::driven("click", "click node {ref} (a ref from a11y)"),
        VerbSpec::driven("type", "insert {text} at the focus"),
        VerbSpec::driven("key", "press {key} (Enter, Tab, Escape, arrows…)"),
    ],
    primary_render: "screenshot",
    recommended_context: "text",
};

/// The a11y projection stops here; a page with more interesting nodes
/// reports how many fell off, so a consumer knows it saw a budgeted view.
const A11Y_NODE_CAP: usize = 800;

#[derive(Default)]
pub struct BrowserKind;

impl Kind for BrowserKind {
    fn spec(&self) -> &'static KindSpec {
        &BROWSER_SPEC
    }

    fn create(
        &self,
        _ctx: &CreateCtx,
        args: Value,
        signals: Signals,
    ) -> Result<Box<dyn Instance>, VerbError> {
        let browser = match args.get("browser").and_then(Value::as_str) {
            Some(path) => path.to_string(),
            None => find_browser().ok_or_else(|| VerbError::BadArgs {
                why: "no browser found — pass {browser} or set MYCO_BROWSER".into(),
            })?,
        };
        let extra: Vec<String> = args
            .get("args")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let start_url = args.get("url").and_then(Value::as_str).map(str::to_string);

        let shared = Shared::new(
            PageState {
                status: "launching",
                detail: String::new(),
            },
            signals.clone(),
        );
        let slot: Arc<Mutex<Option<cdp::Launched>>> = Arc::new(Mutex::new(None));

        let launching = tokio::spawn({
            let shared = shared.clone();
            let slot = Arc::clone(&slot);
            async move {
                match cdp::launch(&browser, &extra, signals).await {
                    Ok(launched) => {
                        if let Some(url) = start_url {
                            let _ = launched
                                .cdp
                                .cmd("Page.navigate", json!({ "url": url }))
                                .await;
                        }
                        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(launched);
                        shared.with(|s| s.status = "ready");
                    }
                    Err(why) => {
                        shared.with(|s| {
                            s.status = "failed";
                            s.detail = why;
                        });
                    }
                }
            }
        });

        Ok(Box::new(Browser {
            shared,
            slot,
            launching,
        }))
    }
}

struct PageState {
    status: &'static str,
    detail: String,
}

struct Browser {
    shared: Shared<PageState>,
    slot: Arc<Mutex<Option<cdp::Launched>>>,
    launching: tokio::task::JoinHandle<()>,
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.launching.abort();
        // Dropping the launched browser kills the process and its tasks.
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

impl Browser {
    /// The live connection, or the refusal that explains the status.
    fn cdp(&self) -> Result<(cdp::Cdp, String), VerbError> {
        let slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(launched) => Ok((launched.cdp.clone(), launched.target_id.clone())),
            None => Err(VerbError::Denied {
                why: self.shared.read(|s| match s.status {
                    "launching" => "the browser is still launching".into(),
                    _ => format!("the browser failed to launch: {}", s.detail),
                }),
            }),
        }
    }
}

#[async_trait::async_trait]
impl Instance for Browser {
    async fn verb(
        &mut self,
        _caller: &Principal,
        verb: &str,
        args: Value,
        _signals: &Signals,
    ) -> Result<Value, VerbError> {
        match verb {
            "about" => {
                let (status, detail) = self.shared.read(|s| (s.status, s.detail.clone()));
                if status != "ready" {
                    return Ok(json!({ "status": status, "detail": detail }));
                }
                let (cdp, target) = self.cdp()?;
                let info = cdp
                    .cmd("Target.getTargetInfo", json!({ "targetId": target }))
                    .await
                    .map_err(failed)?;
                Ok(json!({
                    "status": "ready",
                    "url": info["targetInfo"]["url"],
                    "title": info["targetInfo"]["title"],
                }))
            }
            "screenshot" => {
                let (cdp, _) = self.cdp()?;
                let shot = cdp
                    .cmd("Page.captureScreenshot", json!({ "format": "png" }))
                    .await
                    .map_err(failed)?;
                Ok(json!({ "png": shot["data"] }))
            }
            "a11y" => {
                let (cdp, _) = self.cdp()?;
                Ok(json!({ "nodes": a11y_nodes(&cdp).await? }))
            }
            "text" => {
                let (cdp, _) = self.cdp()?;
                let lines: Vec<String> = a11y_nodes(&cdp)
                    .await?
                    .into_iter()
                    .filter_map(|n| {
                        let name = n["name"].as_str().unwrap_or_default();
                        if name.is_empty() {
                            return None;
                        }
                        let role = n["role"].as_str().unwrap_or_default();
                        Some(match n["value"].as_str() {
                            Some(value) if !value.is_empty() => {
                                format!("{role} {name}: {value}")
                            }
                            _ => format!("{role} {name}"),
                        })
                    })
                    .collect();
                Ok(Value::String(lines.join("\n")))
            }
            "goto" => {
                let url =
                    args.get("url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| VerbError::BadArgs {
                            why: "goto needs {url}".into(),
                        })?;
                let (cdp, _) = self.cdp()?;
                let nav = cdp
                    .cmd("Page.navigate", json!({ "url": url }))
                    .await
                    .map_err(failed)?;
                if let Some(err) = nav["errorText"].as_str().filter(|e| !e.is_empty()) {
                    return Err(VerbError::Failed { why: err.into() });
                }
                Ok(Value::Null)
            }
            "click" => {
                let node =
                    args.get("ref")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| VerbError::BadArgs {
                            why: "click needs {ref} — a node ref from a11y".into(),
                        })?;
                let (cdp, _) = self.cdp()?;
                let model = cdp
                    .cmd("DOM.getBoxModel", json!({ "backendNodeId": node }))
                    .await
                    .map_err(failed)?;
                let quad = model["model"]["content"]
                    .as_array()
                    .filter(|q| q.len() == 8)
                    .ok_or_else(|| VerbError::Failed {
                        why: "that node has no box to click".into(),
                    })?
                    .iter()
                    .filter_map(Value::as_f64)
                    .collect::<Vec<_>>();
                let x = (quad[0] + quad[2] + quad[4] + quad[6]) / 4.0;
                let y = (quad[1] + quad[3] + quad[5] + quad[7]) / 4.0;
                for kind in ["mousePressed", "mouseReleased"] {
                    cdp.cmd(
                        "Input.dispatchMouseEvent",
                        json!({
                            "type": kind, "x": x, "y": y,
                            "button": "left", "clickCount": 1,
                        }),
                    )
                    .await
                    .map_err(failed)?;
                }
                Ok(Value::Null)
            }
            "type" => {
                let text =
                    args.get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| VerbError::BadArgs {
                            why: "type needs {text}".into(),
                        })?;
                let (cdp, _) = self.cdp()?;
                cdp.cmd("Input.insertText", json!({ "text": text }))
                    .await
                    .map_err(failed)?;
                Ok(Value::Null)
            }
            "key" => {
                let key =
                    args.get("key")
                        .and_then(Value::as_str)
                        .ok_or_else(|| VerbError::BadArgs {
                            why: "key needs {key}".into(),
                        })?;
                let (code, text) = match key {
                    "Enter" => (13, Some("\r")),
                    "Tab" => (9, None),
                    "Backspace" => (8, None),
                    "Delete" => (46, None),
                    "Escape" => (27, None),
                    "ArrowLeft" => (37, None),
                    "ArrowUp" => (38, None),
                    "ArrowRight" => (39, None),
                    "ArrowDown" => (40, None),
                    other => {
                        return Err(VerbError::BadArgs {
                            why: format!(
                                "unknown key {other:?} — for characters use type; \
                                 keys are Enter, Tab, Backspace, Delete, Escape, Arrow*"
                            ),
                        });
                    }
                };
                let (cdp, _) = self.cdp()?;
                let mut down = json!({
                    "type": "keyDown", "key": key, "code": key,
                    "windowsVirtualKeyCode": code, "nativeVirtualKeyCode": code,
                });
                if let Some(text) = text {
                    down["text"] = json!(text);
                }
                cdp.cmd("Input.dispatchKeyEvent", down)
                    .await
                    .map_err(failed)?;
                cdp.cmd(
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": "keyUp", "key": key, "code": key,
                        "windowsVirtualKeyCode": code, "nativeVirtualKeyCode": code,
                    }),
                )
                .await
                .map_err(failed)?;
                Ok(Value::Null)
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

fn failed(why: String) -> VerbError {
    VerbError::Failed { why }
}

/// The projected accessibility tree: named or interactive nodes, in
/// document order, each carrying the backend node id as its gesture ref.
async fn a11y_nodes(cdp: &cdp::Cdp) -> Result<Vec<Value>, VerbError> {
    let tree = cdp
        .cmd("Accessibility.getFullAXTree", json!({}))
        .await
        .map_err(failed)?;
    let mut nodes = Vec::new();
    let mut dropped = 0usize;
    for node in tree["nodes"].as_array().into_iter().flatten() {
        if node["ignored"].as_bool().unwrap_or(false) {
            continue;
        }
        let role = node["role"]["value"].as_str().unwrap_or_default();
        let name = node["name"]["value"].as_str().unwrap_or_default();
        if name.is_empty() && !matches!(role, "textbox" | "button" | "link" | "checkbox") {
            continue;
        }
        if nodes.len() == A11Y_NODE_CAP {
            dropped += 1;
            continue;
        }
        let mut row = json!({
            "ref": node["backendDOMNodeId"],
            "role": role,
            "name": name,
        });
        if let Some(value) = node["value"]["value"].as_str() {
            row["value"] = json!(value);
        }
        nodes.push(row);
    }
    if dropped > 0 {
        nodes.push(json!({
            "role": "note",
            "name": format!("{dropped} more nodes beyond the {A11Y_NODE_CAP}-node budget"),
        }));
    }
    Ok(nodes)
}

/// `MYCO_BROWSER`, then the usual names on PATH. No hardcoded paths —
/// an operator with an unusual install names it in the environment.
fn find_browser() -> Option<String> {
    if let Ok(browser) = std::env::var("MYCO_BROWSER")
        && !browser.is_empty()
    {
        return Some(browser);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in ["chromium", "chromium-browser", "google-chrome", "chrome"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate.display().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
