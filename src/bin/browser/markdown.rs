use super::files::Files;
use pulldown_cmark::{Alignment, CowStr, Event, Options, Parser, Tag, TagEnd, html};

pub(super) fn image_url(source: &str, files: &Files) -> String {
    if source.starts_with("/api/image?") {
        return files.route(source);
    }
    if source.starts_with(&files.route("/api/image?")) {
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
    files.route(&format!("/api/image?{query}"))
}

fn link_url(link: &str, files: &Files) -> String {
    if link.starts_with('#') || link.starts_with("/profiles/") {
        return link.into();
    }
    if link.starts_with("/sessions/") || link.starts_with("/api/") {
        return files.route(link);
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
    let mut tables = Tables::default();
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
    html::push_html(&mut output, events.map(|event| tables.render(event)));
    output
}

//
// Tables
//

// The default renderer uses inline alignment styles, which our CSP blocks.
// Only parser table events produce trusted markup; user HTML remains escaped.
#[derive(Default)]
struct Tables {
    alignments: Vec<Alignment>,
    column: usize,
    header: bool,
}

impl Tables {
    fn render<'a>(&mut self, event: Event<'a>) -> Event<'a> {
        let markup: CowStr<'a> = match event {
            Event::Start(Tag::Table(alignments)) => {
                self.alignments = alignments;
                "<div class=\"table-scroll\" role=\"region\" aria-label=\"Markdown table\" tabindex=\"0\"><table>".into()
            }
            Event::Start(Tag::TableHead) => {
                self.header = true;
                self.column = 0;
                "<thead><tr>".into()
            }
            Event::End(TagEnd::TableHead) => {
                self.header = false;
                "</tr></thead><tbody>".into()
            }
            Event::Start(Tag::TableRow) => {
                self.column = 0;
                "<tr>".into()
            }
            Event::End(TagEnd::TableRow) => "</tr>".into(),
            Event::Start(Tag::TableCell) => self.cell().into(),
            Event::End(TagEnd::TableCell) => if self.header {
                "</div></th>"
            } else {
                "</div></td>"
            }
            .into(),
            Event::End(TagEnd::Table) => "</tbody></table></div>\n".into(),
            event => return event,
        };
        Event::Html(markup)
    }

    fn cell(&mut self) -> String {
        let alignment = match self.alignments.get(self.column) {
            Some(Alignment::Center) => "center",
            Some(Alignment::Right) => "right",
            _ => "left",
        };
        self.column += 1;
        let tag = if self.header {
            "th scope=\"col\""
        } else {
            "td"
        };
        format!("<{tag} class=\"align-{alignment}\"><div class=\"table-cell\">")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_links_and_images_preserve_their_profile_prefix() {
        let files = Files::open(&std::env::current_dir().unwrap())
            .unwrap()
            .with_base_path("/profiles/research".into());
        for source in [
            "./plot.png",
            "/files/plot.png",
            "/profiles/research/files/plot.png",
        ] {
            assert_eq!(
                image_url(source, &files),
                "/profiles/research/files/plot.png"
            );
        }
        let stored = image_url("myco-image:sha256:123", &files);
        assert_eq!(
            stored,
            "/profiles/research/api/image?source=myco-image%3Asha256%3A123"
        );
        assert_eq!(image_url(&stored, &files), stored);
        assert_eq!(
            image_url("/api/image?source=123", &files),
            "/profiles/research/api/image?source=123"
        );
        let html = render(
            "[session](/sessions/123) [api](/api/models) [file](/files/notes.txt) [other](/profiles/work/)",
            &files,
        );
        for target in ["sessions/123", "api/models", "files/notes.txt"] {
            assert!(html.contains(&format!("href=\"/profiles/research/{target}\"")));
        }
        assert!(html.contains("href=\"/profiles/work/\""));
    }

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

    #[test]
    fn tables_preserve_alignment_and_inline_content_without_allowing_html_or_inline_styles() {
        let files = Files::open(&std::env::current_dir().unwrap()).unwrap();
        let html = render(
            "| Name | State | Count | Default |\n| :--- | :---: | ---: | --- |\n| **Tools** | Ready | 12 | `<table>` |\n| Browser | Pass | 7 | [docs](https://example.com) |\n\n> | Next |\n> | --- |\n> | Plain |\n\n<table><tr><td>untrusted</td></tr></table>",
            &files,
        );
        assert_eq!(html.matches("class=\"table-scroll\"").count(), 2);
        assert!(html.contains(
            "<th scope=\"col\" class=\"align-center\"><div class=\"table-cell\">State</div></th>"
        ));
        assert!(html.contains(
            "<th scope=\"col\" class=\"align-right\"><div class=\"table-cell\">Count</div></th>"
        ));
        assert!(html.contains(
            "<td class=\"align-left\"><div class=\"table-cell\"><strong>Tools</strong></div></td>"
        ));
        assert!(
            html.contains("<td class=\"align-center\"><div class=\"table-cell\">Pass</div></td>")
        );
        assert!(html.contains("<td class=\"align-right\"><div class=\"table-cell\">7</div></td>"));
        assert!(
            html.contains("<td class=\"align-left\"><div class=\"table-cell\">Plain</div></td>")
        );
        assert!(html.contains("<code>&lt;table&gt;</code>"));
        assert!(html.contains("<a href=\"https://example.com\">docs</a>"));
        assert!(html.contains("&lt;table&gt;&lt;tr&gt;&lt;td&gt;untrusted"));
        assert!(!html.contains("style="));
        assert_eq!(html.matches("<table>").count(), 2);
        assert_eq!(html.matches("</table></div>").count(), 2);
    }
}
