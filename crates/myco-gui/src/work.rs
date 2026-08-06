//! The work panel's machinery: panel state, xterm-style key translation,
//! the styled screen renderer, and [`WorkView`] — the one window chrome a
//! shell terminal and a subagent chat share (head, lock button, rename,
//! scroll-pinned body, line-mode input row).

use myco_api as api;
use web_sys::HtmlInputElement;
use yew::prelude::*;

/// The work panel's mutable surface: live shells and subagent children, the
/// opened terminals' scrollback, and the opened chats' transcripts. A
/// `use_mut_ref` cell (plus a revision counter for renders), because its
/// writers are long-lived tasks and late-completing requests — exactly the
/// closures a captured `UseStateHandle` would go stale in.
#[derive(Default)]
pub(crate) struct PanelBuf {
    pub(crate) shells: Vec<api::Shell>,
    pub(crate) terms: std::collections::HashMap<String, Term>,
    /// Each pty session's latest rendered screen (the snapshot is the view).
    pub(crate) screens: std::collections::HashMap<String, api::ShellScreen>,
    /// Live subagent children — the `subagent` tool's sessions, as tabs.
    pub(crate) subs: Vec<api::Subagent>,
    /// Each opened child's transcript so far (`poll?since=` pages).
    pub(crate) chats: std::collections::HashMap<String, Chat>,
}

/// Keystrokes on their way to one shell: a byte buffer drained by a single
/// in-flight POST at a time, so bytes arrive in the order they were typed
/// however fast the typist outruns the network.
#[derive(Default)]
pub(crate) struct TypeQueue {
    pub(crate) buf: String,
    pub(crate) busy: bool,
}

/// One subagent chat's accumulated entries.
#[derive(Default)]
pub(crate) struct Chat {
    pub(crate) since: usize,
    pub(crate) entries: Vec<api::Entry>,
}

/// One tab of the work panel: a shell terminal or a subagent chat.
#[derive(Clone, PartialEq)]
pub(crate) enum Tab {
    Shell { host: String, id: String },
    Sub { id: String },
}

/// The panel key for one shell: shells are per host, so `(host, id)` is the
/// identity everywhere the GUI files one.
pub(crate) fn term_key(host: &str, id: &str) -> String {
    format!("{host}/{id}")
}

/// One shell's accumulated scrollback view (piped sessions) or latest screen
/// snapshot (pty sessions — see `replace`).
#[derive(Default)]
pub(crate) struct Term {
    /// Absolute offset to tail from next.
    pub(crate) from: u64,
    pub(crate) text: String,
}

/// Keep a terminal's local text bounded; the server ring is the real cap.
pub(crate) const TERM_TEXT_CAP: usize = 128 * 1024;

impl Term {
    pub(crate) fn absorb(&mut self, chunk: &api::ShellTailChunk) {
        if chunk.from > self.from && !self.text.is_empty() {
            self.text.push_str("\n[… output skipped …]\n");
        }
        self.text.push_str(&chunk.data);
        self.from = chunk.end;
        if self.text.len() > TERM_TEXT_CAP {
            let cut = self.text.len() - TERM_TEXT_CAP;
            // Cut on a char boundary; exactness does not matter here.
            let cut = (cut..self.text.len())
                .find(|i| self.text.is_char_boundary(*i))
                .unwrap_or(0);
            self.text.drain(..cut);
        }
    }
}

