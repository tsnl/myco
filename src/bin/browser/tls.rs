//! TLS configuration and persistent, private local certificate material.

use std::io::Write;
use std::path::{Path, PathBuf};

use axum_server::tls_rustls::RustlsConfig;

use super::super::Args;

//
// Listener configuration
//

pub(super) async fn configure(args: &Args, hostname: &str) -> Result<Option<RustlsConfig>, String> {
    if args.insecure_http {
        if !args.bind.is_loopback() {
            return Err(
                "--insecure-http requires a loopback --bind address; use HTTPS for remote access"
                    .into(),
            );
        }
        return Ok(None);
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = if let (Some(cert), Some(key)) = (&args.tls_cert, &args.tls_key) {
        RustlsConfig::from_pem_file(cert, key).await
    } else {
        let names = certificate_names(args, hostname);
        let directory = myco::core::myco_home()?.join("tls");
        let (identity, certificate) =
            tokio::task::spawn_blocking(move || local_identity(&directory, names))
                .await
                .map_err(|e| format!("prepare local TLS identity: {e}"))??;
        eprintln!(
            "Local HTTPS certificate: {}\nTrust this certificate on your browser's device, or provide --tls-cert and --tls-key.",
            certificate.display()
        );
        RustlsConfig::from_pem(identity.clone(), identity).await
    };
    config
        .map(Some)
        .map_err(|e| format!("load HTTPS certificate and private key: {e}"))
}

fn certificate_names(args: &Args, hostname: &str) -> Vec<String> {
    let mut names = vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
        hostname.into(),
    ];
    if !args.bind.is_unspecified() {
        names.push(args.bind.to_string());
    }
    names.extend(args.tls_name.iter().cloned());
    names.sort();
    names.dedup();
    names
}

//
// Persistent local identities
//

fn local_identity(directory: &Path, names: Vec<String>) -> Result<(Vec<u8>, PathBuf), String> {
    let hash = ring::digest::digest(&ring::digest::SHA256, names.join("\n").as_bytes());
    let id: String = hash.as_ref()[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let identity = directory.join(format!("{id}.pem"));
    let certificate = directory.join(format!("{id}.crt"));
    private_directory(directory)?;
    let _lock = identity_lock(&identity.with_extension("lock"))?;
    let bytes = read_or_generate(&identity, names)?;
    // Recover a missing public copy without replacing the private identity.
    myco::core::atomically_write(&certificate, public_certificate(&bytes)?.as_bytes())?;
    Ok((bytes, certificate))
}

fn private_directory(directory: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(directory)
        .map_err(|e| format!("create TLS directory: {e}"))
}

fn identity_lock(path: &Path) -> Result<std::fs::File, String> {
    // Server ports sharing a profile must agree on the identity and public
    // trust certificate, even when they start concurrently for the first time.
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options
        .open(path)
        .map_err(|e| format!("open TLS identity lock: {e}"))?;
    lock.lock().map_err(|e| format!("lock TLS identity: {e}"))?;
    Ok(lock)
}

fn read_or_generate(identity: &Path, names: Vec<String>) -> Result<Vec<u8>, String> {
    match std::fs::read(identity) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let generated = rcgen::generate_simple_self_signed(names)
                .map_err(|e| format!("generate local TLS certificate: {e}"))?;
            let pem = generated.cert.pem();
            let bytes = format!("{pem}{}", generated.signing_key.serialize_pem()).into_bytes();
            write_private(identity, &bytes)?;
            Ok(bytes)
        }
        Err(error) => Err(format!("read local TLS identity: {error}")),
    }
}

fn public_certificate(bytes: &[u8]) -> Result<String, String> {
    let pem = std::str::from_utf8(bytes).map_err(|e| format!("read TLS PEM: {e}"))?;
    let (_, rest) = pem
        .split_once("-----BEGIN CERTIFICATE-----")
        .ok_or("Local TLS identity has no certificate")?;
    let (body, _) = rest
        .split_once("-----END CERTIFICATE-----")
        .ok_or("Local TLS certificate is incomplete")?;
    Ok(format!(
        "-----BEGIN CERTIFICATE-----{body}-----END CERTIFICATE-----\n"
    ))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = atomic_write_file::AtomicWriteFile::open(path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.commit()
        .map_err(|e| format!("save local TLS identity: {e}"))
}
