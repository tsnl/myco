//! Stage the web client's `dist/` into OUT_DIR for embedding. When the
//! client has not been built (`trunk build` in clients/web), a placeholder
//! page is staged instead, so a fresh clone always compiles and `/`
//! always answers something honest.

use std::path::Path;

fn main() {
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let staged = Path::new(&out).join("webdist");
    let _ = std::fs::remove_dir_all(&staged);
    std::fs::create_dir_all(&staged).expect("create staged webdist");

    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../clients/web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    if dist.join("index.html").exists() {
        copy_dir(&dist, &staged);
    } else {
        std::fs::write(
            staged.join("index.html"),
            "<!doctype html><meta charset=\"utf-8\"><title>myco</title>\
             <body style=\"font-family:system-ui;background:#f2f1f6;color:#1e1c26;\
             display:grid;place-items:center;height:100vh;margin:0\">\
             <div style=\"max-width:26rem;text-align:left\">\
             <p style=\"font-weight:650\">&#9679; myco</p>\
             <p>the web client was not built into this binary.</p>\
             <p style=\"color:#6b6779\">build it: <code>cd clients/web &amp;&amp; trunk build</code>, \
             then rebuild <code>myco</code>. the API is live at <code>/api</code>.</p>\
             </div>",
        )
        .expect("write placeholder");
    }
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read dist") {
        let entry = entry.expect("dir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            std::fs::create_dir_all(&target).expect("create dir");
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy dist file");
        }
    }
}
