//! The agent prelude: maildir-style entries under `~/.myco/workspace/prelude/`.
//!
//! Named like a language prelude — knowledge in scope for every agent before
//! any work, no import, no lookup — not a Rust re-export module.
//!
//! One write-once `*.md` file per entry. Every visible entry is rendered into
//! each agent system prompt ([`crate::prompts`]); the root-only `prelude` tool
//! ([`crate::tool_services::PreludeTool`]) is the edit path. The storage rules
//! that keep concurrent agents safe on a weakly consistent filesystem live
//! here: entries are written under a hidden temp name claimed exclusively and
//! renamed into place, never edited in place. Two agents adding at once get
//! distinct entries, and two replacing the same entry leave two candidate
//! entries (a duplicate to merge later) — never a lost one.

use std::path::{Path, PathBuf};

/// One prelude entry: filename (its id) and trimmed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreludeEntry {
    pub name: String,
    pub text: String,
}

/// The prelude directory, respecting `MYCO_HOME`.
pub fn dir() -> Result<PathBuf, String> {
    Ok(crate::core::myco_home()?.join("workspace").join("prelude"))
}

/// Whether `name` is a legal entry id: a plain visible `*.md` filename.
/// Hidden names are in-progress writes; path separators would escape the dir.
pub fn is_entry_name(name: &str) -> bool {
    name.len() > ".md".len()
        && !name.starts_with('.')
        && name.ends_with(".md")
        && !name.contains(['/', '\\'])
}

/// Why the prelude directory could not be read, when the failure is real.
///
/// A missing directory is the ordinary "nothing recorded yet" case and reads
/// as `None`. Anything else — permissions, a broken mount, IO — means
/// [`entries`] renders empty and every agent silently runs without the
/// knowledge it is supposed to carry, which nothing downstream would ever
/// mention. Startup calls this to warn instead
/// ([`crate::harness::StartupPreflight`]).
pub fn read_failure(dir: &Path) -> Option<String> {
    match std::fs::read_dir(dir) {
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("{}: {e}", dir.display())),
    }
}

/// All visible entries in filename order — the render order. Hidden names,
/// non-`*.md` files, and whitespace-only entries are skipped.
///
/// Infallible by design: a prompt must still build when the directory is
/// unreadable. [`read_failure`] is how that case reaches the user.
pub fn entries(dir: &Path) -> Vec<PreludeEntry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PreludeEntry> = read
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !is_entry_name(&name) || !entry.path().is_file() {
                return None;
            }
            let text = std::fs::read_to_string(entry.path())
                .ok()?
                .trim()
                .to_string();
            (!text.is_empty()).then_some(PreludeEntry { name, text })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The prelude as it reads in a prompt (and in `prelude` action=list): each entry
/// under a `[prelude entry <name>]` label, so agents can `replace`/`remove` by
/// the id they see. Empty string when there are no entries.
pub fn rendered_body(entries: &[PreludeEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("[prelude entry {}]\n{}", entry.name, entry.text));
    }
    out
}

/// Create a new entry holding `text`; returns its filename. The name is a
/// UTC timestamp plus random hex, so concurrent adds land side by side, and
/// the write goes through a hidden temp name so a concurrently rendering
/// process never sees a partial entry.
///
/// The temp name is claimed with `create_new`, which is what makes the
/// uniqueness real rather than probabilistic: two processes that mint the same
/// name in the same second both try to create the same temp file, exactly one
/// wins, and the loser retries under a fresh name instead of silently
/// overwriting the winner's bytes and dropping an entry.
///
/// Refuses to write at all if the resulting prelude would render past
/// `max_bytes`, so the cap holds by construction instead of being trimmed back
/// at prompt-build time. `superseding` names an entry the caller is about to
/// remove (the `replace` path), whose bytes are therefore not counted against
/// the budget — otherwise swapping an entry for one the same size could be
/// refused.
pub fn add_entry(
    dir: &Path,
    text: &str,
    max_bytes: usize,
    superseding: Option<&str>,
) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let body = format!("{}\n", text.trim_end());
    for _ in 0..8 {
        let name = format!(
            "{}-{}.md",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
            &crate::core::uuid_simple_hex(uuid::Uuid::new_v4())[..4]
        );
        if dir.join(&name).exists() {
            continue;
        }
        // Every candidate name is the same length, so a retry cannot change
        // this verdict — checking inside the loop just keeps it next to the
        // name it is measuring.
        let projected = projected_bytes(dir, &name, text, superseding);
        if projected > max_bytes {
            return Err(format!(
                "refusing to write: the prelude would render at {projected} bytes, over the \
                 {max_bytes} byte max_prelude_bytes cap. Merge overlapping entries or remove \
                 dead ones (action=list), move cold material to a workspace file, or raise \
                 max_prelude_bytes in config.toml"
            ));
        }
        if write_entry(dir, &name, &body)? {
            return Ok(name);
        }
    }
    Err("could not pick a fresh prelude entry name".into())
}

