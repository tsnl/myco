//! Read-only access relative to the profile workspace captured at worker startup.

use std::path::Path;

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

// Workspace documents may display HTML/SVG and load relative images/CSS.
// Keeping same-origin permits those assets; omitting allow-scripts, forms, and
// navigation prevents documents from exercising the app's API privileges.
const FILE_POLICY: &str = "sandbox allow-same-origin; default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

//
// Workspace URLs and reads
//

pub(super) struct Files {
    pub(super) workspace: myco::core::WorkspaceFiles,
}

impl Files {
    pub(super) fn open(root: &Path) -> Result<Self, String> {
        Ok(Self {
            workspace: myco::core::WorkspaceFiles::open(root)?,
        })
    }

    pub(super) fn with_base_path(mut self, base_path: String) -> Self {
        self.workspace = self.workspace.with_base_path(base_path);
        self
    }

    pub(super) fn route(&self, path: &str) -> String {
        self.workspace.route(path)
    }
    pub(super) fn url(&self, source: &str) -> Option<String> {
        self.workspace.url(source)
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
        let workspace = self.workspace.clone();
        let requested = path.clone();
        let opened =
            tokio::task::spawn_blocking(move || workspace.open_file(Path::new(&path))).await;
        match opened {
            Ok(Ok((_, path)))
                if !requested.is_empty()
                    && !requested.ends_with('/')
                    && path != Path::new(&requested) =>
            {
                Redirect::permanent(&format!(
                    "{}/",
                    self.workspace.path_url(Path::new(&requested))
                ))
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
