//! Loopback HTTP access delegates remote access control to SSH forwarding.
//! Host and browser-origin checks keep other websites out of the local API.

use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

fn addressed_origin(headers: &HeaderMap) -> Option<url::Url> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let origin = url::Url::parse(&format!("http://{host}")).ok()?;
    let loopback = match origin.host()? {
        url::Host::Domain(name) => name == "localhost",
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    };
    (loopback
        && origin.username().is_empty()
        && origin.password().is_none()
        && origin.path() == "/"
        && origin.query().is_none()
        && origin.fragment().is_none())
    .then_some(origin)
}

// Absolute-form requests also carry an authority in the URI. Refuse a
// contradictory Host header before applying the origin policy.
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

pub(super) fn allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = addressed_origin(headers) else {
        return false;
    };
    if headers
        .get(header::ORIGIN)
        .is_some_and(|value| value.to_str().ok() != Some(&origin.origin().ascii_serialization()))
    {
        return false;
    }
    // Native clients omit Fetch Metadata. Browsers must come from this exact
    // origin or a direct navigation, including when SSH changes the local port.
    headers
        .get("sec-fetch-site")
        .is_none_or(|value| matches!(value.to_str(), Ok("same-origin" | "none")))
}

pub(super) async fn guard(mut request: Request, next: Next) -> Response {
    let uri = request.uri().clone();
    if !normalize_host(request.headers_mut(), &uri) || !allowed(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "Use a loopback address from the same origin.",
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_clients_and_forwarded_browser_ports_need_no_credentials() {
        for host in ["localhost:9876", "127.0.0.1:9876", "[::1]:9876"] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            assert!(allowed(&headers));
            headers.insert(header::ORIGIN, format!("http://{host}").parse().unwrap());
            headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
            assert!(allowed(&headers));
            headers.insert(header::ORIGIN, "http://127.0.0.1:8765".parse().unwrap());
            assert!(!allowed(&headers));
        }
    }

    #[test]
    fn websites_cannot_use_credentials_or_fetch_metadata_to_bypass_origin_checks() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:8765".parse().unwrap());
        headers.insert(header::AUTHORIZATION, "Bearer old-token".parse().unwrap());
        headers.insert(header::COOKIE, "myco_8765=old-token".parse().unwrap());
        for origin in ["https://other.example", "http://localhost:8766", "null"] {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            assert!(!allowed(&headers));
        }
        headers.remove(header::ORIGIN);
        for site in ["cross-site", "same-site", "invalid"] {
            headers.insert("sec-fetch-site", site.parse().unwrap());
            assert!(!allowed(&headers));
        }
        headers.insert("sec-fetch-site", "none".parse().unwrap());
        assert!(allowed(&headers));
    }

    #[test]
    fn only_loopback_authorities_are_accepted() {
        let mut headers = HeaderMap::new();
        assert!(!allowed(&headers));
        for host in [
            "other.example:8765",
            "localhost.evil:8765",
            "192.168.1.10:8765",
            "[2001:db8::1]:8765",
            "0.0.0.0:8765",
            "[::]:8765",
            "[::ffff:127.0.0.1]:8765",
            "user@localhost:8765",
            "localhost:8765/path",
            "localhost:8765?x=y",
        ] {
            headers.insert(header::HOST, host.parse().unwrap());
            assert!(!allowed(&headers), "{host}");
        }
    }

    #[test]
    fn absolute_requests_cannot_contradict_the_host_header() {
        let uri: Uri = "http://localhost:8765/api/sessions".parse().unwrap();
        let mut headers = HeaderMap::new();
        assert!(normalize_host(&mut headers, &uri));
        assert!(allowed(&headers));
        headers.insert(header::HOST, "other.example:8765".parse().unwrap());
        assert!(!normalize_host(&mut headers, &uri));
    }
}
