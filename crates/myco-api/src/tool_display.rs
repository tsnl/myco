//! How a tool call is shown to a reader.
//!
//! Each tool gets to decide how its own arguments read, because the useful
//! rendering is tool-specific: `bash` wants its command laid out like shell,
//! `text_editor` wants a path and a body. [`for_tool`] is the lookup; the
//! generic renderer is the fallback for anything without an opinion.
//!
//! **Why this lives in the wire crate rather than beside the tool
//! implementations.** The tools are in `myco-machines`, which links tokio,
//! process spawning and ssh — it cannot be compiled to wasm, so the browser
//! can never call it. Rendering is also width-dependent, and the width is a
//! property of the reader's window, which the server does not know. So the
//! display *rule* is data that both sides can hold, and it lives here where
//! the GUI, the CLI, and the tool services can all reach the same one.
//!
//! ## The YAML dialect
//!
//! Strings are always block scalars (`|`). Uniformly, whatever their length:
//! a reader never has to work out whether something is quoted, escaped, or
//! re-flowed, and content is reproduced exactly as it was sent. Only numbers,
//! booleans and null appear inline, where there is nothing to be ambiguous
//! about.
//!
//! Nothing here is ever parsed back. It is a reading format.

use serde_json::Value;

/// Column tool arguments aim to fit inside.
pub const TOOL_DISPLAY_WIDTH: usize = 76;

/// How one tool's arguments are rendered.
pub trait ToolDisplay: Sync {
    /// The YAML body for this call, wrapped to `width` where wrapping applies.
    fn yaml(&self, input: &Value, width: usize) -> String;
}

/// The display for `tool`, falling back to the generic renderer.
///
/// Matching on the name is the seam: a tool service and its display agree by
/// the name the model calls, which is the only identity that crosses the wire.
pub fn for_tool(tool: &str) -> &'static dyn ToolDisplay {
    match tool {
        "bash" => &Bash,
        _ => &Generic,
    }
}

/// The default: every value rendered structurally, nothing reformatted.
pub struct Generic;

impl ToolDisplay for Generic {
    fn yaml(&self, input: &Value, _width: usize) -> String {
        let mut out = String::new();
        emit(input, 0, &mut out, &|s, indent, out| {
            block_scalar(s, indent, out)
        });
        trim_trailing_newlines(out)
    }
}

/// `bash`: the command laid out the way a shell script would be.
///
/// A one-line pipeline is hard to read and hard to check, and it is the thing
/// people most often want to check. Breaking after each `&&`, `||`, `|` or `;`
/// puts one step per line; over-long steps continue with a trailing `\`.
///
/// Both are real shell continuations, not display marks: the command stays
/// exactly as valid, and as copy-pasteable, as the one the model sent.
pub struct Bash;

