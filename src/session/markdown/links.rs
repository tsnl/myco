//! Bare `http(s)://` URL autolinking for [`MarkdownRenderer`]: scheme
//! detection at word boundaries, trailing-punctuation trimming, and the
//! word-buffer marks that turn a completed URL into an OSC 8 hyperlink.

use super::*;

const HTTP: &str = "http://";
const HTTPS: &str = "https://";

/// Scheme length in bytes if `word[i..]` opens with `http://` / `https://`.
fn scheme_len_at(word: &str, i: usize) -> Option<usize> {
    let rest = &word[i..];
    if rest.starts_with(HTTPS) {
        Some(HTTPS.len())
    } else if rest.starts_with(HTTP) {
        Some(HTTP.len())
    } else {
        None
    }
}

/// End byte offset of the URL that begins at `start`, trimming trailing
/// punctuation more likely to be sentence punctuation than part of the link —
/// `.,;:!?"'<>` unconditionally, and a closing bracket only when unbalanced
/// within the URL (so the inner parens of `…/Foo_(bar)` stay, but a wrapping
/// `(…)` does not).
fn url_end(word: &str, start: usize) -> usize {
    let mut end = word.len();
    loop {
        let seg = &word[start..end];
        let Some(last) = seg.chars().next_back() else {
            break;
        };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' | '<' | '>' => true,
            ')' => seg.matches(')').count() > seg.matches('(').count(),
            ']' => seg.matches(']').count() > seg.matches('[').count(),
            '}' => seg.matches('}').count() > seg.matches('{').count(),
            _ => false,
        };
        if strip {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    end
}

impl MarkdownRenderer {
    /// Splice OSC 8 link marks around a bare `http(s)://…` URL in the completed
    /// word so it hyperlinks like a `[text](url)` link would — the visible text
    /// is untouched, only zero-width [`Mark::Link`] open/close ride in at the
    /// URL's byte offsets. Styled-only: `word_is_url` is set solely when styling
    /// is on, so plain output keeps its byte-for-byte identity.
    pub(super) fn autolink_word(&mut self) {
        if !self.word_is_url {
            return;
        }
        let Some((start, end)) = self.find_url() else {
            return;
        };
        let url = self.word[start..end].to_string();
        self.word_marks.push((start, Mark::Link(Some(url))));
        self.word_marks.push((end, Mark::Link(None)));
        // Keep marks in ascending byte order for the offset-splicing in
        // `emit_word_raw`; the sort is stable, so a co-located style mark
        // (emitted earlier) still precedes the link open.
        self.word_marks.sort_by_key(|(off, _)| *off);
    }

    /// Whether `word` carries an `http(s)://` scheme at a valid boundary — the
    /// word start (with a non-alphanumeric `word_start_prev`) or any position
    /// after a non-alphanumeric char. Cheap-triggered only when a `://` lands,
    /// so its per-char scan runs at most once per word.
    pub(super) fn word_starts_url(&self) -> bool {
        let mut prev = self.word_start_prev;
        for (i, c) in self.word.char_indices() {
            if prev.is_none_or(|p| !p.is_alphanumeric()) && scheme_len_at(&self.word, i).is_some() {
                return true;
            }
            prev = Some(c);
        }
        false
    }

    /// Byte range `[start, end)` of the bare URL in the completed word (scheme
    /// at a boundary, host non-empty), or `None`. The left boundary uses
    /// `word_start_prev` for a scheme at offset 0, so a URL glued to a preceding
    /// letter (`foohttp://…`) is rejected the same in wrap and no-wrap modes.
    fn find_url(&self) -> Option<(usize, usize)> {
        let mut prev = self.word_start_prev;
        for (i, c) in self.word.char_indices() {
            if prev.is_none_or(|p| !p.is_alphanumeric())
                && let Some(len) = scheme_len_at(&self.word, i)
            {
                let end = url_end(&self.word, i);
                // Require at least one host char past the scheme.
                if end > i + len {
                    return Some((i, end));
                }
            }
            prev = Some(c);
        }
        None
    }

    /// No-wrap mode flushes a word per char; a bare URL must instead stay
    /// buffered until whole so [`Self::autolink_word`] can wrap it. True while
    /// the pending word is a URL, or an `http(s)://` scheme still forming at a
    /// valid boundary. Styled-only, and never during code spans or link-text
    /// replay — those never autolink, so they keep their per-char flushing.
    pub(super) fn pending_url_word(&self) -> bool {
        if !self.styled || self.code || self.suppress_autolink {
            return false;
        }
        if self.word_is_url {
            return true;
        }
        if self.word_start_prev.is_some_and(|p| p.is_alphanumeric()) {
            return false;
        }
        let w: &str = &self.word;
        !w.is_empty() && (HTTP.starts_with(w) || HTTPS.starts_with(w))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        OSC_CLOSE, osc_open, plain, render, render_char_chunks, strip_escapes, styled, styled_wrap,
    };
    use super::*;

    #[test]
    fn bare_url_becomes_osc8_hyperlink() {
        // The visible text is the URL itself; the OSC 8 target is the same URL.
        let url = "https://example.com/x";
        assert_eq!(
            render("see https://example.com/x now", styled()),
            format!("see {}{url}{OSC_CLOSE} now", osc_open(url))
        );
        // `http://` is detected the same as `https://`.
        let url = "http://a.test/p";
        assert_eq!(
            render("go http://a.test/p", styled()),
            format!("go {}{url}{OSC_CLOSE}", osc_open(url))
        );
    }

    #[test]
    fn bare_url_trailing_punctuation_stays_outside_link() {
        // Sentence punctuation after the URL is visible but not part of the link.
        let url = "https://example.com";
        assert_eq!(
            render("Visit https://example.com.", styled()),
            format!("Visit {}{url}{OSC_CLOSE}.", osc_open(url))
        );
        // A wrapping paren is excluded; balanced inner parens are kept.
        assert_eq!(
            render("(https://example.com)", styled()),
            format!("({}{url}{OSC_CLOSE})", osc_open(url))
        );
        let wiki = "https://en.wikipedia.org/wiki/Foo_(bar)";
        assert_eq!(
            render("see https://en.wikipedia.org/wiki/Foo_(bar).", styled()),
            format!("see {}{wiki}{OSC_CLOSE}.", osc_open(wiki))
        );
    }

    #[test]
    fn bare_url_not_linked_without_a_real_scheme() {
        // No scheme → literal; a scheme glued to a preceding word → literal.
        assert_eq!(
            render("visit example.com today", styled()),
            "visit example.com today"
        );
        assert_eq!(
            render("see www.example.com", styled()),
            "see www.example.com"
        );
        assert_eq!(
            render("nothttp://x.test here", styled()),
            "nothttp://x.test here"
        );
        // Scheme with an empty host is not a link.
        assert_eq!(
            render("bare https:// slash", styled()),
            "bare https:// slash"
        );
    }

    #[test]
    fn bare_url_in_code_span_is_not_linked() {
        // Inside a code span the URL is literal, monospace content.
        let out = render("run `curl https://example.com`", styled());
        assert!(
            !out.contains("\x1b]8;;"),
            "no hyperlink in code span: {out:?}"
        );
        assert!(out.contains("https://example.com"), "{out:?}");
    }

    #[test]
    fn plain_mode_bare_url_stays_literal() {
        // Styling off ⇒ identity: the URL prints verbatim, no OSC 8.
        let input = "see https://example.com/x, then done";
        assert_eq!(render(input, plain()), input);
        assert_eq!(render_char_chunks(input, plain()), input);
    }

    #[test]
    fn bare_url_split_across_chunks_matches_single_feed() {
        let input = "go https://h.test/a/b?c=d ok";
        assert_eq!(render(input, styled()), render_char_chunks(input, styled()));
        let url = "https://h.test/a/b?c=d";
        assert_eq!(
            render(input, styled()),
            format!("go {}{url}{OSC_CLOSE} ok", osc_open(url))
        );
    }

    #[test]
    fn bare_url_event_stream_carries_link_as_presentation() {
        // The live TUI path is the event path: the URL must ride Link events,
        // its visible Text stay escape-free and equal to the source.
        let mut r = MarkdownRenderer::new(styled());
        let mut events = r.feed_events("open https://h.test/p now");
        events.extend(r.finish_events());
        let mut content = String::new();
        for e in &events {
            if let TuiEvent::Text(text) = e {
                assert!(!text.contains('\x1b'), "escape in Text: {text:?}");
                content.push_str(text);
            }
        }
        assert_eq!(content, "open https://h.test/p now");
        let links: Vec<Option<String>> = events
            .iter()
            .filter_map(|e| match e {
                TuiEvent::Link(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(links, vec![Some("https://h.test/p".to_string()), None]);
    }

    #[test]
    fn url_as_markdown_link_text_is_not_double_linked() {
        // `[url](url)` must yield a single link whose target is the paren URL,
        // never a second link nested over the visible text.
        let out = render("[https://vis.test](https://tgt.test)", styled());
        assert_eq!(
            out,
            format!(
                "{}https://vis.test{OSC_CLOSE}",
                osc_open("https://tgt.test")
            )
        );
    }

    #[test]
    fn long_bare_url_wraps_whole_and_links() {
        // A URL longer than the wrap width breaks before it (no mid-URL split)
        // and is still linked in full, rather than overflowing raw and unlinked.
        let url = "https://example.com/very/long/path/segment";
        let out = render(
            &format!("see {url}"),
            Palette::colored(true).with_wrap(Some(12)),
        );
        assert_eq!(out, format!("see\n{}{url}{OSC_CLOSE}", osc_open(url)));
    }

    #[test]
    fn bare_url_in_table_cell_is_hyperlinked() {
        // Cells render through the same inline machine, so a bare URL inside
        // one hyperlinks like prose; the box pads by visible width.
        let url = "https://h.test/x";
        let input = "| link |\n| - |\n| https://h.test/x |\n";
        let out = render(input, styled());
        assert!(out.contains(&osc_open(url)), "cell URL linked: {out:?}");
        assert_eq!(
            strip_escapes(&out),
            "┌──────────────────┐\n\
             │ link             │\n\
             ├──────────────────┤\n\
             │ https://h.test/x │\n\
             └──────────────────┘\n"
        );
    }

    #[test]
    fn squeezed_cell_keeps_url_whole_and_linked() {
        // In a squeezed table the URL is the column's floor: it stays on one
        // physical line (never split mid-URL) and stays hyperlinked, while the
        // prose around it wraps into further lines.
        let url = "https://h.test/long/path";
        let input = format!("| k | v |\n| - | - |\n| a | see {url} today |\n");
        let out = render(&input, styled_wrap(34));
        assert!(out.contains(&osc_open(url)), "squeezed URL linked: {out:?}");
        let plain = strip_escapes(&out);
        assert!(
            plain.contains(&format!("│ {url} ")),
            "URL on one cell line: {plain:?}"
        );
    }
}
