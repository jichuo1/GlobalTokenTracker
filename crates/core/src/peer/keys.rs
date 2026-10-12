//! Device key, self-signed certificate and TLS 1.3 configurations.
//!
//! A device's peer identity is the SHA-256 fingerprint of its long-lived
//! self-signed certificate. Viewers pin that fingerprint at pairing time and
//! never consult a CA or webpki trust store for peers: the custom verifier
//! below accepts exactly the pinned certificate (and still checks the TLS 1.3
//! handshake signature, so the server must hold the private key).

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const CERT_FILE: &str = "device-cert.der";
const KEY_FILE: &str = "device-key.der";
/// Name in the certificate. Never resolved or checked — the pin is the identity.
const CERT_NAME: &str = "gtt-peer.invalid";

/// `<data dir>/peer` — key, certificate and `peer.db`, owner-only.
pub fn peer_dir(db: &Path) -> PathBuf {
    db.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join("peer")
}

/// Create `dir` (and parents) and restrict it to the owner (0700 on Unix;
/// on Windows the per-user profile ACL already applies).
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| "peer: create data directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `bytes` to `path` atomically, readable by the owner only.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Lowercase hex SHA-256 of a DER certificate — a device's identity.
pub fn fingerprint(cert_der: &[u8]) -> String {
    hex(&Sha256::digest(cert_der))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Equality without an early exit on the first differing byte.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// This device's long-lived TLS identity.
pub struct DeviceKey {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    pub fingerprint: String,
}

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceKey")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl DeviceKey {
    /// Load the key pair from `dir`, or generate it once (ECDSA P-256,
    /// self-signed) and store it owner-only.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        ensure_private_dir(dir)?;
        let (cert_path, key_path) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
        if cert_path.is_file() && key_path.is_file() {
            let cert = std::fs::read(&cert_path).context("peer: read device certificate")?;
            let key = std::fs::read(&key_path).context("peer: read device key")?;
            let device = Self::from_der(cert, key);
            device
                .server_config()
                .context("peer: device key unusable")?;
            return Ok(device);
        }
        let generated = rcgen::generate_simple_self_signed(vec![CERT_NAME.to_string()])
            .context("peer: generate device certificate")?;
        let key = generated.signing_key.serialize_der();
        let cert = generated.cert.der().to_vec();
        write_private(&key_path, &key)?;
        write_private(&cert_path, &cert)?;
        Ok(Self::from_der(cert, key))
    }

    fn from_der(cert: Vec<u8>, key: Vec<u8>) -> Self {
        Self {
            fingerprint: fingerprint(&cert),
            cert: CertificateDer::from(cert),
            key: PrivatePkcs8KeyDer::from(key),
        }
    }

    /// TLS 1.3-only server configuration presenting this certificate.
    pub fn server_config(&self) -> Result<Arc<ServerConfig>> {
        let config = ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_no_client_auth()
            .with_single_cert(
                vec![self.cert.clone()],
                PrivateKeyDer::Pkcs8(self.key.clone_key()),
            )?;
        Ok(Arc::new(config))
    }
}

/// Accepts exactly one certificate fingerprint (or, unpinned, records what
/// it saw — used for `hello` and for pairing, which binds that fingerprint).
#[derive(Debug)]
pub(crate) struct PinVerifier {
    pin: Option<String>,
    seen: Mutex<Option<String>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinVerifier {
    pub(crate) fn new(pin: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            pin: pin.map(str::to_ascii_lowercase),
            seen: Mutex::new(None),
            algorithms: provider().signature_verification_algorithms,
        })
    }

    /// Fingerprint of the certificate the server presented, if any.
    pub(crate) fn seen(&self) -> Option<String> {
        self.seen.lock().ok().and_then(|s| s.clone())
    }

    /// The server presented a certificate other than the pinned one.
    pub(crate) fn mismatched(&self) -> bool {
        match (&self.pin, self.seen()) {
            (Some(pin), Some(seen)) => !ct_eq(pin.as_bytes(), seen.as_bytes()),
            _ => false,
        }
    }

    pub(crate) fn client_config(self: &Arc<Self>) -> Result<Arc<ClientConfig>> {
        let config = ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::clone(self) as Arc<dyn ServerCertVerifier>)
            .with_no_client_auth();
        Ok(Arc::new(config))
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let seen = fingerprint(end_entity);
        if let Ok(mut slot) = self.seen.lock() {
            *slot = Some(seen.clone());
        }
        match &self.pin {
            Some(pin) if !ct_eq(pin.as_bytes(), seen.as_bytes()) => {
                Err(rustls::Error::General("peer_fingerprint_mismatch".into()))
            }
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // TLS 1.2 is not offered; refuse rather than skip the check.
        Err(rustls::Error::General("tls12_not_supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
