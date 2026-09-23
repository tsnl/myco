//! Embedded browser resources. Origin checks and response policy belong to HTTP.

use axum::{Router, http::header, routing::get};

const ASSETS: &[(&str, &str)] = &[
    ("/", include_str!("assets/home.html")),
    ("/new", include_str!("assets/new.html")),
    ("/new.js", include_str!("assets/new.js")),
    ("/sessions/{id}", include_str!("assets/index.html")),
    ("/app.js", include_str!("assets/app.js")),
    ("/activity.js", include_str!("assets/activity.js")),
    ("/attachments.js", include_str!("assets/attachments.js")),
    ("/links.js", include_str!("assets/links.js")),
    ("/timestamps.js", include_str!("assets/timestamps.js")),
    ("/home.js", include_str!("assets/home.js")),
    ("/common.js", include_str!("assets/common.js")),
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

pub(super) fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    let mut router = Router::new();
    for &(path, body) in ASSETS {
        let content_type = match path.rsplit_once('.') {
            Some((_, "js")) => "text/javascript; charset=utf-8",
            Some((_, "css")) => "text/css; charset=utf-8",
            _ => "text/html; charset=utf-8",
        };
        router = router.route(
            path,
            get(move || async move { ([(header::CONTENT_TYPE, content_type)], body) }),
        );
    }
    router
}
