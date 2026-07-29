use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::*;

/// Byte cap for an unranged `view` — every sibling tool bounds its output;
/// whole-file view of a huge log would otherwise become a giant history entry
/// resent to the model every turn.
const MAX_VIEW_BYTES: u64 = 256 * 1024;

/// Gives agents tools to view, create, and edit files, and handle its view, str_replace, create,
/// and insert commands.
///
/// Tracks content fingerprints of files the agent has read so mutations fail if the file changed
/// on disk outside the tool (or was never read).
///
/// Cf https://platform.claude.com/docs/en/agents-and-tools/tool-use/text-editor-tool
#[derive(Default)]
pub struct TextEditorService {
    /// Paths the agent has successfully viewed/mutated → content fingerprint at that moment.
    read_files: Mutex<HashMap<PathBuf, u64>>,
}

impl TextEditorService {
    /// Tool schemas served by this service (static: no instance required).
    pub fn specs() -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "str_replace_based_edit_tool".to_string(),
            description: "A tool for viewing, creating, and editing files. Matches Anthropic tool."
                .to_string(),
            // Schema comes from [`Input`]: one flat object, so schemars emits the
            // root `type: "object"` Anthropic requires (a tagged per-command enum
            // would emit root `oneOf`, which Anthropic rejects).
            input_schema: super::tool_input_schema::<Input>(),
        }]
    }
}

impl ToolService for TextEditorService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        Self::specs()
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        _ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input) {
                Ok(input) => input,
                Err(e) => {
                    return generative_model::ToolResult::err(format!(
                        "Error deserializing text editor input: {e}"
                    ));
                }
            };
            match self.execute(input) {
                Ok(text) => generative_model::ToolResult::text(text),
                Err(e) => generative_model::ToolResult::err(e),
            }
        })
    }
}

impl TextEditorService {
    pub fn new() -> Self {
        Self::default()
    }

    fn execute(&self, input: Input) -> Result<String, String> {
        let Input {
            command,
            path,
            view_range,
            old_str,
            new_str,
            file_text,
            insert_line,
            insert_text,
        } = input;

        // One guard across check + mutate + record: concurrent same-turn edits
        // of one file serialize here, so the stamp that authorized a write can
        // never go stale between its check and the write it authorized, and
        // the recorded fingerprint always describes the bytes this command
        // left behind.
        let mut read_files = self.read_files.lock().unwrap_or_else(|e| e.into_inner());

        let output = match command {
            Command::View => view_path(&path, view_range)?,
            Command::StrReplace => {
                let old_str = require(old_str, "str_replace", "old_str")?;
                let new_str = require(new_str, "str_replace", "new_str")?;
                ensure_mutated_file_already_read(&read_files, Path::new(&path))?;
                str_replace_in_file(&path, &old_str, &new_str)?;
                "Successfully replaced text at exactly one location.".to_string()
            }
            // No read-stamp check: new files cannot have been read.
            Command::Create => {
                let file_text = require(file_text, "create", "file_text")?;
                create_file(&path, &file_text)?
            }
            Command::Insert => {
                let insert_line = require(insert_line, "insert", "insert_line")?;
                let insert_text = require(insert_text, "insert", "insert_text")?;
                ensure_mutated_file_already_read(&read_files, Path::new(&path))?;
                insert_in_file(&path, insert_line, &insert_text)?;
                format!("Successfully inserted text after line {insert_line}.")
            }
        };

        // On success, record the fingerprint so subsequent tool-driven edits
        // don't require a re-view, while external on-disk changes still fail
        // the guard. (A directory view has no fingerprint and is skipped.)
        if let Ok(fingerprint) = file_fingerprint(Path::new(&path)) {
            read_files.insert(PathBuf::from(path), fingerprint);
        }
        Ok(output)
    }
}

/// Missing per-command required field. Checked before the read-stamp guard so
/// a malformed call names its missing field (e.g. "str_replace requires
/// `old_str`") instead of failing as an unread file.
fn require<T>(field: Option<T>, command: &str, name: &str) -> Result<T, String> {
    field.ok_or_else(|| format!("{command} requires `{name}`"))
}

//
// File operations
//

