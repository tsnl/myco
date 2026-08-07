//! Serve the embedded web client — the delivery decision (DESIGN.md L3)
//! made code: one origin, one binary, `/api/*` wins and everything else
//! answers the app shell. Composition happens here in the bin on purpose:
//! `crates/server` stays authentication and translation only.

use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// The client bundle, staged by build.rs (real `trunk build` output, or
/// the honest placeholder when the client was not built).
#[derive(RustEmbed)]
#[folder = "$OUT_DIR/webdist"]
struct WebDist;

/// The fallback handler behind `/api`: exact asset hits serve the asset;
/// anything else serves the shell (client-side routes all land on the one
/// page). COOP/COEP ride every response — the document needs the isolation
/// headers for the wasm-threads path (DP‑1), not just the API.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (path, cache) = match WebDist::get(path) {
        Some(_) if !path.is_empty() => (path, "public, max-age=3600"),
        _ => ("index.html", "no-cache"),
    };
    let Some(asset) = WebDist::get(path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut response = ([(header::CONTENT_TYPE, mime.as_ref())], asset.data).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(value) = HeaderValue::from_str(cache) {
        headers.insert(header::CACHE_CONTROL, value);
    }
    headers.insert(
        header::HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::HeaderName::from_static("cross-origin-embedder-policy"),
        HeaderValue::from_static("require-corp"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_shell_answers_at_slash_with_isolation_headers() {
        let response = serve(Uri::from_static("/")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["cross-origin-opener-policy"],
            "same-origin"
        );
        assert_eq!(
            response.headers()["cross-origin-embedder-policy"],
            "require-corp"
        );
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
    }

    /// Client-side routes are not 404s: the shell answers, and the client
    /// routes once loaded.
    #[tokio::test]
    async fn unknown_paths_answer_the_shell_not_a_404() {
        let response = serve(Uri::from_static("/some/client/route")).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
