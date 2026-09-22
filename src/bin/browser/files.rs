//! Read-only access relative to the directory captured when the server starts.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use cap_std::fs::{Dir, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

// Workspace documents may display HTML/SVG, but never execute with the app's
// credentials. Keeping same-origin permits authenticated relative images/CSS;
// omitting allow-scripts, forms, and navigation prevents active app privileges.
const FILE_POLICY: &str = "sandbox allow-same-origin; default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

//
// Workspace URLs and reads
//

pub(super) struct Files {
    root: PathBuf,
    directory: Arc<Dir>,
}

impl Files {
    pub(super) fn open(root: &Path) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("resolve workspace directory: {e}"))?;
        let directory = Dir::open_ambient_dir(&root, cap_std::ambient_authority())
            .map_err(|e| format!("open workspace directory: {e}"))?;
        Ok(Self {
            root,
            directory: Arc::new(directory),
        })
    }

    pub(super) fn url(&self, source: &str) -> Option<String> {
        if source.starts_with("//") {
            return None;
        }
        if source.starts_with("/files/") {
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
            path_url(relative),
            source
                .fragment()
                .map_or(String::new(), |value| format!("#{value}"))
        ))
    }

    pub(super) async fn serve(
        &self,
        path: String,
        method: Method,
        mut headers: HeaderMap,
    ) -> Response {
        // Range applies only to GET. Without representation validators, an
        // If-Range condition cannot match and must fall back to the full file.
        if method != Method::GET || headers.contains_key(header::IF_RANGE) {
            headers.remove(header::RANGE);
        }
        let directory = self.directory.clone();
        let requested = path.clone();
        let opened = tokio::task::spawn_blocking(move || open_file(&directory, &path)).await;
        match opened {
            Ok(Ok((_, path)))
                if !requested.is_empty()
                    && !requested.ends_with('/')
                    && path != Path::new(&requested) =>
            {
                Redirect::permanent(&format!("{}/", path_url(Path::new(&requested))))
                    .into_response()
            }
            Ok(Ok((file, path))) => stream(file, &path, &headers).await,
            Ok(Err(error)) => {
                let status = match error.kind() {
                    std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput => {
                        StatusCode::NOT_FOUND
                    }
                    _ => {
                        eprintln!("workspace file: {error}");
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                };
                (status, "File unavailable in the workspace.").into_response()
            }
            Err(error) => {
                eprintln!("workspace file task: {error}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Cannot read workspace file.",
                )
                    .into_response()
            }
        }
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
    target.path().into()
}

fn open_file(directory: &Dir, path: &str) -> std::io::Result<(std::fs::File, PathBuf)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let path = PathBuf::from(if path.is_empty() { "." } else { path });
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

//
// Streaming and byte ranges
//

fn byte_range(headers: &HeaderMap, length: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(value) = headers.get(header::RANGE) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| ())?
        .strip_prefix("bytes=")
        .ok_or(())?;
    let (start, end) = value.split_once('-').ok_or(())?;
    if length == 0 {
        return Err(());
    }
    let (start, end) = if start.is_empty() {
        let count: u64 = end.parse().map_err(|_| ())?;
        if count == 0 {
            return Err(());
        }
        (length.saturating_sub(count), length - 1)
    } else {
        (
            start.parse().map_err(|_| ())?,
            if end.is_empty() {
                length - 1
            } else {
                end.parse::<u64>().map_err(|_| ())?.min(length - 1)
            },
        )
    };
    if start > end || start >= length {
        return Err(());
    }
    Ok(Some((start, end)))
}

async fn stream(file: std::fs::File, path: &Path, headers: &HeaderMap) -> Response {
    let mut file = tokio::fs::File::from_std(file);
    let length = match file.metadata().await {
        Ok(meta) => meta.len(),
        Err(error) => {
            eprintln!("workspace file metadata: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let range = match byte_range(headers, length) {
        Ok(range) => range,
        Err(()) => {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{length}"))],
            )
                .into_response();
        }
    };
    let (start, count) = range.map_or((0, length), |(start, end)| (start, end - start + 1));
    if let Err(error) = file.seek(std::io::SeekFrom::Start(start)).await {
        eprintln!("workspace file seek: {error}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let mut response = Body::from_stream(ReaderStream::new(file.take(count))).into_response();
    let headers = response.headers_mut();
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let content_type = if mime.type_() == "text" {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    };
    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    headers.insert(header::CONTENT_LENGTH, count.into());
    headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        FILE_POLICY.parse().unwrap(),
    );
    if let Some((start, end)) = range {
        headers.insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{length}").parse().unwrap(),
        );
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    response
}