impl ToolDisplay for Bash {
    fn yaml(&self, input: &Value, width: usize) -> String {
        let mut out = String::new();
        emit(input, 0, &mut out, &move |s, indent, out| {
            // Only the command itself is shell; everything else is data.
            block_scalar(
                &format_shell(s, width.saturating_sub(indent + 2)),
                indent,
                out,
            )
        });
        let mut out = trim_trailing_newlines(out);
        // A tool with exactly one interesting key reads better without the
        // structure around it, but keep the key when there is more than one.
        if let Value::Object(map) = input
            && map.len() == 1
            && let Some(cmd) = map.get("command").and_then(Value::as_str)
        {
            out = format_shell(cmd, width);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The emitter
// ---------------------------------------------------------------------------

/// How a string scalar is written out. Tool displays supply this so `bash` can
/// reformat its command while everything else is reproduced verbatim.
type ScalarWriter<'a> = dyn Fn(&str, usize, &mut String) + 'a;

fn emit(v: &Value, indent: usize, out: &mut String, scalar: &ScalarWriter<'_>) {
    let pad = " ".repeat(indent);
    match v {
        Value::Object(map) if !map.is_empty() => {
            for (k, val) in map {
                out.push_str(&format!("{pad}{k}:"));
                emit_value(val, indent, out, scalar);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for item in items {
                out.push_str(&format!("{pad}-"));
                emit_value(item, indent, out, scalar);
            }
        }
        Value::Object(_) => out.push_str(&format!("{pad}{{}}\n")),
        Value::Array(_) => out.push_str(&format!("{pad}[]\n")),
        other => {
            out.push_str(&pad);
            emit_inline(other, out);
        }
    }
}

/// Write the value following a `key:` or `-`, which is already on the line.
fn emit_value(v: &Value, indent: usize, out: &mut String, scalar: &ScalarWriter<'_>) {
    match v {
        Value::String(s) if s.is_empty() => out.push_str(" \"\"\n"),
        Value::String(s) => {
            out.push_str(" |\n");
            scalar(s, indent + 2, out);
        }
        Value::Object(map) if map.is_empty() => out.push_str(" {}\n"),
        Value::Array(items) if items.is_empty() => out.push_str(" []\n"),
        Value::Object(_) | Value::Array(_) => {
            out.push('\n');
            emit(v, indent + 2, out, scalar);
        }
        other => {
            out.push(' ');
            emit_inline(other, out);
        }
    }
}

/// Numbers, booleans and null: nothing about them can be misread inline.
fn emit_inline(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null\n"),
        other => {
            out.push_str(&other.to_string());
            out.push('\n');
        }
    }
}

/// Write `text` as the body of a block scalar, every line indented.
fn block_scalar(text: &str, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    for line in text.split('\n') {
        out.push_str(&pad);
        out.push_str(line);
        out.push('\n');
    }
}

fn trim_trailing_newlines(mut s: String) -> String {
    while s.ends_with('\n') {
        s.pop();
    }
    s
}

// ---------------------------------------------------------------------------
// Shell layout
// ---------------------------------------------------------------------------

/// Lay a shell command out over lines: one step per operator, long steps
/// continued with `\`. Quoted regions are never split.
pub fn format_shell(command: &str, width: usize) -> String {
    let budget = width.max(24);
    let mut lines: Vec<String> = Vec::new();
    for step in split_on_operators(command) {
        let step = step.trim_end();
        if step.chars().count() <= budget {
            lines.push(step.to_string());
            continue;
        }
        // Continuations of one step are indented under it.
        let wrapped = wrap_backslash(step, budget.saturating_sub(2));
        for (i, l) in wrapped.into_iter().enumerate() {
            lines.push(if i == 0 { l } else { format!("  {l}") });
        }
    }
    lines.join("\n")
}

/// Split a command after each top-level `&&`, `||`, `|` or `;`, keeping the
/// operator on the line it ends. Operators inside quotes are not operators.
fn split_on_operators(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut single, mut double) = (false, false);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // A backslash escapes the next character, quotes included.
        if c == '\\' && i + 1 < chars.len() {
            cur.push(c);
            cur.push(chars[i + 1]);
            i += 2;
            continue;
        }
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            _ => {}
        }
        if !single && !double {
            let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
            if two == "&&" || two == "||" {
                cur.push_str(&two);
                out.push(std::mem::take(&mut cur));
                i += 2;
                // The space after the operator belongs to neither line.
                while i < chars.len() && chars[i] == ' ' {
                    i += 1;
                }
                continue;
            }
            // A lone `|` is a pipe; `;` ends a command. `|&`/`||` handled above.
            if c == '|' || c == ';' {
                cur.push(c);
                out.push(std::mem::take(&mut cur));
                i += 1;
                while i < chars.len() && chars[i] == ' ' {
                    i += 1;
                }
                continue;
            }
        }
        cur.push(c);
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Break `text` into lines of at most `budget` characters, each but the last
/// ending in ` \`. Breaks at spaces; a single word longer than the budget is
/// left whole rather than split somewhere meaningless.
fn wrap_backslash(text: &str, budget: usize) -> Vec<String> {
    // Two characters of the budget go to the ` \` continuation marker.
    let room = budget.saturating_sub(2).max(8);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let would_be = if line.is_empty() {
            word.chars().count()
        } else {
            line.chars().count() + 1 + word.chars().count()
        };
        if !line.is_empty() && would_be > room {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    let last = lines.len().saturating_sub(1);
    for (i, l) in lines.iter_mut().enumerate() {
        if i != last {
            l.push_str(" \\");
        }
    }
    lines
}

/// The rendering a reader sees for this call.
pub fn tool_input_yaml(tool: &str, input: &Value, width: usize) -> String {
    for_tool(tool).yaml(input, width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn generic(v: &Value) -> String {
        tool_input_yaml("some_tool", v, 76)
    }

    #[test]
    fn every_string_is_a_block_scalar_however_short() {
        // Uniform on purpose: no quoting rules for a reader to keep in mind.
        assert_eq!(generic(&json!({"path": "a.rs"})), "path: |\n  a.rs");
    }

    #[test]
    fn numbers_booleans_and_null_stay_inline() {
        let y = generic(&json!({"timeout_ms": 5000, "deep": true, "prev": null}));
        assert!(y.contains("timeout_ms: 5000"), "{y}");
        assert!(y.contains("deep: true"), "{y}");
        assert!(y.contains("prev: null"), "{y}");
        assert!(!y.contains('|'), "scalars need no block: {y}");
    }

    #[test]
    fn multi_line_content_is_reproduced_exactly() {
        let file = "fn main() {\n    println!(\"hi\");\n}";
        let y = generic(&json!({ "content": file }));
        assert_eq!(y, "content: |\n  fn main() {\n      println!(\"hi\");\n  }");
        // Indentation inside the file survives; nothing is wrapped.
        assert!(y.contains("      println!"), "{y}");
    }

    #[test]
    fn a_long_generic_string_is_not_reflowed() {
        // Only shell gets laid out. Everything else is data, and breaking it
        // would change it.
        let long = "x".repeat(300);
        let y = generic(&json!({ "blob": long.clone() }));
        assert_eq!(y, format!("blob: |\n  {long}"));
    }

    #[test]
    fn nested_structures_indent() {
        let y = generic(&json!({"opts": {"depth": 3}, "files": ["a.rs", "b.rs"]}));
        assert!(y.contains("opts:\n  depth: 3"), "{y}");
        assert!(y.contains("files:\n  - |\n    a.rs"), "{y}");
    }

    #[test]
    fn empty_values_are_visible() {
        assert_eq!(generic(&json!({"name": ""})), "name: \"\"");
        assert_eq!(generic(&json!({"args": []})), "args: []");
        assert_eq!(generic(&json!({"opts": {}})), "opts: {}");
        assert_eq!(generic(&json!({})), "{}");
    }

    // -- bash ------------------------------------------------------------

    #[test]
    fn a_lone_bash_command_drops_the_key() {
        assert_eq!(
            tool_input_yaml("bash", &json!({"command": "ls -la"}), 76),
            "ls -la"
        );
    }

    #[test]
    fn bash_keeps_the_key_when_there_is_more_than_the_command() {
        let y = tool_input_yaml("bash", &json!({"command": "ls", "timeout_ms": 100}), 76);
        assert!(y.contains("command: |\n  ls"), "{y}");
        assert!(y.contains("timeout_ms: 100"), "{y}");
    }

    #[test]
    fn a_pipeline_gets_one_step_per_line_with_operators_kept() {
        let y = tool_input_yaml(
            "bash",
            &json!({"command": "cat log.txt | grep ERROR | wc -l"}),
            76,
        );
        assert_eq!(y, "cat log.txt |\ngrep ERROR |\nwc -l");
    }

    #[test]
    fn and_and_or_operators_end_the_line_they_belong_to() {
        let y = tool_input_yaml(
            "bash",
            &json!({"command": "cargo build && cargo test || echo failed"}),
            76,
        );
        assert_eq!(y, "cargo build &&\ncargo test ||\necho failed");
    }

    #[test]
    fn operators_inside_quotes_are_not_operators() {
        let cmd = "echo 'a && b' && echo \"c | d\"";
        let y = tool_input_yaml("bash", &json!({ "command": cmd }), 76);
        assert_eq!(y, "echo 'a && b' &&\necho \"c | d\"");
    }

    #[test]
    fn an_escaped_quote_does_not_open_a_quoted_region() {
        // Without escape handling the trailing `&&` would be swallowed as if
        // it were inside a string.
        let cmd = "echo \\\" && ls";
        let y = tool_input_yaml("bash", &json!({ "command": cmd }), 76);
        assert_eq!(y, "echo \\\" &&\nls");
    }

    #[test]
    fn an_over_long_step_continues_with_a_backslash() {
        let cmd = "git log --oneline --graph --decorate --all --since=2024-01-01 \
                   --author=ada --grep=refactor";
        let cmd = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
        let y = tool_input_yaml("bash", &json!({ "command": cmd }), 40);
        let lines: Vec<&str> = y.lines().collect();
        assert!(lines.len() > 1, "should have wrapped: {y}");
        for l in &lines[..lines.len() - 1] {
            assert!(l.ends_with(" \\"), "missing continuation: {l:?}");
        }
        // Real shell: stripping the continuations recovers the command.
        let rejoined = y.replace(" \\\n  ", " ").replace(" \\\n", " ");
        assert_eq!(rejoined, cmd);
    }

    #[test]
    fn a_single_unbreakable_word_survives_intact() {
        let url = format!("curl https://example.com/{}", "x".repeat(90));
        let y = tool_input_yaml("bash", &json!({ "command": url.clone() }), 40);
        assert!(y.contains(&"x".repeat(90)), "must not split a token: {y}");
    }

    #[test]
    fn multibyte_commands_wrap_on_characters_not_bytes() {
        let cmd = format!("echo {} && echo {}", "é".repeat(50), "ü".repeat(50));
        let y = tool_input_yaml("bash", &json!({ "command": cmd }), 40);
        assert!(y.contains('é') && y.contains('ü'), "{y}");
    }

    #[test]
    fn a_command_that_is_already_multi_line_keeps_its_own_breaks() {
        let script = "set -e\ncd /tmp\nls";
        let y = tool_input_yaml("bash", &json!({ "command": script }), 76);
        assert_eq!(y, "set -e\ncd /tmp\nls");
    }
}
