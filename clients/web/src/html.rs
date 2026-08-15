//! The render edge's one trust boundary: text becomes inert HTML.

pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_cannot_close_an_attribute() {
        assert_eq!(
            escape(r#"lab" autofocus onfocus="steal()'&<>"#),
            "lab&quot; autofocus onfocus=&quot;steal()&#39;&amp;&lt;&gt;"
        );
    }
}
