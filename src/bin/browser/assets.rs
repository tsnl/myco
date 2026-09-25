//! Embedded browser resources. Origin checks and response policy belong to HTTP.

use axum::{Router, body::Bytes, http::header, routing::get};

const ASSETS: &[(&str, &str)] = &[
    ("/", include_str!("assets/home.html")),
    ("/new", include_str!("assets/new.html")),
    ("/new.js", include_str!("assets/new.js")),
    ("/sessions/{id}", include_str!("assets/index.html")),
    ("/app.js", include_str!("assets/app.js")),
    ("/activity.js", include_str!("assets/activity.js")),
    ("/attachments.js", include_str!("assets/attachments.js")),
    ("/composer.js", include_str!("assets/composer.js")),
    (
        "/message-navigation.js",
        include_str!("assets/message-navigation.js"),
    ),
    ("/links.js", include_str!("assets/links.js")),
    (
        "/markdown-content.js",
        include_str!("assets/markdown-content.js"),
    ),
    ("/timestamps.js", include_str!("assets/timestamps.js")),
    ("/usage.js", include_str!("assets/usage.js")),
    ("/home.js", include_str!("assets/home.js")),
    ("/rename.js", include_str!("assets/rename.js")),
    ("/image-viewer.js", include_str!("assets/image-viewer.js")),
    ("/common.js", include_str!("assets/common.js")),
    ("/scope.js", include_str!("assets/scope.js")),
    ("/profiles.js", include_str!("assets/profiles.js")),
    ("/events.js", include_str!("assets/events.js")),
    ("/style.css", include_str!("assets/style.css")),
    ("/settings.js", include_str!("assets/settings.js")),
    ("/sky.js", include_str!("assets/sky.js")),
    ("/aircraft.js", include_str!("assets/aircraft.js")),
    ("/rain.js", include_str!("assets/rain.js")),
    ("/horizon.js", include_str!("assets/horizon.js")),
    ("/clouds.js", include_str!("assets/clouds.js")),
    (
        "/cloud-renderer.js",
        include_str!("assets/cloud-renderer.js"),
    ),
    ("/sky-settings.js", include_str!("assets/sky-settings.js")),
    ("/sky.css", include_str!("assets/sky.css")),
    ("/sky-weather.js", include_str!("assets/sky-weather.js")),
    ("/sky-noise.js", include_str!("assets/sky-noise.js")),
    ("/sky-light.js", include_str!("assets/sky-light.js")),
    (
        "/sky-atmosphere.js",
        include_str!("assets/sky-atmosphere.js"),
    ),
    ("/cloud-field.js", include_str!("assets/cloud-field.js")),
    (
        "/cloud-textures.js",
        include_str!("assets/cloud-textures.js"),
    ),
];

pub(super) fn routes<S: Clone + Send + Sync + 'static>(base: &str) -> Router<S> {
    let mut router = icons();
    for &(path, body) in ASSETS {
        let content_type = match path.rsplit_once('.') {
            Some((_, "js")) => "text/javascript; charset=utf-8",
            Some((_, "css")) => "text/css; charset=utf-8",
            _ => "text/html; charset=utf-8",
        };
        let body = if content_type.starts_with("text/html") {
            Bytes::from(
                body.replace("href=\"/", &format!("href=\"{base}/"))
                    .replace("src=\"/", &format!("src=\"{base}/"))
                    .replace(
                        "__MYCO_PROFILE__",
                        base.rsplit('/')
                            .next()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("default"),
                    ),
            )
        } else {
            Bytes::from_static(body.as_bytes())
        };
        router = router.route(
            path,
            get(move || {
                let body = body.clone();
                async move { ([(header::CONTENT_TYPE, content_type)], body) }
            }),
        );
    }
    router
}

// Both the supervisor and each profile serve icons without loading a session.
pub(super) fn icons<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(
            "/favicon.svg",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/svg+xml")],
                    include_str!("assets/favicon.svg"),
                )
            }),
        )
        .route(
            "/favicon.ico",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/vnd.microsoft.icon")],
                    include_bytes!("assets/favicon.ico").as_slice(),
                )
            }),
        )
}
