//! Reading a cursored tail under a budget — the one rule every consumer of
//! another instance's output shares: **the end survives**.
//!
//! A command's exit chatter beats its preamble, and a watched terminal's
//! last screenful beats what scrolled past an hour ago, so when there is
//! more output than budget the front is what goes. Written by hand at each
//! call site this drifted immediately — two different scans for the char
//! boundary, one of them silently declining to trim at all when it did not
//! find one nearby. One spelling now; the budget and the wording stay the
//! caller's, because how much context an agent can spend is not this
//! module's business.

use myco_instance::{Pool, Principal, VerbError};
use serde_json::json;

/// One page of a byte tail. Big enough that a normal command drains in one
/// call, small enough that a runaway one does not arrive as a single
/// allocation.
const PAGE_BYTES: u64 = 64 * 1024;

/// Keep only the freshest `budget` bytes, dropping from the front at a char
/// boundary. Answers whether anything was dropped, so the caller says so in
/// its own words.
pub(crate) fn keep_freshest(text: &mut String, budget: usize) -> bool {
    if text.len() <= budget {
        return false;
    }
    let cut = text.len() - budget;
    // Cutting mid-codepoint would panic; the next boundary is at most three
    // bytes on, and the end of the string is always one.
    let cut = (cut..text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    text.drain(..cut);
    true
}

/// Drain an instance's byte tail from its start to its end, keeping the
/// freshest `budget` bytes. Answers the text and whether the front was
/// dropped. Trimming happens per page, so a command that printed a
/// gigabyte costs a page of memory, not a gigabyte of it.
pub(crate) async fn drain(
    pool: &Pool,
    principal: &Principal,
    id: &str,
    budget: usize,
) -> Result<(String, bool), VerbError> {
    let mut out = String::new();
    let mut truncated = false;
    let mut from = 0u64;
    loop {
        let page = pool
            .call(
                principal,
                id,
                "tail",
                json!({"from": from, "max_bytes": PAGE_BYTES}),
            )
            .await?;
        let chunk = page["data"].as_str().unwrap_or("");
        let next = page["next"].as_u64().unwrap_or(from);
        if chunk.is_empty() || next <= from {
            return Ok((out, truncated));
        }
        out.push_str(chunk);
        from = next;
        truncated |= keep_freshest(&mut out, budget);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_keeps_the_end_and_never_splits_a_codepoint() {
        let mut text = "abcdefghij".to_string();
        assert!(!keep_freshest(&mut text, 10), "at budget is not over it");
        assert_eq!(text, "abcdefghij");

        assert!(keep_freshest(&mut text, 4));
        assert_eq!(text, "ghij", "the end survives");

        // The cut lands mid-codepoint: it moves forward to a boundary, so
        // the result is shorter than the budget rather than invalid.
        let mut wide = "αβγδ".to_string();
        assert_eq!(wide.len(), 8);
        assert!(keep_freshest(&mut wide, 5));
        assert_eq!(wide, "γδ");
    }
}
