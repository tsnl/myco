//! Markdown → escaped HTML. Agent replies are markdown; human posts stay
//! plain. The parser is CommonMark + strikethrough + tables + `$`/`$$`
//! math; raw HTML in the source is escaped, never passed through. Math
//! becomes MathML via `latex2mathml` (unparseable latex stays as text).

use latex2mathml::{DisplayStyle, latex_to_mathml};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Render markdown to an HTML fragment. Output is always escaped except
/// for the tags we emit; a `<script>` in the source becomes text.
pub fn render_markdown(src: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_MATH);
    let mut out = String::with_capacity(src.len() + src.len() / 4);
    for event in Parser::new_ext(src, opts) {
        match event {
            Event::Start(tag) => start(&mut out, tag),
            Event::End(tag) => end(&mut out, tag),
            Event::Text(t) => push_esc(&mut out, &t),
            Event::Code(t) => {
                out.push_str("<code>");
                push_esc(&mut out, &t);
                out.push_str("</code>");
            }
            Event::Html(t) | Event::InlineHtml(t) => push_esc(&mut out, &t),
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push_str("<br>"),
            Event::Rule => out.push_str("<hr>"),
            Event::TaskListMarker(done) => {
                out.push_str(if done { "[x] " } else { "[ ] " });
            }
            Event::InlineMath(t) => push_math(&mut out, &t, DisplayStyle::Inline),
            Event::DisplayMath(t) => push_math(&mut out, &t, DisplayStyle::Block),
            Event::FootnoteReference(t) => {
                out.push_str("<sup>");
                push_esc(&mut out, &t);
                out.push_str("</sup>");
            }
        }
    }
    out
}

fn start(out: &mut String, tag: Tag<'_>) {
    match tag {
        Tag::Paragraph => out.push_str("<p>"),
        Tag::Heading { level, .. } => {
            out.push_str(match level {
                HeadingLevel::H1 => "<h1>",
                HeadingLevel::H2 => "<h2>",
                HeadingLevel::H3 => "<h3>",
                HeadingLevel::H4 => "<h4>",
                HeadingLevel::H5 => "<h5>",
                HeadingLevel::H6 => "<h6>",
            });
        }
        Tag::BlockQuote(_) => out.push_str("<blockquote>"),
        Tag::CodeBlock(kind) => {
            out.push_str("<pre><code");
            if let CodeBlockKind::Fenced(lang) = kind {
                let lang = lang.split_whitespace().next().unwrap_or("");
                if !lang.is_empty() {
                    out.push_str(" class=\"language-");
                    push_esc(out, lang);
                    out.push('"');
                }
            }
            out.push('>');
        }
        Tag::List(None) => out.push_str("<ul>"),
        Tag::List(Some(_)) => out.push_str("<ol>"),
        Tag::Item => out.push_str("<li>"),
        Tag::Emphasis => out.push_str("<em>"),
        Tag::Strong => out.push_str("<strong>"),
        Tag::Strikethrough => out.push_str("<del>"),
        Tag::Link { dest_url, .. } => {
            out.push_str("<a href=\"");
            push_esc(out, &dest_url);
            out.push_str("\" rel=\"noopener noreferrer\" target=\"_blank\">");
        }
        Tag::Image { dest_url, title, .. } => {
            out.push_str("<img src=\"");
            push_esc(out, &dest_url);
            out.push_str("\" alt=\"");
            push_esc(out, &title);
            out.push_str("\">");
        }
        Tag::Table(_) => out.push_str("<table>"),
        Tag::TableHead => out.push_str("<thead><tr>"),
        Tag::TableRow => out.push_str("<tr>"),
        Tag::TableCell => out.push_str("<td>"),
        Tag::HtmlBlock | Tag::MetadataBlock(_) => {}
        Tag::FootnoteDefinition(_) => out.push_str("<aside>"),
        Tag::DefinitionList => out.push_str("<dl>"),
        Tag::DefinitionListTitle => out.push_str("<dt>"),
        Tag::DefinitionListDefinition => out.push_str("<dd>"),
        Tag::Superscript => out.push_str("<sup>"),
        Tag::Subscript => out.push_str("<sub>"),
    }
}