/// Translate one keydown into the bytes a terminal sends, or `None` for
/// keys the browser should keep. Follows xterm's conventions, including the
/// copy/paste carve-out: `Ctrl+Shift+C`/`Ctrl+Shift+V` stay the browser's,
/// while plain `Ctrl+C`/`Ctrl+V` are the control bytes a terminal owns.
pub(crate) fn key_bytes(e: &KeyboardEvent, app_cursor: bool) -> Option<String> {
    if e.meta_key() {
        return None;
    }
    let key = e.key();
    if e.ctrl_key() && e.shift_key() && (key == "C" || key == "V") {
        return None;
    }
    if e.ctrl_key() && !e.alt_key() {
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let byte = match c {
                'a'..='z' => (c as u8) - b'a' + 1,
                'A'..='Z' => (c as u8) - b'A' + 1,
                '@' | ' ' => 0,
                '[' => 0x1b,
                '\\' => 0x1c,
                ']' => 0x1d,
                '^' => 0x1e,
                '_' => 0x1f,
                _ => return None,
            };
            return Some(char::from(byte).to_string());
        }
        return None;
    }
    let arrow = |c: char| {
        Some(if app_cursor {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        })
    };
    let base = match key.as_str() {
        "Enter" => Some("\r".to_string()),
        "Backspace" => Some("\x7f".to_string()),
        "Tab" => Some("\t".to_string()),
        "Escape" => Some("\x1b".to_string()),
        "ArrowUp" => arrow('A'),
        "ArrowDown" => arrow('B'),
        "ArrowRight" => arrow('C'),
        "ArrowLeft" => arrow('D'),
        "Home" => Some("\x1b[H".to_string()),
        "End" => Some("\x1b[F".to_string()),
        "PageUp" => Some("\x1b[5~".to_string()),
        "PageDown" => Some("\x1b[6~".to_string()),
        "Insert" => Some("\x1b[2~".to_string()),
        "Delete" => Some("\x1b[3~".to_string()),
        "F1" => Some("\x1bOP".to_string()),
        "F2" => Some("\x1bOQ".to_string()),
        "F3" => Some("\x1bOR".to_string()),
        "F4" => Some("\x1bOS".to_string()),
        "F5" => Some("\x1b[15~".to_string()),
        "F6" => Some("\x1b[17~".to_string()),
        "F7" => Some("\x1b[18~".to_string()),
        "F8" => Some("\x1b[19~".to_string()),
        "F9" => Some("\x1b[20~".to_string()),
        "F10" => Some("\x1b[21~".to_string()),
        "F11" => Some("\x1b[23~".to_string()),
        "F12" => Some("\x1b[24~".to_string()),
        k if k.chars().count() == 1 => Some(k.to_string()),
        _ => None,
    };
    match (base, e.alt_key()) {
        // Alt is the ESC prefix, terminal-style.
        (Some(b), true) => Some(format!("\x1b{b}")),
        (b, false) => b,
        (None, true) => None,
    }
}

/// The styled terminal grid: one `div` per row, one `span` per same-styled
/// run, colors resolved server-side. The cursor cell renders inverse.
pub(crate) fn render_screen(screen: &api::ShellScreen) -> Html {
    let mut runs = screen.runs.iter().peekable();
    let rows = (0..screen.rows).map(|row| {
        let mut spans: Vec<Html> = Vec::new();
        while let Some(r) = runs.peek() {
            if r.row != row {
                break;
            }
            let r = runs.next().expect("peeked");
            // Inverse swaps the pair; a missing side swaps in the terminal
            // default. The cursor is its own run and styles itself.
            let (fg, bg) = if r.inverse && !r.cursor {
                (
                    Some(r.bg.clone().unwrap_or_else(|| "var(--term-bg)".into())),
                    Some(r.fg.clone().unwrap_or_else(|| "var(--fg)".into())),
                )
            } else {
                (r.fg.clone(), r.bg.clone())
            };
            let mut style = String::new();
            if !r.cursor {
                if let Some(fg) = fg {
                    style.push_str(&format!("color:{fg};"));
                }
                if let Some(bg) = bg {
                    style.push_str(&format!("background:{bg};"));
                }
            }
            if r.bold {
                style.push_str("font-weight:bold;");
            }
            if r.italic {
                style.push_str("font-style:italic;");
            }
            if r.underline {
                style.push_str("text-decoration:underline;");
            }
            spans.push(html! {
                <span class={ classes!(r.cursor.then_some("cur")) } style={style}>
                    { r.text.clone() }
                </span>
            });
        }
        html! { <div class="trow">{ for spans.into_iter() }</div> }
    });
    html! { <div class="term-screen">{ for rows }</div> }
}

