//! Who we are and whom we trust. No CA: each side keeps a self-signed
//! certificate, and the other's (its SHA-256) once paired. TLS 1.3 only,
//! both sides' certificates asked for.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::{CertifiedKey, SigningKey};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use std::fmt;
use std::sync::Arc;

/// Whether a certificate (its fingerprint) is a paired device's.
pub type Paired = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// The SHA-256 of a certificate, in hex: how devices know each other.
pub fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Our certificate and what signs for it: a key we hold, or one that never
/// leaves Android's keystore (signing asked of Kotlin).
pub struct Identity {
    key: Arc<CertifiedKey>,
}

impl Identity {
    /// A certificate and its PKCS#8 key, DER.
    pub fn new(certificate: Vec<u8>, key: Vec<u8>) -> Result<Identity, String> {
        let key = provider().key_provider.load_private_key(PrivateKeyDer::Pkcs8(key.into()))
            .map_err(|e| format!("the key: {e}"))?;
        Ok(Self::signed_by(certificate, key))
    }

    /// The PC's: the files plusone/link/trust.py made (PEM).
    pub fn from_pem_files(certificate: &str, key: &str) -> Result<Identity, String> {
        let cert = CertificateDer::from_pem_file(certificate).map_err(|e| format!("{certificate}: {e}"))?;
        let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{key}: {e}"))?;
        let key = provider().key_provider.load_private_key(key).map_err(|e| format!("the key: {e}"))?;
        Ok(Self::signed_by(cert.to_vec(), key))
    }

    /// A certificate whose key signs elsewhere.
    pub fn signed_by(certificate: Vec<u8>, key: Arc<dyn SigningKey>) -> Identity {
        Identity { key: Arc::new(CertifiedKey::new(vec![CertificateDer::from(certificate)], key)) }
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.key.cert[0])
    }

    pub(crate) fn client_config(&self, paired: Paired) -> Result<Arc<rustls::ClientConfig>, String> {
        let config = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13]).map_err(|e| e.to_string())?
            .dangerous().with_custom_certificate_verifier(Arc::new(Pinned::new(paired)))
            .with_client_cert_resolver(Arc::new(Ours(self.key.clone())));
        Ok(Arc::new(config))
    }

    pub(crate) fn server_config(&self, paired: Paired) -> Result<Arc<rustls::ServerConfig>, String> {
        let config = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13]).map_err(|e| e.to_string())?
            .with_client_cert_verifier(Arc::new(Pinned::new(paired)))
            .with_cert_resolver(Arc::new(Ours(self.key.clone())));
        Ok(Arc::new(config))
    }
}

/// Our certificate, whatever the other side asks.
#[derive(Debug)]
struct Ours(Arc<CertifiedKey>);

impl rustls::client::ResolvesClientCert for Ours {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

impl rustls::server::ResolvesServerCert for Ours {
    fn resolve(&self, _hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// The other side's certificate: a paired one, and its signature good.
struct Pinned {
    paired: Paired,
    provider: Arc<CryptoProvider>,
}

impl fmt::Debug for Pinned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pinned")
    }
}

impl Pinned {
    fn new(paired: Paired) -> Pinned {
        Pinned { paired, provider: provider() }
    }

    fn check(&self, certificate: &CertificateDer<'_>) -> Result<(), Error> {
        if (self.paired)(&fingerprint(certificate)) {
            Ok(())
        } else {
            Err(Error::General("not a paired device".into()))
        }
    }

    fn signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct)
                 -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
                          _name: &ServerName<'_>, _ocsp: &[u8], _now: UnixTime) -> Result<ServerCertVerified, Error> {
        self.check(end_entity).map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2".into()))
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        self.signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
                          _now: UnixTime) -> Result<ClientCertVerified, Error> {
        self.check(end_entity).map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2".into()))
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        self.signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}
