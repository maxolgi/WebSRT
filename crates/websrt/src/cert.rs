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
//!   Parse PEM cert/key from in-memory byte slices (e.g. from a secret manager)
//!   directly into a WebTransport `Identity` — no temp files, no disk I/O.
//!   Browser uses normal PKI validation. Private key must be PKCS#8.

use anyhow::{Context, Result};
use std::path::PathBuf;
use wtransport::Identity;

pub enum CertSource {
    /// Generated self-signed ECDSA P-256 (≤14-day validity).
    SelfSigned { sans: Vec<String> },
    /// Loaded from PEM files on disk.
    Mkcert { cert: PathBuf, key: PathBuf },
    /// Loaded from PEM-encoded byte slices (e.g. from a secret manager).
    ///
    /// Parsed entirely in memory; never touches disk. The private key must be
    /// PKCS#8 (`-----BEGIN PRIVATE KEY-----`); PKCS#1 RSA and SEC1 EC keys are
    /// rejected — re-export with `openssl pkcs8 -topk8 -nocrypt`.
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
            CertSource::PemBytes { cert, key } => Self::build_pem_bytes(cert, key),
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

    fn build_pem_bytes(cert: Vec<u8>, key: Vec<u8>) -> Result<Self> {
        use rustls_pki_types::pem::PemObject;
        use rustls_pki_types::{CertificateDer, PrivateKeyDer};
        use wtransport::tls::{Certificate, CertificateChain, PrivateKey};

        let chain: Vec<Certificate> = CertificateDer::pem_slice_iter(&cert)
            .map(|der| {
                let der = der.context("parse cert pem")?;
                Certificate::from_der(der.to_vec()).context("build certificate from der")
            })
            .collect::<Result<_, _>>()
            .context("build certificate chain from pem")?;
        if chain.is_empty() {
            anyhow::bail!("no CERTIFICATE sections found in cert PEM");
        }

        // wtransport's `PrivateKey` only accepts PKCS#8 DER; PKCS#1 (RSA) and
        // SEC1 (EC) keys are rejected with an actionable message.
        let key_der = PrivateKeyDer::from_pem_slice(&key).context("parse key pem")?;
        let private_key = match key_der {
            PrivateKeyDer::Pkcs8(pkcs8) => {
                PrivateKey::from_der_pkcs8(pkcs8.secret_pkcs8_der().to_vec())
            }
            _ => anyhow::bail!(
                "PemBytes private key must be PKCS#8 ('PRIVATE KEY'); \
                 PKCS#1 RSA and SEC1 EC keys are not supported. \
                 Re-export with: openssl pkcs8 -topk8 -nocrypt -in current.key -out pkcs8.key"
            ),
        };

        let identity = Identity::new(CertificateChain::new(chain), private_key);
        Ok(Self {
            identity,
            der_sha256: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pem_bytes_round_trips_self_signed() {
        // Self-signed identities use PKCS#8 keys internally (rcgen serialize_der),
        // so their PEM is directly usable by PemBytes.
        let src = Identity::self_signed(["localhost"]).unwrap();
        let cert_pem = src.certificate_chain().as_slice()[0].to_pem().into_bytes();
        let key_pem = src.private_key().to_secret_pem().into_bytes();

        let cert = Cert::build(CertSource::PemBytes {
            cert: cert_pem,
            key: key_pem,
        })
        .await
        .expect("in-memory PEM parse should succeed");

        // Leaf DER must round-trip byte-for-byte.
        let parsed_leaf = &cert.identity.certificate_chain().as_slice()[0];
        assert_eq!(parsed_leaf.der(), src.certificate_chain().as_slice()[0].der());
    }

    #[tokio::test]
    async fn pem_bytes_rejects_empty_cert() {
        let err = Cert::build(CertSource::PemBytes {
            cert: b"-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n".to_vec(),
            key: b"-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----\n".to_vec(),
        })
        .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn pem_bytes_rejects_non_pkcs8_key() {
        // SEC1 EC private-key section. Empty body decodes to empty DER
        // (rustls-pki-types), so parsing succeeds and we hit the kind-match arm.
        let ec_pem = b"-----BEGIN EC PRIVATE KEY-----\n-----END EC PRIVATE KEY-----\n";
        let src = Identity::self_signed(["localhost"]).unwrap();
        let cert_pem = src.certificate_chain().as_slice()[0].to_pem().into_bytes();

        let err = Cert::build(CertSource::PemBytes {
            cert: cert_pem,
            key: ec_pem.to_vec(),
        })
        .await
        .err()
        .expect("non-PKCS#8 key should be rejected");
        assert!(
            err.to_string().contains("PKCS#8"),
            "error should mention PKCS#8: {err}"
        );
    }
}