/// The active tab's window — a shell terminal or a subagent chat behind one
/// piece of chrome. The head names the thing and carries the keyboard-lock
/// affordance as an explicit button (a double-click on a header that also
/// selects was two gestures fighting over one element).
///
/// Two input modes, by what the thing *is*:
/// - a pty session is a real terminal: while the user holds the lock the
///   body itself has keyboard focus and every keystroke goes down raw —
///   Ctrl+C, Esc, arrows, F-keys, Alt chords, paste — so full-screen
///   programs behave. The window is also fitted to the panel (resize →
///   SIGWINCH).
/// - a piped shell or a subagent chat is line-based: an input row appears
///   while the lock is held (Enter sends, Esc hands back).
#[derive(Properties, PartialEq)]
pub(crate) struct WorkViewProps {
    /// Something is running inside (a command, a turn) — the disc state.
    pub(crate) running: bool,
    pub(crate) title: String,
    /// Small annotation after the title: `@host`, a model key.
    #[prop_or_default]
    pub(crate) badge: Option<String>,
    /// Dim detail text in the head (a shell's command line).
    #[prop_or_default]
    pub(crate) detail: String,
    pub(crate) user_locked: bool,
    /// Prose content (a chat) rather than preformatted terminal text.
    #[prop_or_default]
    pub(crate) chat: bool,
    /// A pty session: raw keystrokes into the body, no input row.
    #[prop_or_default]
    pub(crate) pty: bool,
    /// DECCKM from the screen: arrows send SS3 instead of CSI.
    #[prop_or_default]
    pub(crate) app_cursor: bool,
    /// Point the keyboard: `true` takes it, `false` hands it back.
    pub(crate) on_lock: Callback<bool>,
    /// Rename affordance (shells only; a subagent's name is its id).
    #[prop_or_default]
    pub(crate) on_rename: Option<Callback<()>>,
    /// A line submitted from the input row (line mode).
    #[prop_or_default]
    pub(crate) on_send: Callback<String>,
    /// Raw bytes typed or pasted into the terminal (pty mode).
    #[prop_or_default]
    pub(crate) on_keys: Callback<String>,
    /// The cols×rows that currently fit the body (pty mode, lock held).
    #[prop_or_default]
    pub(crate) on_resize: Callback<(u16, u16)>,
    #[prop_or_default]
    pub(crate) placeholder: AttrValue,
    #[prop_or_default]
    pub(crate) children: Html,
}

