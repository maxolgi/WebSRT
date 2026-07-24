//! Certificate generation for WebTransport.
//!
//! Mode A (default, `--cert-mode self`):
//!   wtransport's built-in self-signed ECDSA P-256 cert (≤14-day validity,
//!   regenerated each boot). The SHA-256 of the DER is printed for the browser
//!   to use with `serverCertificateHashes`.
//!
//! Mode B (`--cert-mode mkcert`):
//!   Load PEM cert/key files from disk (e.g. produced by `mkcert`). Browser uses
//!   normal PKI validation.
//!
//! Mode C (`CertSource::PemBytes`):
//!   Load PEM cert/key from in-memory byte slices (e.g. from a secret manager).
//!   Browser uses normal PKI validation.

use anyhow::{Context, Result};
use std::path::PathBuf;
use wtransport::Identity;

pub enum CertSource {
    /// Generated self-signed ECDSA P-256 (≤14-day validity).
    SelfSigned { sans: Vec<String> },
    /// Loaded from PEM files on disk.
    Mkcert { cert: PathBuf, key: PathBuf },
    /// Loaded from PEM-encoded byte slices (e.g. from a secret manager).
    PemBytes { cert: Vec<u8>, key: Vec<u8> },
}

/// A ready-to-use WebTransport identity plus the DER SHA-256 hash for the
/// browser (only meaningful for `SelfSigned`).
pub struct Cert {
    pub identity: Identity,
    pub der_sha256: Option<[u8; 32]>,
}

impl Cert {
    pub async fn build(src: CertSource) -> Result<Self> {
        match src {
            CertSource::SelfSigned { sans } => Self::build_self_signed(sans),
            CertSource::Mkcert { cert, key } => Self::build_mkcert(cert, key).await,
            CertSource::PemBytes { cert, key } => Self::build_pem_bytes(cert, key).await,
        }
    }

    fn build_self_signed(sans: Vec<String>) -> Result<Self> {
        let identity = Identity::self_signed(&sans).context("self-signed cert")?;
        // First (leaf) cert DER → SHA-256.
        let chain = identity.certificate_chain();
        let leaf = chain
            .as_slice()
            .first()
            .context("empty certificate chain")?;
        let hash = leaf.hash();
        let der_sha256 = *hash.as_ref();
        tracing::info!(
            sans = ?sans,
            "self-signed cert generated (valid ≤14 days; regenerate via restart)"
        );
        Ok(Self {
            identity,
            der_sha256: Some(der_sha256),
        })
    }

    async fn build_mkcert(cert_path: PathBuf, key_path: PathBuf) -> Result<Self> {
        let identity = Identity::load_pemfiles(cert_path, key_path)
            .await
            .context("load mkcert pem files")?;
        Ok(Self {
            identity,
            der_sha256: None,
        })
    }

    async fn build_pem_bytes(cert: Vec<u8>, key: Vec<u8>) -> Result<Self> {
        // wtransport 0.7.1 exposes no in-memory PEM loader: `Identity::load_pemfiles`
        // accepts only filesystem paths. Round-trip the caller's PEM bytes through
        // restrictively-permissioned temp files and let wtransport parse them, so
        // every PEM key format (PKCS#8 / PKCS#1 / SEC1) is supported without adding
        // a new dependency. Temp files are removed on every path (success or error).
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let token = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let cert_path = dir.join(format!("websrt-pembytes-{}-{token}.crt", std::process::id()));
        let key_path = dir.join(format!("websrt-pembytes-{}-{token}.key", std::process::id()));

        let loaded = async {
            Self::write_secret_file(&cert_path, &cert)
                .await
                .context("write temp cert pem")?;
            Self::write_secret_file(&key_path, &key)
                .await
                .context("write temp key pem")?;
            Identity::load_pemfiles(&cert_path, &key_path)
                .await
                .context("load pem bytes")
        }
        .await;

        // Best-effort cleanup regardless of outcome.
        let _ = tokio::fs::remove_file(&cert_path).await;
        let _ = tokio::fs::remove_file(&key_path).await;

        Ok(Self {
            identity: loaded?,
            der_sha256: None,
        })
    }

    /// Writes `data` to `path` with `0600` permissions on Unix (private).
    #[cfg(unix)]
    async fn write_secret_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .await?;
        file.write_all(data).await
    }

    #[cfg(not(unix))]
    async fn write_secret_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
        tokio::fs::write(path, data).await
    }
}
