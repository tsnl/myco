use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

pub(super) fn image_url(source: &str) -> String {
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

fn safe_link(link: &str) -> bool {
    match url::Url::parse(link) {
        Ok(url) => matches!(url.scheme(), "http" | "https" | "mailto"),
        Err(_) => !link.trim_start().contains(':') && !link.starts_with("//"),
    }
}

pub(super) fn render(text: &str) -> String {
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
            dest_url: CowStr::from(image_url(&dest_url)),
            title,
            id,
        }),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) if !safe_link(&dest_url) => Event::Start(Tag::Link {
            link_type,
            dest_url: CowStr::from("#"),
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
        let html = render(
            "# Heading\n\n**bold** and _emphasis_\n\n- [x] done\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n![plot](./plot.png)\n\n<script>alert(1)</script>\n\n[x](javascript:alert)\n",
        );
        assert!(html.contains("<h1>Heading</h1>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("type=\"checkbox\""));
        assert!(html.contains("/api/image?source=.%2Fplot.png"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(!render("![x](data:text/html,bad)").contains("src=\"data:"));
    }
}
