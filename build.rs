//! Fetch MiniLM embedding assets for `include_bytes!`. Candle loads safetensors
//! (no ONNX Runtime).
//!
//! Assets are staged under `OUT_DIR/embed_weights/` (never the package source
//! tree) so `cargo publish` verify stays valid and crates.io packages stay
//! small. A generated `OUT_DIR/embed_assets.rs` exposes the byte slices.
//!
//! Env:
//! - `MYCO_EMBED_OFFLINE=1` — never download; fail if any asset missing
//! - `MYCO_EMBED_BASE_URL` — override HF base
//! - `MYCO_EMBED_CACHE` — optional seed dir (also checks gitignored
//!   `src/text_search/embed_weights/` as a convenience cache)

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SRC_WEIGHTS_DIR: &str = "src/text_search/embed_weights";
const DEFAULT_BASE: &str =
    "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main";

struct Asset {
    local_name: &'static str,
    remote_path: &'static str,
    size: u64,
    rust_const: &'static str,
}

const ASSETS: &[Asset] = &[
    Asset {
        local_name: "model.safetensors",
        remote_path: "model.safetensors",
        size: 90868376,
        rust_const: "MODEL_WEIGHTS",
    },
    Asset {
        local_name: "tokenizer.json",
        remote_path: "tokenizer.json",
        size: 466247,
        rust_const: "TOKENIZER_JSON",
    },
    Asset {
        local_name: "config.json",
        remote_path: "config.json",
        size: 612,
        rust_const: "CONFIG_JSON",
    },
];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let src_weights = manifest_dir.join(SRC_WEIGHTS_DIR);
    let out_weights = out_dir.join("embed_weights");
    let manifest_path = src_weights.join("MODEL.manifest");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    for a in ASSETS {
        println!(
            "cargo:rerun-if-changed={}",
            src_weights.join(a.local_name).display()
        );
    }
    println!("cargo:rerun-if-env-changed=MYCO_EMBED_CACHE");
    println!("cargo:rerun-if-env-changed=MYCO_EMBED_OFFLINE");
    println!("cargo:rerun-if-env-changed=MYCO_EMBED_BASE_URL");

    fs::create_dir_all(&out_weights).unwrap_or_else(|e| {
        panic!("create {}: {e}", out_weights.display());
    });

    let shas = read_manifest_shas(&manifest_path);
    let base = env::var("MYCO_EMBED_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE.to_string());
    let offline = env::var_os("MYCO_EMBED_OFFLINE").is_some_and(|v| v != "0");
    let mut seed_dirs: Vec<PathBuf> = Vec::new();
    if let Some(cache) = env::var_os("MYCO_EMBED_CACHE").map(PathBuf::from) {
        seed_dirs.push(cache);
    }
    // Convenience: reuse a developer-local gitignored tree when present.
    if src_weights.is_dir() {
        seed_dirs.push(src_weights.clone());
    }

    for asset in ASSETS {
        let dest = out_weights.join(asset.local_name);
        let want_sha = shas.get(asset.local_name).map(String::as_str);
        if asset_ok(&dest, asset.size, want_sha) {
            continue;
        }

        let mut seeded = false;
        for cache_root in &seed_dirs {
            let candidates = [
                cache_root.join(asset.local_name),
                cache_root.join(asset.remote_path),
            ];
            if let Some(src) = candidates.into_iter().find(|p| p.is_file())
                && asset_ok(&src, asset.size, want_sha)
            {
                fs::copy(&src, &dest).unwrap_or_else(|e| {
                    panic!("copy {} → {}: {e}", src.display(), dest.display());
                });
                println!(
                    "cargo:warning=MiniLM asset {} seeded from {}",
                    asset.local_name,
                    src.display()
                );
                seeded = true;
                break;
            }
        }
        if seeded {
            continue;
        }

        if offline {
            panic!(
                "MYCO_EMBED_OFFLINE set but MiniLM asset missing/invalid: {}.\n\
                 Seed OUT_DIR via MYCO_EMBED_CACHE or pre-populate \
                 {src}/, or allow network + curl. See {src}/README.md",
                dest.display(),
                src = src_weights.display()
            );
        }

        if dest.is_file() {
            let _ = fs::remove_file(&dest);
        }
        let url = format!("{}/{}", base.trim_end_matches('/'), asset.remote_path);
        download_curl(&url, &dest);
        if !asset_ok(&dest, asset.size, want_sha) {
            let len = fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            if want_sha.is_none() && len > 0 && asset.local_name != "model.safetensors" {
                println!(
                    "cargo:warning=MiniLM {} size {len} (expected {}); accepting",
                    asset.local_name, asset.size
                );
                continue;
            }
            let _ = fs::remove_file(&dest);
            panic!(
                "MiniLM asset {} failed integrity after download (size={len}, expected {}) from {url}",
                asset.local_name, asset.size
            );
        }
        println!(
            "cargo:warning=downloaded MiniLM asset {} ({} bytes)",
            asset.local_name, asset.size
        );
    }

    write_embed_assets_rs(&out_dir, &out_weights);

    println!(
        "cargo:warning=MiniLM candle embed assets ready under {}",
        out_weights.display()
    );
}

