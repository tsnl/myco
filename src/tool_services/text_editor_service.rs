use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use super::*;

/// Gives agents tools to view, create, and edit files, and handle its view, str_replace, create,
/// and insert commands.
///
/// Tracks last-modified times of files the agent has read so mutations fail if the file changed on
/// disk outside the tool (or was never read).
///
/// Cf https://platform.claude.com/docs/en/agents-and-tools/tool-use/text-editor-tool
#[derive(Default)]
pub struct TextEditorService {
    /// Paths the agent has successfully viewed/mutated → last-modified time at that moment.
    read_files: Mutex<HashMap<PathBuf, SystemTime>>,
}

impl TextEditorService {
    /// Tool schemas served by this service (static: no instance required).
    pub fn specs() -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "str_replace_based_edit_tool".to_string(),
            description: "A tool for viewing, creating, and editing files. Matches Anthropic tool."
                .to_string(),
            // Schema comes from [`Input`] (flat object). The tagged [`ParsedInput`] enum is used
            // at runtime after conversion — schemars would emit root `oneOf` for that enum,
            // which Anthropic rejects (missing `input_schema.type`).
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
            let parsed = match ParsedInput::try_from(input) {
                Ok(parsed) => parsed,
                Err(e) => {
                    return generative_model::ToolResult::err(e);
                }
            };
            self.execute(parsed)
        })
    }
}

impl TextEditorService {
    pub fn new() -> Self {
        Self::default()
    }

    fn execute(&self, input: ParsedInput) -> generative_model::ToolResult {
        for path in input.mutated_files() {
            if let Err(e) = self.ensure_mutated_file_already_read(&path) {
                return generative_model::ToolResult::err(e);
            }
        }

        let result = match &input {
            ParsedInput::View { path, view_range } => self.cmd_view(path, *view_range),
            ParsedInput::StrReplace {
                path,
                old_str,
                new_str,
            } => self.cmd_str_replace(path, old_str, new_str),
            ParsedInput::Create { path, file_text } => self.cmd_create(path, file_text),
            ParsedInput::Insert {
                path,
                insert_line,
                insert_text,
            } => self.cmd_insert(path, *insert_line, insert_text),
        };

        // On success, record LMT so subsequent tool-driven edits don't require a re-view, while
        // external on-disk changes still fail the guard.
        if !result.is_error {
            self.record_read_files(input.read_files());
        }

        result
    }

    fn ensure_mutated_file_already_read(&self, path: &Path) -> Result<(), String> {
        let read_files = self
            .read_files
            .lock()
            .map_err(|e| format!("read_files lock poisoned: {e}"))?;
        let Some(last_read_lmt) = read_files.get(path) else {
            return Err(format!(
                "File {path:?} was not read before being mutated. Read the file first."
            ));
        };
        let current_lmt = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|e| {
                format!("Error getting metadata for file {path:?} to check if it was modified: {e}")
            })?;
        if current_lmt > *last_read_lmt {
            return Err(format!(
                "File {path:?} was modified on disk after being read. Read the file again before mutating."
            ));
        }
        Ok(())
    }

    fn record_read_files(&self, paths: Vec<PathBuf>) {
        let Ok(mut read_files) = self.read_files.lock() else {
            return;
        };
        for path in paths {
            if let Ok(modified_time) = std::fs::metadata(&path).and_then(|m| m.modified()) {
                read_files.insert(path, modified_time);
            }
        }
    }

    fn cmd_view(&self, path: &str, view_range: Option<[i64; 2]>) -> generative_model::ToolResult {
        match view_path(path, view_range) {
            Ok(text) => generative_model::ToolResult::text(text),
            Err(e) => generative_model::ToolResult::err(e),
        }
    }

    fn cmd_str_replace(
        &self,
        path: &str,
        old_str: &str,
        new_str: &str,
    ) -> generative_model::ToolResult {
        match str_replace_in_file(path, old_str, new_str) {
            Ok(()) => generative_model::ToolResult::text(
                "Successfully replaced text at exactly one location.",
            ),
            Err(e) => generative_model::ToolResult::err(e),
        }
    }

    fn cmd_create(&self, path: &str, file_text: &str) -> generative_model::ToolResult {
        let path_buf = PathBuf::from(path);
        if path_buf.exists() {
            return generative_model::ToolResult::err(format!(
                "File already exists at '{path}'. Use str_replace or insert to modify it, or choose a new path."
            ));
        }
        if let Some(parent) = path_buf.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return generative_model::ToolResult::err(format!(
                "Error creating parent directories for '{path}': {e}"
            ));
        }
        match atomically_write_file(&path_buf, file_text.as_bytes()) {
            Ok(()) => generative_model::ToolResult::text(format!("Created file '{path}'.")),
            Err(e) => {
                generative_model::ToolResult::err(format!("Error creating file '{path}': {e}"))
            }
        }
    }

    fn cmd_insert(
        &self,
        path: &str,
        insert_line: i64,
        insert_text: &str,
    ) -> generative_model::ToolResult {
        match insert_in_file(path, insert_line, insert_text) {
            Ok(()) => generative_model::ToolResult::text(format!(
                "Successfully inserted text after line {insert_line}."
            )),
            Err(e) => generative_model::ToolResult::err(e),
        }
    }
}

