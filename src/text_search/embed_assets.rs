//! Runtime resolution of the MiniLM embedding assets.
//!
//! Weights are **not** baked into the binary and **not** tracked in git. They
//! are fetched on first semantic-search use into
//! `~/.myco/models/all-MiniLM-L6-v2/` and verified against the sha256 digests
//! in `embed_weights/MODEL.manifest` (compiled in via `include_str!`, so the
//! manifest is the single source of truth shared with
//! `scripts/seed-minilm-weights.sh`).
//!
//! Env:
//! - `MYCO_EMBED_CACHE` — use this directory instead of `~/.myco/models/…`
//! - `MYCO_EMBED_OFFLINE=1` — never download; fail if the cache is incomplete
//! - `MYCO_EMBED_ENDPOINT` / `HF_ENDPOINT` — mirror base URL (default
//!   `https://huggingface.co`)
//!
//! The cache is deliberately *not* rooted at `MYCO_HOME`: that variable is
//! retargeted at throwaway directories by tests and `--mode host` workers,
//! and a ~87 MiB download must not follow session storage around.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs};

use sha2::{Digest, Sha256};

const MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
const DEFAULT_ENDPOINT: &str = "https://huggingface.co";
/// Directory name under `~/.myco/models/`.
const MODEL_DIR: &str = "all-MiniLM-L6-v2";

const MANIFEST: &str = include_str!("embed_weights/MODEL.manifest");

const MODEL_SAFETENSORS: &str = "model.safetensors";
const TOKENIZER_JSON: &str = "tokenizer.json";
const CONFIG_JSON: &str = "config.json";

const DOWNLOAD_ATTEMPTS: u32 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// The three files Candle needs, as bytes.
pub struct EmbedAssets {
    pub weights: Vec<u8>,
    pub tokenizer: Vec<u8>,
    pub config: Vec<u8>,
}

/// Resolve (downloading if needed) every MiniLM asset and return its contents.
pub fn load() -> Result<EmbedAssets, String> {
    let dir = cache_dir()?;
    let shas = manifest_shas();
    Ok(EmbedAssets {
        weights: asset_bytes(&dir, MODEL_SAFETENSORS, &shas)?,
        tokenizer: asset_bytes(&dir, TOKENIZER_JSON, &shas)?,
        config: asset_bytes(&dir, CONFIG_JSON, &shas)?,
    })
}

/// Where assets live: `$MYCO_EMBED_CACHE` or `~/.myco/models/all-MiniLM-L6-v2`.
pub fn cache_dir() -> Result<PathBuf, String> {
    if let Some(dir) = env::var_os("MYCO_EMBED_CACHE")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().ok_or_else(|| "could not resolve home directory".to_string())?;
    Ok(home.join(".myco").join("models").join(MODEL_DIR))
}

fn offline() -> bool {
    env::var_os("MYCO_EMBED_OFFLINE").is_some_and(|v| v != "0")
}

fn endpoint() -> String {
    for var in ["MYCO_EMBED_ENDPOINT", "HF_ENDPOINT"] {
        if let Ok(v) = env::var(var) {
            let v = v.trim().trim_end_matches('/');
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    DEFAULT_ENDPOINT.to_string()
}

/// Read `name` from the cache, downloading it first when absent or corrupt.
fn asset_bytes(dir: &Path, name: &str, shas: &HashMap<&str, &str>) -> Result<Vec<u8>, String> {
    let want = shas
        .get(name)
        .copied()
        .ok_or_else(|| format!("{name} has no sha256 in MODEL.manifest"))?;
    let path = dir.join(name);

    if let Ok(bytes) = fs::read(&path) {
        if sha256_hex(&bytes) == want {
            return Ok(bytes);
        }
        eprintln!(
            "myco: {} failed its sha256 check — refetching",
            path.display()
        );
    }

    if offline() {
        return Err(format!(
            "MYCO_EMBED_OFFLINE is set and {} is missing or corrupt.\n\
             Seed it with `bash scripts/seed-minilm-weights.sh`, or point \
             MYCO_EMBED_CACHE at a directory holding {MODEL_SAFETENSORS}, \
             {TOKENIZER_JSON} and {CONFIG_JSON}.",
            path.display()
        ));
    }

    let url = format!("{}/{MODEL_ID}/resolve/main/{name}", endpoint());
    eprintln!("myco: fetching embedding asset {name} → {}", path.display());
    let bytes = download(&url)?;

    let got = sha256_hex(&bytes);
    if got != want {
        return Err(format!("{url}: sha256 mismatch (got {got}, want {want})"));
    }
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    // Concurrent myco processes may fetch the same asset; each writes its own
    // temporary and renames over the destination, so readers see whole files.
    let mut file = atomic_write_file::AtomicWriteFile::open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    file.commit()
        .map_err(|e| format!("commit {}: {e}", path.display()))
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        match http_get(url) {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                last = e;
                if attempt < DOWNLOAD_ATTEMPTS {
                    std::thread::sleep(Duration::from_secs(1u64 << attempt));
                }
            }
        }
    }
    Err(format!(
        "{url}: download failed after {DOWNLOAD_ATTEMPTS} attempts: {last}"
    ))
}