#[function_component(WorkView)]
pub(crate) fn work_view(props: &WorkViewProps) -> Html {
    let input = use_node_ref();
    let body = use_node_ref();
    let probe = use_node_ref();
    let raw_mode = props.pty && props.user_locked;

    // Ride the bottom on every render — for the scrolling views. A pty
    // screen is a fixed grid; there is nothing to chase.
    {
        let body = body.clone();
        let scrolling = !props.pty;
        use_effect(move || {
            if scrolling && let Some(el) = body.cast::<web_sys::Element>() {
                el.set_scroll_top(el.scroll_height());
            }
        });
    }

    // Taking a terminal's keyboard focuses it, so typing starts immediately.
    {
        let body = body.clone();
        use_effect_with(raw_mode, move |raw| {
            if *raw && let Some(el) = body.cast::<web_sys::HtmlElement>() {
                let _ = el.focus();
            }
        });
    }

    // Fit the terminal to the panel: measure a 10-char probe for the cell
    // size, emit the cols×rows that fit. The parent dedupes and resizes the
    // pty; runs every render so panel-size changes are picked up on the
    // next tick.
    {
        let body = body.clone();
        let probe = probe.clone();
        let on_resize = props.on_resize.clone();
        use_effect(move || {
            if raw_mode
                && let Some(p) = probe.cast::<web_sys::HtmlElement>()
                && let Some(b) = body.cast::<web_sys::HtmlElement>()
            {
                let cell_w = p.offset_width() as f64 / 10.0;
                let cell_h = p.offset_height() as f64;
                if cell_w > 0.0 && cell_h > 0.0 {
                    let cols = ((b.client_width() as f64 - 18.0) / cell_w) as i64;
                    let rows = ((b.client_height() as f64 - 10.0) / cell_h) as i64;
                    if cols >= 10 && rows >= 3 {
                        on_resize.emit((cols.min(500) as u16, rows.min(500) as u16));
                    }
                }
            }
        });
    }

    let on_lock_click = {
        let on_lock = props.on_lock.clone();
        let take = !props.user_locked;
        Callback::from(move |_: MouseEvent| on_lock.emit(take))
    };
    // Line mode: Enter sends the line, Esc hands the keyboard back.
    let on_line_key = {
        let input = input.clone();
        let on_send = props.on_send.clone();
        let on_lock = props.on_lock.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                e.prevent_default();
                if let Some(field) = input.cast::<HtmlInputElement>() {
                    let text = field.value();
                    field.set_value("");
                    on_send.emit(text);
                }
            } else if e.key() == "Escape" {
                e.prevent_default();
                on_lock.emit(false);
            }
        })
    };
    // Raw mode: the body is the terminal. (Esc types ESC here — handing the
    // keyboard back is the head button.)
    let on_raw_key = {
        let on_keys = props.on_keys.clone();
        let app_cursor = props.app_cursor;
        Callback::from(move |e: KeyboardEvent| {
            if let Some(bytes) = key_bytes(&e, app_cursor) {
                e.prevent_default();
                on_keys.emit(bytes);
            }
        })
    };
    let on_paste = {
        let on_keys = props.on_keys.clone();
        Callback::from(move |e: Event| {
            use wasm_bindgen::JsCast;
            let Ok(e) = e.dyn_into::<web_sys::ClipboardEvent>() else {
                return;
            };
            if let Some(data) = e.clipboard_data()
                && let Ok(text) = data.get_data("text")
                && !text.is_empty()
            {
                e.prevent_default();
                on_keys.emit(text);
            }
        })
    };

    html! {
        <div class={ classes!("work-view", props.user_locked.then_some("work-view-user")) }>
            <div class="work-view-head">
                <span class={ if props.running { "tool-disc tool-disc-running" }
                              else { "tool-disc tool-disc-ok" } }>{ "●" }</span>
                <span class="tool-name">{ &props.title }</span>
                { if let Some(b) = &props.badge { html! {
                    <span class="host-badge">{ b }</span>
                } } else { html!{} } }
                <span class="dim">{ &props.detail }</span>
                { if let Some(rename) = props.on_rename.clone() { html! {
                    <button class="linkish rename-badge" title="rename this terminal"
                            onclick={Callback::from(move |_: MouseEvent| rename.emit(()))}>
                        { "✎" }
                    </button>
                } } else { html!{} } }
                <button class="linkish lock-badge" onclick={on_lock_click}>
                    { if props.user_locked { "⌨ yours — hand back" } else { "⌨ agent — take keyboard" } }
                </button>
            </div>
            <div ref={body}
                 class={ classes!("work-body", props.chat.then_some("chat-body"),
                                  raw_mode.then_some("work-body-raw")) }
                 tabindex={ raw_mode.then_some("0") }
                 onkeydown={ raw_mode.then_some(on_raw_key) }
                 onpaste={ raw_mode.then_some(on_paste) }>
                <span class="term-probe" ref={probe} aria-hidden="true">{ "0000000000" }</span>
                { props.children.clone() }
            </div>
            { if props.user_locked && !props.pty { html! {
                <div class="term-input-row">
                    <input ref={input} type="text" placeholder={props.placeholder.clone()}
                           onkeydown={on_line_key} />
                </div>
            } } else { html!{} } }
        </div>
    }
}