//
// File operations
//

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
/// Convert to [`ParsedInput`] after deserialize so execution can pattern-match per command.
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

/// Type-safe per-command text-editor input (runtime form).
///
/// Prefer deserializing [`Input`] from the wire, then [`ParsedInput::try_from`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ParsedInput {
    /// Examine a file or list a directory.
    View {
        /// Path to the file or directory to view.
        path: String,
        /// Optional 1-indexed inclusive line range `[start, end]`.
        /// Use `-1` for `end` to read through the end of the file.
        /// Only applies when viewing files, not directories.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view_range: Option<[i64; 2]>,
    },
    /// Replace exactly one occurrence of `old_str` with `new_str` in a file.
    StrReplace {
        /// Path to the file to modify.
        path: String,
        /// Exact text to replace (must match whitespace and indentation).
        old_str: String,
        /// Replacement text.
        new_str: String,
    },
    /// Create a new file with the given contents.
    Create {
        /// Path where the new file should be created.
        path: String,
        /// Content to write to the new file.
        file_text: String,
    },
    /// Insert text after a given line number.
    Insert {
        /// Path to the file to modify.
        path: String,
        /// Line number after which to insert text (`0` = beginning of file).
        insert_line: i64,
        /// Text to insert.
        insert_text: String,
    },
}

impl From<ParsedInput> for Input {
    fn from(input: ParsedInput) -> Self {
        match input {
            ParsedInput::View { path, view_range } => Self {
                command: Command::View,
                path,
                view_range,
                old_str: None,
                new_str: None,
                file_text: None,
                insert_line: None,
                insert_text: None,
            },
            ParsedInput::StrReplace {
                path,
                old_str,
                new_str,
            } => Self {
                command: Command::StrReplace,
                path,
                view_range: None,
                old_str: Some(old_str),
                new_str: Some(new_str),
                file_text: None,
                insert_line: None,
                insert_text: None,
            },
            ParsedInput::Create { path, file_text } => Self {
                command: Command::Create,
                path,
                view_range: None,
                old_str: None,
                new_str: None,
                file_text: Some(file_text),
                insert_line: None,
                insert_text: None,
            },
            ParsedInput::Insert {
                path,
                insert_line,
                insert_text,
            } => Self {
                command: Command::Insert,
                path,
                view_range: None,
                old_str: None,
                new_str: None,
                file_text: None,
                insert_line: Some(insert_line),
                insert_text: Some(insert_text),
            },
        }
    }
}

impl TryFrom<Input> for ParsedInput {
    type Error = String;

    fn try_from(input: Input) -> Result<Self, Self::Error> {
        match input.command {
            Command::View => Ok(ParsedInput::View {
                path: input.path,
                view_range: input.view_range,
            }),
            Command::StrReplace => {
                let old_str = input
                    .old_str
                    .ok_or_else(|| "str_replace requires `old_str`".to_string())?;
                let new_str = input
                    .new_str
                    .ok_or_else(|| "str_replace requires `new_str`".to_string())?;
                Ok(ParsedInput::StrReplace {
                    path: input.path,
                    old_str,
                    new_str,
                })
            }
            Command::Create => {
                let file_text = input
                    .file_text
                    .ok_or_else(|| "create requires `file_text`".to_string())?;
                Ok(ParsedInput::Create {
                    path: input.path,
                    file_text,
                })
            }
            Command::Insert => {
                let insert_line = input
                    .insert_line
                    .ok_or_else(|| "insert requires `insert_line`".to_string())?;
                let insert_text = input
                    .insert_text
                    .ok_or_else(|| "insert requires `insert_text`".to_string())?;
                Ok(ParsedInput::Insert {
                    path: input.path,
                    insert_line,
                    insert_text,
                })
            }
        }
    }
}

impl ParsedInput {
    /// Paths this command will modify. `create` is excluded: new files cannot have been read.
    fn mutated_files(&self) -> Vec<PathBuf> {
        match self {
            ParsedInput::View { .. } | ParsedInput::Create { .. } => Vec::default(),
            ParsedInput::StrReplace { path, .. } | ParsedInput::Insert { path, .. } => {
                vec![PathBuf::from(path)]
            }
        }
    }