/// Content fingerprint for the read-stamp guard. Hashes the file bytes:
/// mtime comparison misses external writes within the same filesystem clock
/// granule, and byte identity is the property the guard actually promises.
fn file_fingerprint(path: &Path) -> Result<u64, String> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(path)
        .map_err(|e| format!("Error reading file {path:?} to check if it was modified: {e}"))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(hasher.finish())
}

fn ensure_mutated_file_already_read(
    read_files: &HashMap<PathBuf, u64>,
    path: &Path,
) -> Result<(), String> {
    let Some(read_fingerprint) = read_files.get(path) else {
        return Err(format!(
            "File {path:?} was not read before being mutated. Read the file first."
        ));
    };
    if file_fingerprint(path)? != *read_fingerprint {
        return Err(format!(
            "File {path:?} was modified on disk after being read. Read the file again before mutating."
        ));
    }
    Ok(())
}

fn view_path(path: &str, view_range: Option<[i64; 2]>) -> Result<String, String> {
    let path_buf = PathBuf::from(path);
    let metadata = std::fs::metadata(&path_buf)
        .map_err(|e| format!("Error reading metadata for '{path}': {e}"))?;

    if metadata.is_dir() {
        if view_range.is_some() {
            return Err(format!(
                "view_range is only supported for files, not directories (path: '{path}')"
            ));
        }
        return view_directory(&path_buf, path);
    }

    if !metadata.is_file() {
        return Err(format!("Path '{path}' is neither a file nor a directory"));
    }

    view_file(&path_buf, path, view_range)
}

fn view_directory(path: &Path, path_display: &str) -> Result<String, String> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|e| format!("Error listing directory '{path_display}': {e}"))?
        .map(|entry| {
            entry.map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    format!("{name}/")
                } else {
                    name
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Error listing directory '{path_display}': {e}"))?;

    entries.sort();
    Ok(entries.join("\n"))
}

fn view_file(
    path: &Path,
    path_display: &str,
    view_range: Option<[i64; 2]>,
) -> Result<String, String> {
    use std::io::{BufReader, Read};

    let file = std::fs::File::open(path)
        .map_err(|e| format!("Error reading file '{path_display}': {e}"))?;
    let mut reader = BufReader::new(file);

    let Some([start, end]) = view_range else {
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if len > MAX_VIEW_BYTES {
            return Err(format!(
                "File '{path_display}' is {len} bytes — over the {MAX_VIEW_BYTES}-byte cap for \
                 an unranged view. Pass view_range to read a slice (e.g. [1, 400]), or search \
                 it with bash + rg."
            ));
        }
        let mut data = String::new();
        reader
            .read_to_string(&mut data)
            .map_err(|e| format!("Error reading file '{path_display}': {e}"))?;
        return Ok(data);
    };

    read_lines_in_range(&mut reader, start, end, path_display)
}

/// Stream a 1-indexed inclusive line range from `reader`.
///
/// Skips lines before `start` and stops after `end` so the rest of the file is not read.
/// `end == -1` means "through the last line". An `end` past EOF is clamped.
fn read_lines_in_range<R: std::io::BufRead>(
    reader: &mut R,
    start: i64,
    end: i64,
    path_display: &str,
) -> Result<String, String> {
    if start < 1 {
        return Err(format!(
            "view_range start must be >= 1 (1-indexed), got {start}"
        ));
    }
    if end != -1 && end < start {
        return Err(format!(
            "view_range end ({end}) must be >= start ({start}), or -1 for end of file"
        ));
    }

    let mut line_no = 0i64;
    let mut selected: Vec<String> = Vec::new();
    let mut buf = String::new();

    loop {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .map_err(|e| format!("Error reading file '{path_display}': {e}"))?;
        if n == 0 {
            break;
        }
        line_no += 1;

        // `read_line` keeps the trailing delimiter; drop it so joining with `\n` is correct.
        if buf.ends_with('\n') {
            buf.pop();
            if buf.ends_with('\r') {
                buf.pop();
            }
        }

        if line_no < start {
            continue;
        }
        // end past EOF is clamped by simply stopping at EOF below.
        if end != -1 && line_no > end {
            break;
        }

        selected.push(std::mem::take(&mut buf));

        if end != -1 && line_no == end {
            break;
        }
    }

    if line_no == 0 {
        return Err("view_range specified but file is empty".to_string());
    }
    if start > line_no {
        return Err(format!(
            "view_range start ({start}) is past end of file ({line_no} lines)"
        ));
    }

    Ok(selected.join("\n"))
}

fn str_replace_in_file(path: &str, old_str: &str, new_str: &str) -> Result<(), String> {
    enum SearchResult {
        None,
        One { offset: usize },
        Multiple { match_count: usize },
    }

    impl SearchResult {
        fn match_count(&self) -> usize {
            match self {
                SearchResult::None => 0,
                SearchResult::One { .. } => 1,
                SearchResult::Multiple { match_count } => *match_count,
            }
        }
    }

    let old_file_text =
        std::fs::read_to_string(path).map_err(|e| format!("Error reading file '{path}': {e}"))?;

    // Find exact substring matches (not regex) so agent-provided text is literal.
    let search_result = old_file_text.match_indices(old_str).fold(
        SearchResult::None,
        |acc, (offset, _)| match acc {
            SearchResult::None => SearchResult::One { offset },
            SearchResult::One { .. } => SearchResult::Multiple { match_count: 2 },
            SearchResult::Multiple { match_count } => SearchResult::Multiple {
                match_count: match_count + 1,
            },
        },
    );

    let SearchResult::One { offset } = search_result else {
        return Err(format!(
            concat!(
                "Expected to find exactly one occurrence of the old_str in the file, but ",
                "found {} occurrences. Please refine your `old_str` parameter accordingly so ",
                "there is exactly one match."
            ),
            search_result.match_count()
        ));
    };

    let new_file_text = format!(
        "{}{}{}",
        &old_file_text[..offset],
        new_str,
        &old_file_text[offset + old_str.len()..]
    );

    atomically_write_file(Path::new(path), new_file_text.as_bytes())
        .map_err(|e| format!("Error writing file '{path}': {e}"))
}

fn create_file(path: &str, file_text: &str) -> Result<String, String> {
    let path_buf = PathBuf::from(path);
    if path_buf.exists() {
        return Err(format!(
            "File already exists at '{path}'. Use str_replace or insert to modify it, or choose a new path."
        ));
    }
    if let Some(parent) = path_buf.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Err(format!(
            "Error creating parent directories for '{path}': {e}"
        ));
    }
    atomically_write_file(&path_buf, file_text.as_bytes())
        .map_err(|e| format!("Error creating file '{path}': {e}"))?;
    Ok(format!("Created file '{path}'."))
}

