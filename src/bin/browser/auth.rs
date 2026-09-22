//! One credential protects UI assets, files, event streams, and API requests.

use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub(super) struct Auth {
    pub(super) token: String,
    pub(super) origin: String,
    pub(super) cookie: String,
    port: u16,
    launch_path: String,
}

impl Auth {
    pub(super) fn new(origin: String, port: u16, launch_path: String) -> Self {
        let prefix = if origin.starts_with("https://") {
            "__Host-"
        } else {
            ""
        };
        Self {
            token: uuid::Uuid::new_v4().as_simple().to_string(),
            cookie: format!("{prefix}myco_{port}"),
            origin,
            port,
            launch_path,
        }
    }

    fn secure(&self) -> bool {
        self.origin.starts_with("https://")
    }
}

//
// Credential and origin checks
//

fn addressed_origin(headers: &HeaderMap, app: &Auth) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let scheme = if app.secure() { "https" } else { "http" };
    let origin = url::Url::parse(&format!("{scheme}://{host}")).ok()?;
    (origin.port_or_known_default()? == app.port
        && origin.username().is_empty()
        && origin.password().is_none()
        && origin.path() == "/"
        && origin.query().is_none()
        && origin.fragment().is_none())
    .then(|| origin.origin().ascii_serialization())
}

fn matches_token(candidate: &str, token: &str) -> bool {
    bool::from(candidate.as_bytes().ct_eq(token.as_bytes()))
}

// HTTP/2 carries :authority in the URI, rather than a Host header. Refuse
// contradictory authorities before applying the same origin policy to either.
fn normalize_host(headers: &mut HeaderMap, uri: &Uri) -> bool {
    if let Some(authority) = uri.authority() {
        if headers
            .get(header::HOST)
            .is_some_and(|host| host.as_bytes() != authority.as_str().as_bytes())
        {
            return false;
        }
        headers.insert(header::HOST, authority.as_str().parse().unwrap());
    }
    true
}

pub(super) fn allowed(headers: &HeaderMap, app: &Auth, mutation: bool) -> bool {
    let Some(origin) = addressed_origin(headers, app) else {
        return false;
    };
    let supplied_origin = headers.get(header::ORIGIN);
    if supplied_origin.is_some_and(|value| value.to_str().ok() != Some(&origin)) {
        return false;
    }
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        let mut words = value.to_str().unwrap_or("").split_whitespace();
        return words
            .next()
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("Bearer"))
            && words
                .next()
                .is_some_and(|token| matches_token(token, &app.token))
            && words.next().is_none();
    }
    (!mutation || supplied_origin.is_some())
        && headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|cookies| cookies.split(';'))
            .any(|cookie| {
                cookie.trim().split_once('=').is_some_and(|(name, token)| {
                    name == app.cookie && matches_token(token, &app.token)
                })
            })
}

pub(super) async fn authorize(
    State(app): State<Arc<Auth>>,
    mut request: Request,
    next: Next,
) -> Response {
    let mutation = !matches!(*request.method(), Method::GET | Method::HEAD);
    let uri = request.uri().clone();
    if !normalize_host(request.headers_mut(), &uri) || !allowed(request.headers(), &app, mutation) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"myco\"")],
            "Open the launch URL or supply Authorization: Bearer <launch-token>.",
        )
            .into_response();
    }
    next.run(request).await
}

//
// Browser login
//

#[derive(Deserialize)]
pub(super) struct Login {
    token: String,
}

pub(super) async fn login(
    State(app): State<Arc<Auth>>,
    mut headers: HeaderMap,
    uri: Uri,
    Query(query): Query<Login>,
) -> Result<Response, (StatusCode, &'static str)> {
    if !normalize_host(&mut headers, &uri)
        || addressed_origin(&headers, &app).is_none()
        || !matches_token(&query.token, &app.token)
    {
        return Err((StatusCode::UNAUTHORIZED, "Invalid launch URL."));
    }
    let mut response = Redirect::to(&app.launch_path).into_response();
    let secure = if app.secure() { "; Secure" } else { "" };
    response.headers_mut().insert(
        header::SET_COOKIE,
        format!(
            "{}={}; HttpOnly; SameSite=Strict; Path=/{secure}",
            app.cookie, app.token
        )
        .parse()
        .unwrap(),
    );
    Ok(response)
}