    /// Paths whose last-modified time should be recorded after a successful command.
    fn read_files(&self) -> Vec<PathBuf> {
        match self {
            ParsedInput::View { path, .. }
            | ParsedInput::StrReplace { path, .. }
            | ParsedInput::Create { path, .. }
            | ParsedInput::Insert { path, .. } => {
                vec![PathBuf::from(path)]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostWorker;
    use serde_json::json;
    use std::sync::Arc;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir() -> TempDir {
        let dir = std::env::temp_dir().join(format!("myco-text-editor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn tool_use(parsed: ParsedInput) -> generative_model::ToolUse {
        // Wire format is [`Input`]; [`ParsedInput`] is only the runtime form.
        let input = Input::from(parsed);
        generative_model::ToolUse {
            id: "test".into(),
            name: "str_replace_based_edit_tool".into(),
            input: serde_json::to_value(input).unwrap(),
        }
    }

    fn result_text(result: &generative_model::ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                generative_model::Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// HostWorker with a single shared [`TextEditorService`] (stateful read LMT map).
    fn harness() -> Arc<HostWorker> {
        Arc::new(HostWorker::new(
            "test",
            vec![Arc::new(TextEditorService::new()) as Arc<dyn ToolService>],
        ))
    }

    fn dispatch(harness: &Arc<HostWorker>, input: ParsedInput) -> generative_model::ToolResult {
        futures::executor::block_on(harness.dispatch_tool_use(
            tool_use(input),
            HostDispatchContext {
                agent_id: uuid::Uuid::nil(),
                cancel: crate::core::CancelToken::new(),
                agent_root: None,
            },
        ))
    }

    fn assert_input_roundtrip(parsed: ParsedInput, expected_json: serde_json::Value) {
        let input = Input::from(parsed.clone());
        let value = serde_json::to_value(&input).unwrap();
        assert_eq!(value, expected_json);
        let wire: Input = serde_json::from_value(value).unwrap();
        assert_eq!(ParsedInput::try_from(wire).unwrap(), parsed);
    }

    #[test]
    fn view_roundtrip() {
        assert_input_roundtrip(
            ParsedInput::View {
                path: "primes.py".into(),
                view_range: None,
            },
            json!({
                "command": "view",
                "path": "primes.py",
            }),
        );
    }

    #[test]
    fn view_with_range_roundtrip() {
        assert_input_roundtrip(
            ParsedInput::View {
                path: "primes.py".into(),
                view_range: Some([1, 10]),
            },
            json!({
                "command": "view",
                "path": "primes.py",
                "view_range": [1, 10],
            }),
        );
    }

    #[test]
    fn str_replace_roundtrip() {
        assert_input_roundtrip(
            ParsedInput::StrReplace {
                path: "primes.py".into(),
                old_str: "    for num in range(2, limit + 1)".into(),
                new_str: "    for num in range(2, limit + 1):".into(),
            },
            json!({
                "command": "str_replace",
                "path": "primes.py",
                "old_str": "    for num in range(2, limit + 1)",
                "new_str": "    for num in range(2, limit + 1):",
            }),
        );
    }

    #[test]
    fn create_roundtrip() {
        assert_input_roundtrip(
            ParsedInput::Create {
                path: "test_primes.py".into(),
                file_text: "print('hi')\n".into(),
            },
            json!({
                "command": "create",
                "path": "test_primes.py",
                "file_text": "print('hi')\n",
            }),
        );
    }

    #[test]
    fn insert_roundtrip() {
        assert_input_roundtrip(
            ParsedInput::Insert {
                path: "primes.py".into(),
                insert_line: 0,
                insert_text: "\"\"\"Module docstring.\"\"\"\n".into(),
            },
            json!({
                "command": "insert",
                "path": "primes.py",
                "insert_line": 0,
                "insert_text": "\"\"\"Module docstring.\"\"\"\n",
            }),
        );
    }

    #[test]
    fn rejects_unknown_command() {
        let err = serde_json::from_value::<Input>(json!({
            "command": "delete",
            "path": "x.py",
        }))
        .unwrap_err();
        assert!(err.to_string().contains("unknown variant") || err.to_string().contains("delete"));
    }

    #[test]
    fn str_replace_missing_fields_errors_on_convert() {
        let input = Input {
            command: Command::StrReplace,
            path: "x.py".into(),
            view_range: None,
            old_str: None,
            new_str: Some("y".into()),
            file_text: None,
            insert_line: None,
            insert_text: None,
        };
        let err = ParsedInput::try_from(input).unwrap_err();
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
        let tmp = temp_dir();
        let path = write_file(&tmp.0, "primes.py", "for x in y\n");
        let path_str = path.to_string_lossy().into_owned();
        let harness = harness();

        let view = dispatch(
            &harness,
            ParsedInput::View {
                path: path_str.clone(),
                view_range: None,
            },
        );
        assert!(!view.is_error, "{}", result_text(&view));
        assert_eq!(result_text(&view), "for x in y\n");

        let edit = dispatch(
            &harness,
            ParsedInput::StrReplace {
                path: path_str.clone(),
                old_str: "for x in y".into(),
                new_str: "for x in y:".into(),
            },
        );
        assert!(!edit.is_error, "{}", result_text(&edit));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "for x in y:\n");
    }

    /// Editing through a symlink must rewrite the *target*, not replace the
    /// symlink with a regular file (which forks the content).
    #[test]
    fn str_replace_through_symlink_edits_target() {
        let tmp = temp_dir();
        let target = write_file(&tmp.0, "real.md", "old text\n");
        let link = tmp.0.join("link.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let link_str = link.to_string_lossy().into_owned();
        let harness = harness();

        let view = dispatch(
            &harness,
            ParsedInput::View {
                path: link_str.clone(),
                view_range: None,
            },
        );
        assert!(!view.is_error, "{}", result_text(&view));

        let edit = dispatch(
            &harness,
            ParsedInput::StrReplace {
                path: link_str,
                old_str: "old text".into(),
                new_str: "new text".into(),
            },
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
        let tmp = temp_dir();
        let path = write_file(&tmp.0, "f.py", "a\n");
        let harness = harness();

        let result = dispatch(
            &harness,
            ParsedInput::StrReplace {
                path: path.to_string_lossy().into(),
                old_str: "a".into(),
                new_str: "b".into(),
            },
        );
        assert!(result.is_error);
        assert!(result_text(&result).contains("was not read"));
    }

    #[test]
    fn external_modification_blocks_str_replace() {
        let tmp = temp_dir();
        let path = write_file(&tmp.0, "f.py", "hello\n");
        let path_str = path.to_string_lossy().into_owned();
        let harness = harness();

        let view = dispatch(
            &harness,
            ParsedInput::View {
                path: path_str.clone(),
                view_range: None,
            },
        );
        assert!(!view.is_error);

        // Ensure mtime advances past the recorded read time.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "hello externally\n").unwrap();
        // Bump mtime explicitly in case the sleep was not enough on coarse FS clocks.
        let now = SystemTime::now() + std::time::Duration::from_secs(2);
        filetime_set_mtime(&path, now);

        let edit = dispatch(
            &harness,
            ParsedInput::StrReplace {
                path: path_str,
                old_str: "hello".into(),
                new_str: "hi".into(),
            },
        );
        assert!(edit.is_error, "{}", result_text(&edit));
        assert!(result_text(&edit).contains("modified on disk"));
    }

    #[test]
    fn create_insert_and_view_range() {
        let tmp = temp_dir();
        let path = tmp.0.join("new.py");
        let path_str = path.to_string_lossy().into_owned();
        let harness = harness();

        let create = dispatch(
            &harness,
            ParsedInput::Create {
                path: path_str.clone(),
                file_text: "line1\nline2\nline3\n".into(),
            },
        );
        assert!(!create.is_error, "{}", result_text(&create));

        // Create records LMT, so insert may proceed without an explicit view.
        let insert = dispatch(
            &harness,
            ParsedInput::Insert {
                path: path_str.clone(),
                insert_line: 1,
                insert_text: "inserted\n".into(),
            },
        );
        assert!(!insert.is_error, "{}", result_text(&insert));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\ninserted\nline2\nline3\n"
        );

        let view = dispatch(
            &harness,
            ParsedInput::View {
                path: path_str,
                view_range: Some([2, 3]),
            },
        );
        assert!(!view.is_error, "{}", result_text(&view));
        assert_eq!(result_text(&view), "inserted\nline2");
    }

    #[test]
    fn create_rejects_existing_file() {
        let tmp = temp_dir();
        let path = write_file(&tmp.0, "exists.py", "x\n");
        let harness = harness();

        let result = dispatch(
            &harness,
            ParsedInput::Create {
                path: path.to_string_lossy().into(),
                file_text: "y\n".into(),
            },
        );
        assert!(result.is_error);
        assert!(result_text(&result).contains("already exists"));
    }

    /// Set mtime without depending on the `filetime` crate.
    fn filetime_set_mtime(path: &Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }
}
