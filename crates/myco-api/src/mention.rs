//! `@handle` addressing — the rule that decides who a message is *for*.
//!
//! A session is a room, not a private line: once a second person posts, the
//! agent has to know when it is being spoken to and when it is overhearing.
//! Parsing lives here, in the wire crate, because both sides need the same
//! answer — the server gates the agent turn on it, and the client highlights
//! mentions and raises notifications from it.
//!
//! Deliberately mechanical: an explicit `@` prefix, never a guess about intent.
//! A rule people can see in the text they typed is one they can rely on.

/// Handles that always mean "the agent". A session's model key (`@opus`) works
/// too — see [`addresses_agent`] — but these are stable across model swaps.
pub const AGENT_HANDLES: [&str; 3] = ["myco", "agent", "assistant"];

/// Characters that continue a handle after `@`. Ends at whitespace or
/// punctuation, so `@ada,` and `@ada.` address ada.
fn is_handle_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// Every `@handle` in `text`, lowercased, in order, without duplicates.
///
/// An `@` inside a word (`user@example.com`) is not a mention: a handle must
/// start at the beginning of the text or follow a non-handle character.
pub fn handles(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '@' {
            i += 1;
            continue;
        }
        // Reject the email case: `@` glued to the end of a preceding word.
        if i > 0 && is_handle_char(chars[i - 1]) {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < chars.len() && is_handle_char(chars[end]) {
            end += 1;
        }
        if end > start {
            // A trailing `.` is sentence punctuation, not part of the handle.
            let mut stop = end;
            while stop > start && chars[stop - 1] == '.' {
                stop -= 1;
            }
            let handle: String = chars[start..stop].iter().collect::<String>().to_lowercase();
            if !handle.is_empty() && !out.contains(&handle) {
                out.push(handle);
            }
        }
        i = end.max(start);
    }
    out
}

/// Does `text` address the agent — by a stable handle or by the model key it
/// is running as (`@opus`)?
pub fn addresses_agent(text: &str, model_key: &str) -> bool {
    let model = model_key.to_lowercase();
    handles(text)
        .iter()
        .any(|h| AGENT_HANDLES.contains(&h.as_str()) || (!model.is_empty() && *h == model))
}

/// Does `text` address the user `id`/`name`?
///
/// Both the roster id and the first word of the display name count, so
/// "Ada Lovelace" answers to `@ada` as well as to `@ada.lovelace` if that is
/// how she is rostered.
pub fn addresses_user(text: &str, id: &str, name: &str) -> bool {
    let handles = handles(text);
    handles.iter().any(|h| matches_user(h, id, name))
}

/// Whether one parsed handle names this user.
pub fn matches_user(handle: &str, id: &str, name: &str) -> bool {
    let handle = handle.to_lowercase();
    if handle == id.to_lowercase() {
        return true;
    }
    let name = name.to_lowercase();
    if handle == name {
        return true;
    }
    // First word of the display name: "Ada Lovelace" → `@ada`.
    name.split_whitespace().next() == Some(handle.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_handles_are_extracted_in_order_without_duplicates() {
        assert_eq!(
            handles("@ada ping @grace and @ada again"),
            vec!["ada", "grace"]
        );
    }

    #[test]
    fn handles_stop_at_punctuation_but_keep_inner_dots() {
        assert_eq!(handles("@ada, look"), vec!["ada"]);
        assert_eq!(handles("thanks @grace."), vec!["grace"]);
        assert_eq!(handles("@ada.lovelace here"), vec!["ada.lovelace"]);
        assert_eq!(handles("(@ada)"), vec!["ada"]);
    }

    #[test]
    fn an_email_address_is_not_a_mention() {
        assert!(handles("mail ada@example.com").is_empty());
    }

    #[test]
    fn a_bare_at_sign_is_not_a_mention() {
        assert!(handles("@ @@ costs $5 @").is_empty());
    }

    #[test]
    fn handles_are_case_insensitive() {
        assert_eq!(handles("@Ada and @GRACE"), vec!["ada", "grace"]);
    }

    #[test]
    fn the_agent_answers_to_stable_handles_and_to_its_model_key() {
        assert!(addresses_agent("@myco status?", "opus"));
        assert!(addresses_agent("@agent status?", "opus"));
        assert!(addresses_agent("@assistant status?", "opus"));
        assert!(addresses_agent("@opus status?", "opus"));
        // Someone else's model key does not summon this session's agent.
        assert!(!addresses_agent("@haiku status?", "opus"));
        assert!(!addresses_agent("@ada what do you think?", "opus"));
    }

    #[test]
    fn an_empty_model_key_never_matches_an_empty_handle() {
        assert!(!addresses_agent("hello @", ""));
        assert!(!addresses_agent("plain text", ""));
    }

    #[test]
    fn users_answer_to_their_id_and_their_first_name() {
        assert!(addresses_user(
            "@ada up for a review?",
            "ada",
            "Ada Lovelace"
        ));
        assert!(addresses_user("@Ada up for it?", "ada", "Ada Lovelace"));
        assert!(addresses_user(
            "@ada.lovelace formally",
            "ada.lovelace",
            "Ada Lovelace"
        ));
        assert!(!addresses_user("@grace up for it?", "ada", "Ada Lovelace"));
    }

    #[test]
    fn addressing_a_user_does_not_address_the_agent() {
        assert!(!addresses_agent("@ada can you look?", "opus"));
        assert!(addresses_user("@ada can you look?", "ada", "Ada Lovelace"));
    }
}
