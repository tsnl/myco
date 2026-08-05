//! Syntax highlighting for fenced code blocks and tool-call JSON.
//!
//! syntect's full syntax and theme dumps ship in the wasm bundle, which buys
//! every language it knows rather than the handful we could hand-roll. The
//! sets are parsed once on first use and cached in a thread-local — wasm is
//! single-threaded, so there is no lock to pay for.
//!
//! Colors are emitted as inline `style` attributes with no background, so
//! highlighted code sits on the page's own surface and the terminal identity
//! survives.

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::html::{IncludeBackground, styled_line_to_highlighted_html};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

thread_local! {
    static SYNTAXES: SyntaxSet = SyntaxSet::load_defaults_newlines();
    static THEME: Theme = {
        let mut themes = ThemeSet::load_defaults();
        // Muted, low-contrast dark palette — closest of the bundled themes to
        // the CLI's own coloring.
        themes
            .themes
            .remove("base16-eighties.dark")
            .or_else(|| themes.themes.remove("base16-ocean.dark"))
            .expect("syntect ships base16 themes")
    };
}

/// Highlight `code` as `lang`, returning `<span style=…>` markup with the
/// text HTML-escaped by syntect. An unknown language falls back to plain
/// escaped text rather than guessing.
pub fn highlight_to_html(code: &str, lang: &str) -> String {
    SYNTAXES.with(|syntaxes| {
        let syntax = lookup(syntaxes, lang);
        let Some(syntax) = syntax else {
            return escape(code);
        };
        THEME.with(|theme| {
            let mut h = HighlightLines::new(syntax, theme);
            let mut out = String::with_capacity(code.len() * 2);
            for line in LinesWithEndings::from(code) {
                let Ok(ranges) = h.highlight_line(line, syntaxes) else {
                    return escape(code);
                };
                out.push_str(
                    &styled_line_to_highlighted_html(&ranges, IncludeBackground::No)
                        .unwrap_or_else(|_| escape(line)),
                );
            }
            out
        })
    })
}

/// Resolve a fence label to a syntax: by token (`rs`, `sh`), then by name
/// (`Rust`), then by a few aliases syntect does not carry itself.
fn lookup<'a>(
    syntaxes: &'a SyntaxSet,
    lang: &str,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    let lang = lang.trim();
    if lang.is_empty() {
        return None;
    }
    let alias = match lang.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "sh" | "shell" | "zsh" | "console" => "bash",
        "py" => "python",
        "js" | "mjs" => "javascript",
        "ts" => "typescript",
        "yml" => "yaml",
        "md" => "markdown",
        other => other,
    }
    .to_string();
    syntaxes
        .find_syntax_by_token(&alias)
        .or_else(|| syntaxes.find_syntax_by_name(lang))
        .or_else(|| syntaxes.find_syntax_by_extension(&alias))
}

/// Minimal HTML escape for the un-highlightable fallback path. syntect
/// escapes its own output; this covers the text that never reaches it.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
