//! Hosted mode: WordCraft running in a frame of another app (`?host` in the URL), such as a notes
//! app that edits Word documents with it. The host hands documents in and takes saves out with
//! `postMessage`, in place of the browser's file pickers and downloads.
//!
//! Every document the host opens carries a token (`doc`) that WordCraft repeats on everything it
//! says about that document, so a late message about an earlier document is never taken for the
//! current one.
//!
//! From the host (only the frame's own parent window is listened to):
//! - `{type: "open", doc, name, bytes: Uint8Array}` opens a document (as Open would).
//! - `{type: "command", doc, id}` runs one of [`crate::host_rules::HOST_COMMANDS`] on that document.
//! - `{type: "theme", value: "light" | "dark" | "system"}` sets the appearance, to match the host's.
//! - `{type: "page", dark: bool}` shows the page dark (View › Switch Modes); a view setting, so the file is unchanged.
//!
//! To the host:
//! - `{type: "ready"}` once the app is running and listening.
//! - `{type: "opened", doc, ok, ms, error?}` after an open.
//! - `{type: "drawn", doc, ms, frames, frameMs, renderMs, layoutMs}` once every page on screen has been drawn after an open.
//! - `{type: "save", doc, name, bytes: Uint8Array}` when Save or Save As writes the document.
//! - `{type: "dirty", doc, dirty: bool}` when unsaved changes appear or go away.
//!
//! Messages are acted on when they arrive, not on the next frame: a hidden page draws no frames,
//! and a host asking for a save as it hides or quits must still get one.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wordcraft_ui_egui::WordApp;

use crate::host_rules::{clean_name, host_may_run};

/// Largest document the host may hand in (as Open allows for a picked file).
const MAX_OPEN_BYTES: u32 = 200 << 20;
/// Longest document token kept from the host.
const MAX_DOC_CHARS: usize = 100;

/// What the page knows about its host: the document it is showing, and the unsaved-changes state
/// last reported (so it is said once per change, not every frame).
#[derive(Default)]
pub struct HostState {
    doc: RefCell<String>,
    last_dirty: Cell<Option<bool>>,
    drawing: RefCell<Option<Drawing>>,
}

struct Drawing {
    doc: String,
    since: f64,
    canvas_frames: u64,
    render_ms: f64,
    frames: u32,
    frame_ms: f64,
}

pub fn after_frame(state: &HostState, app: &mut WordApp) {
    let mut drawing = state.drawing.borrow_mut();
    let Some(d) = drawing.as_mut() else { return };
    d.frames = d.frames.saturating_add(1);
    d.frame_ms += app.frame_ms;
    if app.canvas.frames <= d.canvas_frames || app.canvas.pages_pending {
        return;
    }
    let layout_ms = app.session.layout().ms;
    post(&[
        ("type", "drawn".into()),
        ("doc", d.doc.clone().into()),
        ("ms", (wordcraft_ui_egui::now_ms() - d.since).into()),
        ("frames", d.frames.into()),
        ("frameMs", d.frame_ms.into()),
        ("renderMs", (app.canvas.render_total_ms - d.render_ms).into()),
        ("layoutMs", layout_ms.into()),
    ]);
    *drawing = None;
}

/// Whether this page runs hosted: `host` is a key of the URL's query, and the page is in a frame.
pub fn hosted() -> bool {
    let Some(window) = web_sys::window() else { return false };
    let asked = window.location().search().ok().and_then(|q| web_sys::UrlSearchParams::new_with_str(&q).ok()).map(|p| p.has("host")).unwrap_or(false);
    asked && parent(&window).is_some()
}

/// The host window, when this page is in a frame.
fn parent(window: &web_sys::Window) -> Option<web_sys::Window> {
    let parent = window.parent().ok().flatten()?;
    if js_sys::Object::is(parent.as_ref(), window.as_ref()) { None } else { Some(parent) }
}

/// Sends a message to the host. A page that is not hosted sends nothing.
pub fn post(fields: &[(&str, JsValue)]) {
    let Some(window) = web_sys::window() else { return };
    let Some(host) = parent(&window) else { return };
    let msg = js_sys::Object::new();
    for (k, v) in fields {
        let _ = js_sys::Reflect::set(&msg, &JsValue::from_str(k), v);
    }
    // The host page may be a file: page, whose origin can't be named here; it only ever gets its own
    // document back.
    if let Err(e) = host.post_message(&msg, "*") {
        log::error!("couldn't reach the host: {e:?}");
    }
}

/// Hands a saved document to the host (`download` in hosted mode), tagged with its document.
pub fn save(state: &HostState, path: &str, bytes: &[u8]) -> Result<(), String> {
    let name = clean_name(path).unwrap_or_else(|| "Document.docx".into());
    let doc = state.doc.borrow().clone();
    post(&[("type", "save".into()), ("doc", doc.into()), ("name", name.into()), ("bytes", js_sys::Uint8Array::from(bytes).into())]);
    Ok(())
}