/// Insert `insert_text` after line `insert_line` (0 = beginning of file).
fn insert_in_file(path: &str, insert_line: i64, insert_text: &str) -> Result<(), String> {
    if insert_line < 0 {
        return Err(format!("insert_line must be >= 0, got {insert_line}"));
    }

    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Error reading file '{path}': {e}"))?;

    let new_content = insert_after_line(&content, insert_line, insert_text)?;
    atomically_write_file(Path::new(path), new_content.as_bytes())
        .map_err(|e| format!("Error writing file '{path}': {e}"))
}

fn insert_after_line(content: &str, insert_line: i64, insert_text: &str) -> Result<String, String> {
    if insert_line == 0 {
        return Ok(format!("{insert_text}{content}"));
    }

    let mut newlines_seen = 0i64;
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            newlines_seen += 1;
            if newlines_seen == insert_line {
                let at = idx + ch.len_utf8();
                let mut out = String::with_capacity(content.len() + insert_text.len());
                out.push_str(&content[..at]);
                out.push_str(insert_text);
                out.push_str(&content[at..]);
                return Ok(out);
            }
        }
    }

    let total_lines = if content.is_empty() {
        0
    } else if content.ends_with('\n') {
        newlines_seen
    } else {
        newlines_seen + 1
    };

    if insert_line == total_lines {
        // A last line without trailing newline: add one so the inserted text
        // starts on its own line instead of gluing onto the last line. The
        // model cannot compensate — ranged view strips trailing-newline info.
        if !content.is_empty() && !content.ends_with('\n') {
            return Ok(format!("{content}\n{insert_text}"));
        }
        return Ok(format!("{content}{insert_text}"));
    }

    Err(format!(
        "insert_line ({insert_line}) is past end of file ({total_lines} lines)"
    ))
}

