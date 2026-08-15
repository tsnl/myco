//! Driven against a real chromium when one is present; skipped loudly
//! when not. The page under test is a data: URL, so nothing leaves the
//! machine — the whole computer-use loop (read the tree, click a ref,
//! type at the focus, watch the title change) runs against bytes we
//! authored here.

use super::*;
use base64::Engine as _;
use myco_instance::Pool;
use std::sync::Arc;

fn ada() -> Principal {
    Principal::Human("ada".into())
}

fn in_secs(s: u64) -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(s)
}

/// The test's own discovery: the env override, the Playwright layout
/// this repo's dev container ships, then PATH. Product code never
/// hardcodes paths; a test may know its house.
fn test_browser() -> Option<String> {
    if let Ok(browser) = std::env::var("MYCO_BROWSER")
        && !browser.is_empty()
    {
        return Some(browser);
    }
    let playwright = "/opt/pw-browsers/chromium";
    if std::path::Path::new(playwright).is_file() {
        return Some(playwright.into());
    }
    find_browser()
}

#[tokio::test]
async fn the_page_answers_computer_use_end_to_end() {
    let Some(browser) = test_browser() else {
        eprintln!("skipping: no chromium on this machine (set MYCO_BROWSER)");
        return;
    };

    let pool = Pool::new();
    pool.register(Arc::new(BrowserKind));
    let html = r#"<title>probe</title><button onclick="document.title='pressed'">press me</button><input aria-label="the field">"#;
    let url = format!(
        "data:text/html;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(html)
    );
    let page = pool
        .create(
            &ada(),
            "browser",
            "",
            "probe",
            serde_json::json!({
                "browser": browser,
                "args": ["--no-sandbox"],
                "url": url,
            }),
        )
        .expect("the instance creates while the browser is still launching");

    // Launching is state; ready is a watermark away.
    pool.wait_until(&ada(), &page.id, "about", in_secs(60), |about| {
        about["status"] == "ready"
    })
    .await
    .expect("about answers")
    .expect("the browser came up");

    // The tree names what the page shows.
    pool.wait_until(&ada(), &page.id, "text", in_secs(30), |text| {
        text.as_str().is_some_and(|t| t.contains("press me"))
    })
    .await
    .expect("text answers")
    .expect("the page rendered");

    // Read a ref, spend it on a click, watch the page react.
    let tree = pool
        .call(&ada(), &page.id, "a11y", Value::Null)
        .await
        .expect("a11y answers");
    let button = tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "press me")
        .expect("the button is in the tree")["ref"]
        .clone();
    pool.call(&ada(), &page.id, "click", serde_json::json!({ "ref": button }))
        .await
        .expect("clicks");
    pool.wait_until(&ada(), &page.id, "about", in_secs(30), |about| {
        about["title"] == "pressed"
    })
    .await
    .expect("about answers")
    .expect("the click landed — the title changed");

    // Focus the field, type at the focus, read the value back.
    let tree = pool
        .call(&ada(), &page.id, "a11y", Value::Null)
        .await
        .expect("a11y answers");
    let field = tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "the field")
        .expect("the field is in the tree")["ref"]
        .clone();
    pool.call(&ada(), &page.id, "click", serde_json::json!({ "ref": field }))
        .await
        .expect("focuses");
    pool.call(
        &ada(),
        &page.id,
        "type",
        serde_json::json!({ "text": "hi" }),
    )
    .await
    .expect("types");
    pool.wait_until(&ada(), &page.id, "a11y", in_secs(30), |tree| {
        tree["nodes"]
            .as_array()
            .is_some_and(|nodes| nodes.iter().any(|n| n["value"] == "hi"))
    })
    .await
    .expect("a11y answers")
    .expect("the typed text landed in the field");

    // The other projection: pixels, as a real PNG.
    let shot = pool
        .call(&ada(), &page.id, "screenshot", Value::Null)
        .await
        .expect("screenshot answers");
    let png = base64::engine::general_purpose::STANDARD
        .decode(shot["png"].as_str().expect("base64"))
        .expect("decodes");
    assert_eq!(&png[..4], b"\x89PNG", "a real PNG comes back");
}