/// Rendered size of the prelude if `text` landed as entry `name` and
/// `superseding` were dropped.
fn projected_bytes(dir: &Path, name: &str, text: &str, superseding: Option<&str>) -> usize {
    let mut projected: Vec<PreludeEntry> = entries(dir)
        .into_iter()
        .filter(|e| Some(e.name.as_str()) != superseding)
        .collect();
    projected.push(PreludeEntry {
        name: name.to_string(),
        text: text.trim().to_string(),
    });
    rendered_body(&projected).len()
}

/// Write `body` as entry `name`: claim a hidden temp exclusively, fill it,
/// rename it into place. `Ok(false)` means another process already holds that
/// temp name and the caller should retry under a fresh one — overwriting it
/// would destroy bytes a concurrent add is mid-way through writing, and one of
/// the two entries would vanish at rename time.
fn write_entry(dir: &Path, name: &str, body: &str) -> Result<bool, String> {
    let tmp = dir.join(format!(".tmp-{name}"));
    let mut file = match std::fs::File::create_new(&tmp) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(format!("create {}: {e}", tmp.display())),
    };
    if let Err(e) = std::io::Write::write_all(&mut file, body.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write {}: {e}", tmp.display()));
    }
    drop(file);
    match std::fs::rename(&tmp, dir.join(name)) {
        Ok(()) => Ok(true),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(format!("rename {} into place: {e}", tmp.display()))
        }
    }
}

