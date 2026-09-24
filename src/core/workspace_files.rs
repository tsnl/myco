//! Workspace file capabilities and browser URLs shared by tool output and HTTP
//! presentation. URL generation does not widen the static server's file access.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cap_std::fs::{Dir, OpenOptions};

#[derive(Clone)]
pub struct WorkspaceFiles {
    root: PathBuf,
    directory: Arc<Dir>,
    base_path: String,
}

impl WorkspaceFiles {
    pub fn open(root: &Path) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("resolve workspace directory: {e}"))?;
        let directory = Dir::open_ambient_dir(&root, cap_std::ambient_authority())
            .map_err(|e| format!("open workspace directory: {e}"))?;
        Ok(Self {
            root,
            directory: Arc::new(directory),
            base_path: String::new(),
        })
    }

    pub fn with_base_path(mut self, base_path: String) -> Self {
        self.base_path = base_path;
        self
    }

    pub fn route(&self, path: &str) -> String {
        format!("{}{path}", self.base_path)
    }

    /// Encode a path relative to the captured workspace, including its URL prefix.
    pub fn path_url(&self, path: &Path) -> String {
        self.route(&path_url(path))
    }

    /// Open the same regular file (or directory index) that the static server serves.
    /// Capability-relative IO prevents symlinks and parent paths escaping the root.
    pub fn open_file(&self, path: &Path) -> std::io::Result<(std::fs::File, PathBuf)> {
        open_file(&self.directory, path)
    }

    /// Resolve a literal filesystem path to an existing, served file. Characters
    /// such as `#`, `?`, and `%` belong to the filename, not URL syntax.
    pub fn link(&self, path: &Path) -> Result<String, String> {
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.root)
                .map_err(|_| "File is outside the profile workspace.".to_string())?
        } else {
            path
        };
        let (_, file) = self
            .open_file(relative)
            .map_err(|e| format!("File unavailable in the workspace: {e}"))?;
        let file = self
            .directory
            .canonicalize(file)
            .map_err(|e| format!("Cannot resolve workspace file: {e}"))?;
        file.to_str().ok_or("Workspace file path must be UTF-8.")?;
        Ok(self.path_url(&file))
    }

    /// Rewrite Markdown file references without requiring the file to exist yet.
    pub fn url(&self, source: &str) -> Option<String> {
        if source.starts_with("//") {
            return None;
        }
        if source.starts_with("/files/") {
            return Some(self.route(source));
        }
        // A generated link keeps its addressed profile when pasted elsewhere.
        if source
            .strip_prefix("/profiles/")
            .and_then(|tail| tail.split_once("/files/"))
            .is_some_and(|(profile, _)| crate::core::validate_profile(profile).is_ok())
        {
            return Some(source.into());
        }
        let base = url::Url::from_directory_path(&self.root).ok()?;
        let source = if let Some(tail) = source.strip_prefix("~/") {
            url::Url::from_file_path(dirs::home_dir()?.join(tail)).ok()?
        } else {
            base.join(source).ok()?
        };
        let path = source.to_file_path().ok()?;
        let relative = path.strip_prefix(&self.root).ok()?;
        Some(format!(
            "{}{}",
            self.path_url(relative),
            source
                .fragment()
                .map_or(String::new(), |value| format!("#{value}"))
        ))
    }
}

fn path_url(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        return "/files/".into();
    }
    let mut target = url::Url::parse("https://workspace.invalid/files/").unwrap();
    target
        .path_segments_mut()
        .unwrap()
        .pop_if_empty()
        .extend(path.iter().map(|part| part.to_string_lossy()));
    // Parentheses are legal URL characters but can terminate a Markdown link.
    target.path().replace('(', "%28").replace(')', "%29")
}

fn open_file(directory: &Dir, path: &Path) -> std::io::Result<(std::fs::File, PathBuf)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
    .to_owned();
    let file = directory.open_with(&path, &options)?;
    let (file, path) = if file.metadata()?.is_dir() {
        let index = path.join("index.html");
        (directory.open_with(&index, &options)?, index)
    } else {
        (file, path)
    };
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not a regular file",
        ));
    }
    Ok((file.into_std(), path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    #[test]
    fn file_links_encode_literal_names_and_use_the_markdown_mapping() {
        let dir = temp_dir("file-links");
        let name = "plot #1? 50% 雪).png";
        std::fs::write(dir.path().join(name), b"image").unwrap();
        let files = WorkspaceFiles::open(dir.path())
            .unwrap()
            .with_base_path("/profiles/work".into());
        let url = files.link(Path::new(name)).unwrap();
        assert_eq!(
            url,
            "/profiles/work/files/plot%20%231%3F%2050%25%20%E9%9B%AA%29.png"
        );
        assert_eq!(files.link(&dir.path().join(name)).unwrap(), url);
        let source = url.strip_prefix("/profiles/work/files/").unwrap();
        assert_eq!(files.url(source).unwrap(), url);
        assert_eq!(files.url(&url).unwrap(), url);
        let other = files.with_base_path("/profiles/personal".into());
        assert_eq!(other.url(&url).unwrap(), url);
    }

    #[test]
    fn file_links_require_served_files_and_reject_escaping_symlinks() {
        let root = temp_dir("served-links");
        let outside = temp_dir("unserved-links");
        let files = WorkspaceFiles::open(root.path()).unwrap();
        std::fs::write(outside.path().join("private"), "outside").unwrap();
        std::fs::create_dir(root.path().join("site")).unwrap();
        assert!(files.link(Path::new("missing")).is_err());
        assert!(files.link(Path::new("site")).is_err());
        assert!(files.link(&outside.path().join("private")).is_err());
        assert!(files.link(Path::new("../private")).is_err());
        std::fs::write(root.path().join("site/index.html"), "page").unwrap();
        assert_eq!(
            files.link(Path::new("site")).unwrap(),
            "/files/site/index.html"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path().join("private"), root.path().join("escape"))
                .unwrap();
            std::os::unix::fs::symlink("site/index.html", root.path().join("alias")).unwrap();
            assert!(files.link(Path::new("escape")).is_err());
            assert_eq!(
                files.link(Path::new("alias")).unwrap(),
                "/files/site/index.html"
            );
        }
    }
}