/// Blocking GET on a dedicated thread with its own runtime: callers reach this
/// from `spawn_blocking`, and reqwest must not drive a client on a borrowed
/// executor thread (nor can it, from a `current_thread` runtime).
fn http_get(url: &str) -> Result<Vec<u8>, String> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("runtime: {e}"))?;
                rt.block_on(async {
                    let client = reqwest::Client::builder()
                        .connect_timeout(CONNECT_TIMEOUT)
                        .read_timeout(READ_TIMEOUT)
                        .build()
                        .map_err(|e| format!("http client: {e}"))?;
                    let resp = client
                        .get(url)
                        .send()
                        .await
                        .map_err(|e| format!("GET: {e}"))?;
                    let status = resp.status();
                    if !status.is_success() {
                        return Err(format!("HTTP {status}"));
                    }
                    resp.bytes()
                        .await
                        .map(|b| b.to_vec())
                        .map_err(|e| format!("body: {e}"))
                })
            })
            .join()
            .map_err(|_| "download thread panicked".to_string())?
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Parse `<sha256>  <name>` lines out of `MODEL.manifest` (`#` starts a comment).
fn manifest_shas() -> HashMap<&'static str, &'static str> {
    let mut map = HashMap::new();
    for line in MANIFEST.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            map.insert(name.trim_start_matches("./"), hash);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_covers_every_asset() {
        let shas = manifest_shas();
        for name in [MODEL_SAFETENSORS, TOKENIZER_JSON, CONFIG_JSON] {
            assert!(
                shas.contains_key(name),
                "{name} missing from MODEL.manifest"
            );
        }
        assert_eq!(shas.len(), 3, "unexpected manifest entries: {shas:?}");
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn cache_dir_defaults_under_dot_myco() {
        let dir = cache_dir().expect("cache dir");
        assert!(
            dir.ends_with(MODEL_DIR) || env::var_os("MYCO_EMBED_CACHE").is_some(),
            "unexpected cache dir {}",
            dir.display()
        );
    }

    #[test]
    fn write_atomic_creates_parents_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!("myco-embed-{}", std::process::id()));
        let path = dir.join("nested").join("blob.bin");
        write_atomic(&path, b"payload").expect("write");
        assert_eq!(fs::read(&path).expect("read"), b"payload");
        // Overwrite in place is the refetch path.
        write_atomic(&path, b"replaced").expect("rewrite");
        assert_eq!(fs::read(&path).expect("read"), b"replaced");
        let _ = fs::remove_dir_all(&dir);
    }

    /// One-shot loopback server: replies with `status` and `body`, then exits.
    fn serve_once(status: &'static str, body: &'static [u8]) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body);
        });
        format!("http://{addr}")
    }

    #[test]
    fn http_get_returns_body() {
        let base = serve_once("200 OK", b"hello weights");
        assert_eq!(http_get(&base).expect("get"), b"hello weights");
    }

    #[test]
    fn http_get_reports_error_status() {
        let base = serve_once("404 Not Found", b"nope");
        let err = http_get(&base).expect_err("should fail");
        assert!(err.contains("404"), "unexpected error: {err}");
    }
}