/// Remove entry `name`. `Ok(false)` when it is already gone — a concurrent
/// agent superseding the same entry is normal, not an IO failure.
pub fn remove_entry(dir: &Path, name: &str) -> Result<bool, String> {
    if !is_entry_name(name) {
        return Err(format!(
            "{name:?} is not a prelude entry id (a plain `*.md` filename, as listed)"
        ));
    }
    match std::fs::remove_file(dir.join(name)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("remove {name}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// For tests about storage mechanics rather than the size cap.
    const NO_CAP: usize = usize::MAX;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("myco-prelude-store-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn entries_render_in_filename_order_with_labels() {
        let dir = temp_dir("order");
        std::fs::write(dir.join("20260102T000000Z-bbbb.md"), "second\n").unwrap();
        std::fs::write(dir.join("20260101T000000Z-aaaa.md"), "first\n").unwrap();
        std::fs::write(dir.join(".tmp-20260103T000000Z-cccc.md"), "hidden\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "not markdown\n").unwrap();
        std::fs::write(dir.join("20260104T000000Z-dddd.md"), "  \n\n").unwrap();

        let entries = entries(&dir);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["20260101T000000Z-aaaa.md", "20260102T000000Z-bbbb.md"]
        );
        assert_eq!(
            rendered_body(&entries),
            "[prelude entry 20260101T000000Z-aaaa.md]\nfirst\n\n\
             [prelude entry 20260102T000000Z-bbbb.md]\nsecond"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_creates_a_visible_entry_and_leaves_no_temp_behind() {
        let dir = temp_dir("add");
        let name = add_entry(&dir, "  a fact  \n\n", NO_CAP, None).unwrap();
        assert!(is_entry_name(&name), "{name}");
        assert_eq!(
            std::fs::read_to_string(dir.join(&name)).unwrap(),
            "  a fact\n"
        );
        // No stray temp files: hidden names would still pollute the dir.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory that is merely absent is the ordinary empty case; one that
    /// exists but cannot be read has to be distinguishable, since `entries`
    /// renders both as "no prelude".
    #[test]
    fn read_failure_separates_absent_from_unreadable() {
        let dir = temp_dir("read-failure");
        assert_eq!(read_failure(&dir.join("nope")), None);
        assert_eq!(read_failure(&dir), None);

        // A file where the directory should be: readable path, unreadable as
        // a directory — the same shape as a permissions or mount failure,
        // without needing to drop privileges in a test.
        let not_a_dir = dir.join("occupied");
        std::fs::write(&not_a_dir, "x").unwrap();
        assert!(entries(&not_a_dir).is_empty());
        let failure = read_failure(&not_a_dir).expect("a non-directory should report a failure");
        assert!(failure.contains("occupied"), "{failure}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two adds that mint the same name must not both write the same temp
    /// file: the loser reports "not mine" so the caller retries, instead of
    /// overwriting bytes the winner is mid-write and losing an entry at
    /// rename time.
    #[test]
    fn a_claimed_temp_name_is_refused_not_overwritten() {
        let dir = temp_dir("temp-claim");
        let name = "20260801T120000Z-abcd.md";
        let tmp = dir.join(format!(".tmp-{name}"));
        std::fs::write(&tmp, "another process is mid-write").unwrap();

        assert_eq!(write_entry(&dir, name, "loser\n"), Ok(false));
        // The winner's in-flight bytes are intact and nothing was published.
        assert_eq!(
            std::fs::read_to_string(&tmp).unwrap(),
            "another process is mid-write"
        );
        assert!(!dir.join(name).exists());

        // With the temp free, the same name writes and publishes normally.
        std::fs::remove_file(&tmp).unwrap();
        assert_eq!(write_entry(&dir, name, "winner\n"), Ok(true));
        assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), "winner\n");
        assert!(
            !tmp.exists(),
            "temp should be renamed away, not left behind"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cap holds by refusal, not by trimming: an add that would cross it
    /// writes nothing at all, and says what to do about it.
    #[test]
    fn an_add_over_the_cap_is_refused_and_writes_nothing() {
        let dir = temp_dir("cap-refusal");
        let first = add_entry(&dir, "the first fact", NO_CAP, None).unwrap();
        let used = rendered_body(&entries(&dir)).len();

        let err = add_entry(&dir, "the second fact", used + 10, None)
            .expect_err("an entry past the cap must be refused");
        assert!(err.contains("refusing to write"), "{err}");
        assert!(err.contains("max_prelude_bytes"), "{err}");
        // Nothing landed, and no temp file was left behind either.
        assert_eq!(
            entries(&dir).iter().map(|e| &e.name).collect::<Vec<_>>(),
            vec![&first]
        );
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().starts_with('.')),
            "a refused add must not leave a temp file"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `replace` discounts the entry it is about to drop, so swapping an entry
    /// for one of similar size is not refused just because the two coexist for
    /// an instant.
    #[test]
    fn replace_measures_against_the_entry_it_supersedes() {
        let dir = temp_dir("cap-replace");
        let old = add_entry(&dir, "some fact, first draft", NO_CAP, None).unwrap();
        let exactly_enough = rendered_body(&entries(&dir)).len();

        // Same-size rewrite with no headroom at all: allowed, because `old` is
        // discounted; refused if it were counted twice.
        let new = add_entry(
            &dir,
            "some fact, later draft",
            exactly_enough,
            Some(old.as_str()),
        )
        .expect("a same-size replacement should fit");
        remove_entry(&dir, &old).unwrap();

        let live = entries(&dir);
        assert_eq!(live.len(), 1, "{live:?}");
        assert_eq!(live[0].name, new);
        assert_eq!(live[0].text, "some fact, later draft");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The end-to-end property the write-once scheme exists for: agents adding
    /// at the same moment all keep their entry.
    #[test]
    fn concurrent_adds_all_survive() {
        let dir = temp_dir("concurrent-add");
        const WRITERS: usize = 16;

        std::thread::scope(|scope| {
            for i in 0..WRITERS {
                let dir = &dir;
                scope.spawn(move || add_entry(dir, &format!("entry {i}"), NO_CAP, None).unwrap());
            }
        });

        let found = entries(&dir);
        assert_eq!(found.len(), WRITERS, "{found:?}");
        let mut texts: Vec<&str> = found.iter().map(|e| e.text.as_str()).collect();
        texts.sort();
        let mut want: Vec<String> = (0..WRITERS).map(|i| format!("entry {i}")).collect();
        want.sort();
        assert_eq!(texts, want);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_distinguishes_gone_from_failure() {
        let dir = temp_dir("remove");
        let name = add_entry(&dir, "ephemeral", NO_CAP, None).unwrap();
        assert_eq!(remove_entry(&dir, &name), Ok(true));
        // Already gone reads as a concurrent supersede, not an error.
        assert_eq!(remove_entry(&dir, &name), Ok(false));
        // Ids never traverse paths or touch hidden names.
        assert!(remove_entry(&dir, "../escape.md").is_err());
        assert!(remove_entry(&dir, ".tmp-x.md").is_err());
        assert!(remove_entry(&dir, "plain.txt").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
