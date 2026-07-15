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

use anyhow::{Context, Result};
use std::path::PathBuf;
use wtransport::Identity;

pub enum CertSource {
    /// Generated self-signed ECDSA P-256 (≤14-day validity).
    SelfSigned { sans: Vec<String> },
    /// Loaded from PEM files on disk.
    Mkcert { cert: PathBuf, key: PathBuf },
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
}