fn end(out: &mut String, tag: TagEnd) {
    match tag {
        TagEnd::Paragraph => out.push_str("</p>"),
        TagEnd::Heading(level) => out.push_str(match level {
            HeadingLevel::H1 => "</h1>",
            HeadingLevel::H2 => "</h2>",
            HeadingLevel::H3 => "</h3>",
            HeadingLevel::H4 => "</h4>",
            HeadingLevel::H5 => "</h5>",
            HeadingLevel::H6 => "</h6>",
        }),
        TagEnd::BlockQuote(_) => out.push_str("</blockquote>"),
        TagEnd::CodeBlock => out.push_str("</code></pre>"),
        TagEnd::List(false) => out.push_str("</ul>"),
        TagEnd::List(true) => out.push_str("</ol>"),
        TagEnd::Item => out.push_str("</li>"),
        TagEnd::Emphasis => out.push_str("</em>"),
        TagEnd::Strong => out.push_str("</strong>"),
        TagEnd::Strikethrough => out.push_str("</del>"),
        TagEnd::Link => out.push_str("</a>"),
        TagEnd::Image => {}
        TagEnd::Table => out.push_str("</table>"),
        TagEnd::TableHead => out.push_str("</tr></thead>"),
        TagEnd::TableRow => out.push_str("</tr>"),
        TagEnd::TableCell => out.push_str("</td>"),
        TagEnd::HtmlBlock | TagEnd::MetadataBlock(_) => {}
        TagEnd::FootnoteDefinition => out.push_str("</aside>"),
        TagEnd::DefinitionList => out.push_str("</dl>"),
        TagEnd::DefinitionListTitle => out.push_str("</dt>"),
        TagEnd::DefinitionListDefinition => out.push_str("</dd>"),
        TagEnd::Superscript => out.push_str("</sup>"),
        TagEnd::Subscript => out.push_str("</sub>"),
    }
}

fn push_math(out: &mut String, latex: &str, display: DisplayStyle) {
    match latex_to_mathml(latex, display) {
        Ok(mathml) => out.push_str(&mathml),
        Err(_) => {
            let wrap = if matches!(display, DisplayStyle::Block) {
                "$$"
            } else {
                "$"
            };
            out.push_str("<code class=\"math\">");
            push_esc(out, wrap);
            push_esc(out, latex);
            push_esc(out, wrap);
            out.push_str("</code>");
        }
    }
}

fn push_esc(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::render_markdown;

    #[test]
    fn a_paragraph_and_emphasis_survive() {
        let html = render_markdown("hello **world** and `code`");
        assert!(html.contains("<strong>world</strong>"), "{html}");
        assert!(html.contains("<code>code</code>"), "{html}");
    }

    #[test]
    fn raw_html_is_escaped() {
        let html = render_markdown("<script>alert(1)</script>");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn a_fenced_block_keeps_newlines() {
        let html = render_markdown("```rs\nfn main() {}\n```");
        assert!(html.contains("<pre><code"), "{html}");
        assert!(html.contains("fn main() {}"), "{html}");
    }

    #[test]
    fn inline_and_display_math_become_mathml() {
        let inline = render_markdown("the root is $\\sqrt{2}$.");
        assert!(inline.contains("<math"), "{inline}");
        assert!(inline.contains("display=\"inline\""), "{inline}");
        let display = render_markdown("$$x = \\frac{1}{2}$$");
        assert!(display.contains("display=\"block\""), "{display}");
    }

    #[test]
    fn unparseable_math_stays_as_text() {
        let html = render_markdown("$\\notacommand{");
        assert!(!html.contains("<math"), "{html}");
        assert!(html.contains("$"), "{html}");
    }
}