fn atomically_write_file(path: &Path, content: &[u8]) -> Result<(), String> {
    // Resolve symlinks first: AtomicWriteFile replaces the path it is given,
    // so writing through the raw path would turn a symlink into a regular
    // file and silently fork its content away from the target.
    let target = match path.canonicalize() {
        Ok(resolved) => resolved,
        // New file (create): nothing to resolve yet.
        Err(_) => path.to_path_buf(),
    };
    let mut file = atomic_write_file::AtomicWriteFile::options()
        .open(&target)
        .map_err(|e| e.to_string())?;
    file.write_all(content).map_err(|e| e.to_string())?;
    file.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Command discriminator for the text-editor tool.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    View,
    StrReplace,
    Create,
    Insert,
}

/// Wire / JSON-Schema shape for the text-editor tool: one flat object with all fields.
///
/// Anthropic requires `input_schema.type == "object"`. Schemars emits that for this struct.
/// Per-command required fields stay `Option` here; [`TextEditorService::execute`] validates
/// them in its match arms, naming any missing field.
///
/// ```json
/// { "command": "view", "path": "primes.py" }
/// ```
///
/// See: https://platform.claude.com/docs/en/agents-and-tools/tool-use/text-editor-tool
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct Input {
    /// The command to execute.
    pub command: Command,
    /// Path to the file or directory.
    pub path: String,
    /// Optional 1-indexed inclusive line range `[start, end]` for `view`.
    /// Use `-1` for `end` to read through the end of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_range: Option<[i64; 2]>,
    /// For `str_replace`: exact text to replace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_str: Option<String>,
    /// For `str_replace`: replacement text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_str: Option<String>,
    /// For `create`: content of the new file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_text: Option<String>,
    /// For `insert`: line number after which to insert (`0` = beginning of file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_line: Option<i64>,
    /// For `insert`: text to insert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostWorker;
    use crate::test_support::{result_text, temp_dir};
    use serde_json::json;
    use std::sync::Arc;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// HostWorker with a single shared [`TextEditorService`] (stateful read-stamp map).
    fn harness() -> Arc<HostWorker> {
        Arc::new(HostWorker::new(
            "test",
            vec![Arc::new(TextEditorService::new()) as Arc<dyn ToolService>],
        ))
    }

    /// Dispatch a wire-shaped input (same flat JSON the model sends).
    fn dispatch(
        harness: &Arc<HostWorker>,
        input: serde_json::Value,
    ) -> generative_model::ToolResult {
        futures::executor::block_on(harness.dispatch_tool_use(
            generative_model::ToolUse {
                id: "test".into(),
                name: "str_replace_based_edit_tool".into(),
                input,
            },
            HostDispatchContext {
                agent_id: uuid::Uuid::nil(),
                cancel: crate::core::CancelToken::new(),
            },
        ))
    }

    /// A malformed call must name its missing field, not fail some later
    /// guard (the path here was never read, so a reordered check would
    /// surface the read-stamp error instead).
    #[test]
    fn str_replace_missing_fields_errors() {
        let harness = harness();
        let result = dispatch(
            &harness,
            json!({"command": "str_replace", "path": "x.py", "new_str": "y"}),
        );
        assert!(result.is_error);
        let err = result_text(&result);
        assert!(err.contains("old_str"), "{err}");
    }

    #[test]
    fn schemars_schema_is_object_type() {
        let schema = crate::tool_services::tool_input_schema::<Input>();
        assert_eq!(
            schema.get("type").and_then(|t| t.as_str()),
            Some("object"),
            "{schema}"
        );
        assert!(schema.get("properties").is_some(), "{schema}");
        // Must not be a root oneOf (Anthropic rejects that).
        assert!(schema.get("oneOf").is_none(), "{schema}");
    }

    #[test]
    fn insert_after_line_helpers() {
        assert_eq!(
            insert_after_line("a\nb\nc", 0, "X\n").unwrap(),
            "X\na\nb\nc"
        );
        assert_eq!(
            insert_after_line("a\nb\nc", 1, "X\n").unwrap(),
            "a\nX\nb\nc"
        );
        // Appending after a last line with no trailing newline supplies the
        // separator itself; "Z" must not glue onto "c".
        assert_eq!(insert_after_line("a\nb\nc", 3, "Z").unwrap(), "a\nb\nc\nZ");
        assert_eq!(
            insert_after_line("a\nb\nc\n", 3, "Z\n").unwrap(),
            "a\nb\nc\nZ\n"
        );
        assert!(
            insert_after_line("a\nb", 5, "x")
                .unwrap_err()
                .contains("past end")
        );
    }

    fn range_from(data: &str, start: i64, end: i64) -> Result<String, String> {
        read_lines_in_range(
            &mut std::io::Cursor::new(data.as_bytes()),
            start,
            end,
            "<test>",
        )
    }

    #[test]
    fn view_range_middle_lines() {
        assert_eq!(range_from("a\nb\nc\nd\ne\n", 2, 4).unwrap(), "b\nc\nd");
    }

    #[test]
    fn view_range_to_end_with_minus_one() {
        assert_eq!(range_from("a\nb\nc\nd\ne", 3, -1).unwrap(), "c\nd\ne");
    }

    #[test]
    fn view_range_clamps_end_past_eof() {
        assert_eq!(range_from("a\nb\nc", 2, 99).unwrap(), "b\nc");
    }

    #[test]
    fn view_range_rejects_start_past_eof() {
        assert!(range_from("a\nb", 5, -1).unwrap_err().contains("past end"));
    }

    #[test]
    fn view_range_rejects_start_zero() {
        assert!(range_from("a\nb", 0, 1).unwrap_err().contains(">= 1"));
    }

    #[test]
    fn view_range_rejects_end_before_start() {
        assert!(
            range_from("a\nb\nc", 3, 1)
                .unwrap_err()
                .contains("must be >= start")
        );
    }

    /// Ensures we stop after `end` instead of draining the reader.
    #[test]
    fn view_range_stops_after_end() {
        use std::io::Read;

        /// Fails if more than `remaining` bytes are pulled from the underlying reader.
        struct BudgetReader<R> {
            inner: R,
            remaining: usize,
        }
        impl<R: Read> Read for BudgetReader<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Err(std::io::Error::other("read past budget"));
                }
                let want = buf.len().min(self.remaining);
                let n = self.inner.read(&mut buf[..want])?;
                self.remaining -= n;
                Ok(n)
            }
        }

        // "1\n2\n3\n..." — lines 2–3 only need the first three lines (`1\n2\n3\n` = 6 bytes).
        let data = b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let budget = BudgetReader {
            inner: std::io::Cursor::new(&data[..]),
            remaining: 6,
        };
        // capacity 1 so BufReader cannot prefetch past the line we stop on.
        let mut reader = std::io::BufReader::with_capacity(1, budget);
        let out = read_lines_in_range(&mut reader, 2, 3, "<test>").unwrap();
        assert_eq!(out, "2\n3");
    }

    #[test]
    fn view_then_str_replace() {
        let tmp = temp_dir("text-editor");
        let path = write_file(tmp.path(), "primes.py", "for x in y\n");
        let path_str = path.to_string_lossy().into_owned();
        let harness = harness();

        let view = dispatch(&harness, json!({"command": "view", "path": &path_str}));
        assert!(!view.is_error, "{}", result_text(&view));
        assert_eq!(result_text(&view), "for x in y\n");

        let edit = dispatch(
            &harness,
            json!({
                "command": "str_replace",
                "path": &path_str,
                "old_str": "for x in y",
                "new_str": "for x in y:",
            }),
        );
        assert!(!edit.is_error, "{}", result_text(&edit));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "for x in y:\n");
    }

    /// Editing through a symlink must rewrite the *target*, not replace the
    /// symlink with a regular file (which forks the content).
    #[test]
    fn str_replace_through_symlink_edits_target() {
        let tmp = temp_dir("text-editor");
        let target = write_file(tmp.path(), "real.md", "old text\n");
        let link = tmp.path().join("link.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let link_str = link.to_string_lossy().into_owned();
        let harness = harness();

        let view = dispatch(&harness, json!({"command": "view", "path": &link_str}));
        assert!(!view.is_error, "{}", result_text(&view));

        let edit = dispatch(
            &harness,
            json!({
                "command": "str_replace",
                "path": &link_str,
                "old_str": "old text",
                "new_str": "new text",
            }),
        );
        assert!(!edit.is_error, "{}", result_text(&edit));
        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "symlink must survive the edit"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new text\n");
    }

    #[test]
    fn str_replace_without_view_errors() {
        let tmp = temp_dir("text-editor");
        let path = write_file(tmp.path(), "f.py", "a\n");
        let harness = harness();

        let result = dispatch(
            &harness,
            json!({
                "command": "str_replace",
                "path": path.to_string_lossy(),
                "old_str": "a",
                "new_str": "b",
            }),
        );
        assert!(result.is_error);
        assert!(result_text(&result).contains("was not read"));
    }

    #[test]
    fn external_modification_blocks_str_replace() {
        let tmp = temp_dir("text-editor");
        let path = write_file(tmp.path(), "f.py", "hello\n");
        let path_str = path.to_string_lossy().into_owned();
        let harness = harness();

        let view = dispatch(&harness, json!({"command": "view", "path": &path_str}));
        assert!(!view.is_error);

        // Immediate external write — the guard fingerprints content, so no
        // sleep or mtime bump is needed even on coarse filesystem clocks.
        std::fs::write(&path, "hello externally\n").unwrap();

        let edit = dispatch(
            &harness,
            json!({
                "command": "str_replace",
                "path": &path_str,
                "old_str": "hello",
                "new_str": "hi",
            }),
        );
        assert!(edit.is_error, "{}", result_text(&edit));
        assert!(result_text(&edit).contains("modified on disk"));
    }

    #[test]
    fn create_insert_and_view_range() {
        let tmp = temp_dir("text-editor");
        let path = tmp.path().join("new.py");
        let path_str = path.to_string_lossy().into_owned();
        let harness = harness();

        let create = dispatch(
            &harness,
            json!({
                "command": "create",
                "path": &path_str,
                "file_text": "line1\nline2\nline3\n",
            }),
        );
        assert!(!create.is_error, "{}", result_text(&create));

        // Create records the read stamp, so insert may proceed without an
        // explicit view.
        let insert = dispatch(
            &harness,
            json!({
                "command": "insert",
                "path": &path_str,
                "insert_line": 1,
                "insert_text": "inserted\n",
            }),
        );
        assert!(!insert.is_error, "{}", result_text(&insert));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\ninserted\nline2\nline3\n"
        );

        let view = dispatch(
            &harness,
            json!({"command": "view", "path": &path_str, "view_range": [2, 3]}),
        );
        assert!(!view.is_error, "{}", result_text(&view));
        assert_eq!(result_text(&view), "inserted\nline2");
    }

    #[test]
    fn create_rejects_existing_file() {
        let tmp = temp_dir("text-editor");
        let path = write_file(tmp.path(), "exists.py", "x\n");
        let harness = harness();

        let result = dispatch(
            &harness,
            json!({
                "command": "create",
                "path": path.to_string_lossy(),
                "file_text": "y\n",
            }),
        );
        assert!(result.is_error);
        assert!(result_text(&result).contains("already exists"));
    }

    /// Unranged view of an over-cap file must refuse with a pointer at
    /// view_range instead of dumping the whole file into context.
    #[test]
    fn unranged_view_of_huge_file_is_capped() {
        let tmp = temp_dir("text-editor");
        let big = "x".repeat((MAX_VIEW_BYTES + 1) as usize);
        let path = write_file(tmp.path(), "huge.log", &big);
        let harness = harness();

        let view = dispatch(
            &harness,
            json!({"command": "view", "path": path.to_string_lossy()}),
        );
        assert!(view.is_error, "{}", result_text(&view));
        assert!(
            result_text(&view).contains("view_range"),
            "{}",
            result_text(&view)
        );

        // A ranged view of the same file still works.
        let ranged = dispatch(
            &harness,
            json!({"command": "view", "path": path.to_string_lossy(), "view_range": [1, 1]}),
        );
        assert!(!ranged.is_error, "{}", result_text(&ranged));
    }
}
