use super::files::Files;
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

pub(super) fn image_url(source: &str, files: &Files) -> String {
    if source.starts_with("/api/image?") {
        return source.into();
    }
    if source.starts_with("https://")
        || source.starts_with("http://")
        || [
            "data:image/png;base64,",
            "data:image/jpeg;base64,",
            "data:image/gif;base64,",
            "data:image/webp;base64,",
        ]
        .iter()
        .any(|prefix| source.starts_with(prefix))
    {
        return source.into();
    }
    if let Some(url) = files.url(source) {
        return url;
    }
    if source.contains(':')
        && !source.starts_with("myco-image:sha256:")
        && !source.starts_with("file://")
    {
        return String::new();
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("source", source)
        .finish();
    format!("/api/image?{query}")
}

fn link_url(link: &str, files: &Files) -> String {
    if link.starts_with('#') || link.starts_with("/sessions/") || link.starts_with("/api/") {
        return link.into();
    }
    match url::Url::parse(link) {
        Ok(url) if matches!(url.scheme(), "http" | "https" | "mailto") => link.into(),
        _ => files.url(link).unwrap_or_else(|| "#".into()),
    }
}

pub(super) fn render(text: &str, files: &Files) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let events = Parser::new_ext(text, options).map(|event| match event {
        Event::Html(text) | Event::InlineHtml(text) => Event::Text(text),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: CowStr::from(image_url(&dest_url, files)),
            title,
            id,
        }),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: CowStr::from(link_url(&dest_url, files)),
            title,
            id,
        }),
        event => event,
    });
    let mut output = String::new();
    html::push_html(&mut output, events);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_tables_tasks_and_local_images_without_active_html() {
        let files = Files::open(&std::env::current_dir().unwrap()).unwrap();
        let html = render(
            "# Heading\n\n**bold** and _emphasis_\n\n- [x] done\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n![plot](./plot.png)\n\n<script>alert(1)</script>\n\n[x](javascript:alert)\n",
            &files,
        );
        assert!(html.contains("<h1>Heading</h1>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("type=\"checkbox\""));
        assert!(html.contains("/files/plot.png"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(!render("![x](data:text/html,bad)", &files).contains("src=\"data:"));
    }
}