/// Tells the host about unsaved changes, once per change.
pub fn report_dirty(state: &HostState, dirty: bool) {
    if state.last_dirty.get() == Some(dirty) {
        return;
    }
    state.last_dirty.set(Some(dirty));
    let doc = state.doc.borrow().clone();
    post(&[("type", "dirty".into()), ("doc", doc.into()), ("dirty", dirty.into())]);
}

/// A command's outcome for the host: a result still waiting on the user ("pending") did not happen.
fn settled(result: Result<serde_json::Value, String>) -> Result<(), String> {
    let value = result?;
    match value.get("pending") {
        Some(p) => Err(format!("WordCraft is waiting for an answer ({p})")),
        None => Ok(()),
    }
}

/// Listens to the host and acts on its messages at once, on `app`.
pub fn listen(app: Rc<RefCell<WordApp>>, state: Rc<HostState>, ctx: egui::Context) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let host = parent(&window).ok_or("not in a frame")?;
    let on_message = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
        // Only the frame's own host is listened to.
        let from_host = e.source().map(|s| js_sys::Object::is(s.as_ref(), host.as_ref())).unwrap_or(false);
        if !from_host {
            return;
        }
        let data = e.data();
        let field = |k: &str| js_sys::Reflect::get(&data, &JsValue::from_str(k)).ok();
        let text = |k: &str| field(k).and_then(|v| v.as_string());
        let kind = text("type").unwrap_or_default();
        let doc: String = text("doc").unwrap_or_default().chars().take(MAX_DOC_CHARS).collect();
        // The page is between frames whenever a message arrives, so the app is free; if it is not
        // (a message during a save), the message is dropped and the host's timeout reports it.
        let Ok(mut app) = app.try_borrow_mut() else {
            log::warn!("host: busy, {kind} dropped");
            return;
        };
        match kind.as_str() {
            "open" => {
                let started = wordcraft_ui_egui::now_ms();
                let opened = (|| {
                    let name = text("name").and_then(|n| clean_name(&n)).ok_or("the document has no name")?;
                    let bytes = field("bytes").and_then(|v| v.dyn_into::<js_sys::Uint8Array>().ok()).ok_or("the document has no bytes")?;
                    if bytes.length() > MAX_OPEN_BYTES {
                        return Err("the document is larger than 200 MB".to_string());
                    }
                    let data = wordcraft_engine::cmd::insert::base64_encode(&bytes.to_vec());
                    // `execute`, not `run`: the host decides about unsaved changes, so no "Save changes?" or
                    // password dialog may take the place of the open.
                    settled(app.execute("file.open", serde_json::json!({"path": name, "data": data})))
                })();
                state.last_dirty.set(None);
                let ms = wordcraft_ui_egui::now_ms() - started;
                match opened {
                    Ok(()) => {
                        *state.doc.borrow_mut() = doc.clone();
                        *state.drawing.borrow_mut() = Some(Drawing {
                            doc: doc.clone(),
                            since: started,
                            canvas_frames: app.canvas.frames,
                            render_ms: app.canvas.render_total_ms,
                            frames: 0,
                            frame_ms: 0.0,
                        });
                        post(&[("type", "opened".into()), ("doc", doc.into()), ("ok", true.into()), ("ms", ms.into())]);
                    }
                    Err(err) => {
                        // Whatever is still on show is no document of the host's: its commands are ignored.
                        state.doc.borrow_mut().clear();
                        *state.drawing.borrow_mut() = None;
                        log::warn!("host: open failed: {err}");
                        post(&[("type", "opened".into()), ("doc", doc.into()), ("ok", false.into()), ("error", err.into())]);
                    }
                }
                ctx.request_repaint();
            }
            "command" => {
                let Some(id) = text("id") else { return };
                if doc.is_empty() || doc != *state.doc.borrow() {
                    log::warn!("host: {id} for a document that is no longer open; ignored");
                } else if !host_may_run(&id) {
                    log::warn!("host: command {id} is not one a host may run");
                } else if let Err(err) = settled(app.execute(&id, serde_json::json!({}))) {
                    log::error!("host command {id} failed: {err}");
                }
                ctx.request_repaint();
            }
            "theme" => {
                let Some(value) = text("value") else { return };
                if let Err(err) = app.execute("ui.theme", serde_json::json!({"value": value})) {
                    log::warn!("host: theme {value:?}: {err}");
                }
                ctx.request_repaint();
            }
            "page" => {
                let Some(dark) = field("dark").and_then(|v| v.as_bool()) else { return };
                if let Err(err) = app.execute("view.darkMode", serde_json::json!({"value": dark})) {
                    log::warn!("host: dark page {dark}: {err}");
                }
                ctx.request_repaint();
            }
            other => log::warn!("host: unknown message {other:?}"),
        }
    });
    window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).map_err(|e| format!("{e:?}"))?;
    // The listener lives as long as the page.
    on_message.forget();
    Ok(())
}