fn write_embed_assets_rs(out_dir: &Path, weights: &Path) {
    let assets_rs = out_dir.join("embed_assets.rs");
    let mut body = String::from(
        "// @generated by build.rs — MiniLM assets staged under OUT_DIR/embed_weights\n",
    );
    for asset in ASSETS {
        let path = weights.join(asset.local_name);
        // Absolute path so include_bytes! works from any including module.
        let path_lit = path
            .to_str()
            .unwrap_or_else(|| panic!("non-utf8 path {}", path.display()))
            .replace('\\', "/");
        body.push_str(&format!(
            "pub static {}: &[u8] = include_bytes!(r#\"{}\"#);\n",
            asset.rust_const, path_lit
        ));
    }
    fs::write(&assets_rs, body).unwrap_or_else(|e| panic!("write {}: {e}", assets_rs.display()));
}

fn asset_ok(path: &Path, size: u64, expected_sha: Option<&str>) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if meta.len() != size {
        return false;
    }
    if let Some(want) = expected_sha {
        match sha256_file(path) {
            Ok(got) if got == want => true,
            Ok(got) => {
                eprintln!(
                    "cargo:warning=sha256 mismatch for {} (got {got}, want {want})",
                    path.display()
                );
                false
            }
            Err(e) => {
                eprintln!("cargo:warning=sha256 failed for {}: {e}", path.display());
                false
            }
        }
    } else {
        true
    }
}

fn read_manifest_shas(path: &Path) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return map;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else { continue };
        let Some(name) = parts.next() else { continue };
        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            map.insert(
                name.trim_start_matches("./").to_string(),
                hash.to_ascii_lowercase(),
            );
        }
    }
    map
}

fn download_curl(url: &str, dest: &Path) {
    let partial = dest.with_extension(format!(
        "{}.partial",
        dest.extension().and_then(|e| e.to_str()).unwrap_or("bin")
    ));
    let _ = fs::remove_file(&partial);
    eprintln!("cargo:warning=downloading {url} → {}", dest.display());
    let status = Command::new("curl")
        .args([
            "-fL",
            "--retry",
            "3",
            "--retry-delay",
            "2",
            "--connect-timeout",
            "30",
            "-o",
        ])
        .arg(&partial)
        .arg(url)
        .status();
    match status {
        Ok(s) if s.success() => {
            fs::rename(&partial, dest).unwrap_or_else(|e| panic!("rename partial: {e}"));
        }
        Ok(s) => {
            let _ = fs::remove_file(&partial);
            panic!("curl failed (exit {s}) downloading {url}");
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            panic!("curl not runnable ({e})");
        }
    }
}

fn sha256_file(path: &Path) -> Result<String, String> {
    for (bin, args_prefix) in [("shasum", &["-a", "256"][..]), ("sha256sum", &[][..])] {
        let mut cmd = Command::new(bin);
        for a in args_prefix {
            cmd.arg(a);
        }
        cmd.arg(path);
        if let Ok(out) = cmd.output()
            && out.status.success()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            let hash = s
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if hash.len() == 64 {
                return Ok(hash);
            }
        }
    }
    Err("no shasum/sha256sum on PATH".into())
}
