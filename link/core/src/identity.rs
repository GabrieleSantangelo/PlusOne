//! Who we are and whom we trust. No CA: each side keeps a self-signed
//! certificate, and the other's (its SHA-256) once paired. TLS 1.3 only,
//! both sides' certificates asked for, every time: no session is resumed
//! (architecture §3.14, F-1), so a device forgotten is refused at once,
//! and a stolen resumption secret stands for no one.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{alg_id, CertificateDer, PrivateKeyDer, ServerName, SignatureVerificationAlgorithm, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::{CertifiedKey, SigningKey};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use std::fmt;
use std::sync::{Arc, OnceLock};

/// Whether a certificate (its fingerprint) is a paired device's.
pub type Paired = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// The SHA-256 of a certificate, in hex: how devices know each other.
pub fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// aws-lc-rs's, with the hybrid post-quantum key exchange first
/// (X25519MLKEM768: architecture §3.14), and X25519 for an app or PC from
/// before it (TLS 1.3 signs the choice: no one in between can force it).
pub(crate) fn provider() -> Arc<CryptoProvider> {
    use rustls::crypto::aws_lc_rs::{default_provider, kx_group};
    Arc::new(CryptoProvider { kx_groups: vec![kx_group::X25519MLKEM768, kx_group::X25519], ..default_provider() })
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
        let mut config = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13]).map_err(|e| e.to_string())?
            .dangerous().with_custom_certificate_verifier(Arc::new(Pinned::new(paired)))
            .with_client_cert_resolver(Arc::new(Ours(self.key.clone())));
        config.resumption = rustls::client::Resumption::disabled();
        Ok(Arc::new(config))
    }

    pub(crate) fn server_config(&self, paired: Paired) -> Result<Arc<rustls::ServerConfig>, String> {
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13]).map_err(|e| e.to_string())?
            .with_client_cert_verifier(Arc::new(Pinned::new(paired)))
            .with_cert_resolver(Arc::new(Ours(self.key.clone())));
        // No tickets, no sessions kept, no early data (0 already: QUIC's
        // 0-RTT is never taken either).
        config.send_tls13_tickets = 0;
        config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        config.max_early_data_size = 0;
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

/// The one signature either side makes: ECDSA P-256 with SHA-256 (the
/// tablet's keystore key, the PC's plusone/link/trust.py); no other is
/// taken (architecture §3.14, F-8).
const SCHEME: SignatureScheme = SignatureScheme::ECDSA_NISTP256_SHA256;

/// The verifiers for SCHEME, P-256 keys' alone (aws-lc-rs takes P-384
/// ones for it too).
fn p256(all: &WebPkiSupportedAlgorithms) -> WebPkiSupportedAlgorithms {
    static ONLY: OnceLock<WebPkiSupportedAlgorithms> = OnceLock::new();
    *ONLY.get_or_init(|| {
        let algorithms: Vec<&'static dyn SignatureVerificationAlgorithm> = all.mapping.iter()
            .filter(|(scheme, _)| *scheme == SCHEME)
            .flat_map(|(_, algorithms)| algorithms.iter().copied())
            .filter(|a| a.public_key_alg_id() == alg_id::ECDSA_P256 && a.signature_alg_id() == alg_id::ECDSA_SHA256)
            .collect();
        let algorithms: &'static [&'static dyn SignatureVerificationAlgorithm] = Box::leak(algorithms.into_boxed_slice());
        WebPkiSupportedAlgorithms { all: algorithms, mapping: Box::leak(Box::new([(SCHEME, algorithms)])) }
    })
}

/// The other side's certificate: a paired one (the whole certificate, its
/// key with it), and its signature good.
struct Pinned {
    paired: Paired,
    algorithms: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for Pinned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pinned")
    }
}

impl Pinned {
    fn new(paired: Paired) -> Pinned {
        Pinned { paired, algorithms: p256(&provider().signature_verification_algorithms) }
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
        if dss.scheme != SCHEME {
            return Err(Error::PeerIncompatible(rustls::PeerIncompatible::NoSignatureSchemesInCommon));
        }
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        vec![SCHEME]
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{ClientConnection, HandshakeKind, ServerConnection};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn made(algorithm: &'static rcgen::SignatureAlgorithm) -> (Vec<u8>, Vec<u8>) {
        let key = rcgen::KeyPair::generate_for(algorithm).unwrap();
        let cert = rcgen::CertificateParams::new(vec!["plus-one".into()]).unwrap().self_signed(&key).unwrap();
        (cert.der().to_vec(), key.serialize_der())
    }

    fn identity() -> Identity {
        let (cert, key) = made(&rcgen::PKCS_ECDSA_P256_SHA256);
        Identity::new(cert, key).unwrap()
    }

    fn only(fingerprint: String) -> Paired {
        Arc::new(move |seen: &str| seen == fingerprint)
    }

    /// A handshake in memory, the two ends' bytes to each other until
    /// neither has more: the client's end, or the first error.
    fn handshake(client: &Arc<rustls::ClientConfig>, server: &Arc<rustls::ServerConfig>) -> Result<ClientConnection, Error> {
        let mut c = ClientConnection::new(client.clone(), ServerName::try_from("plus-one.invalid").unwrap())?;
        let mut s = ServerConnection::new(server.clone())?;
        loop {
            let mut moved = false;
            let mut bytes = Vec::new();
            while c.wants_write() {
                c.write_tls(&mut bytes).unwrap();
            }
            let mut left = &bytes[..];
            while !left.is_empty() {
                moved = true;
                s.read_tls(&mut left).unwrap();
                s.process_new_packets()?;
            }
            bytes.clear();
            while s.wants_write() {
                s.write_tls(&mut bytes).unwrap();
            }
            let mut left = &bytes[..];
            while !left.is_empty() {
                moved = true;
                c.read_tls(&mut left).unwrap();
                c.process_new_packets()?;
            }
            if !moved {
                return Ok(c);
            }
        }
    }

    #[test]
    fn every_handshake_is_whole_and_a_device_forgotten_is_refused_at_once() {
        let (tablet, pc) = (identity(), identity());
        let paired = Arc::new(AtomicBool::new(true));
        let still = paired.clone();
        let pc_fingerprint = pc.fingerprint();
        let server = tablet.server_config(Arc::new(move |seen: &str| still.load(Ordering::SeqCst) && seen == pc_fingerprint))
            .unwrap();
        let client = pc.client_config(only(tablet.fingerprint())).unwrap();
        for _ in 0..2 {
            let c = handshake(&client, &server).unwrap();
            assert!(!c.is_handshaking());
            assert_eq!(c.handshake_kind(), Some(HandshakeKind::Full));
            assert_eq!(c.negotiated_key_exchange_group().map(|group| group.name()),
                       Some(rustls::NamedGroup::X25519MLKEM768));
        }
        paired.store(false, Ordering::SeqCst);
        assert!(handshake(&client, &server).is_err());
    }

    #[test]
    fn the_paired_certificate_signed_by_another_key_is_refused() {
        let (tablet, pc, thief) = (identity(), identity(), identity());
        let server = tablet.server_config(only(pc.fingerprint())).unwrap();
        // The PC's certificate (public), the thief's key.
        let posing = Identity::signed_by(pc.key.cert[0].to_vec(), thief.key.key.clone());
        assert!(handshake(&posing.client_config(only(tablet.fingerprint())).unwrap(), &server).is_err());
        assert!(handshake(&thief.client_config(only(tablet.fingerprint())).unwrap(), &server).is_err());
        assert!(handshake(&pc.client_config(only(tablet.fingerprint())).unwrap(), &server).is_ok());
    }

    #[test]
    fn only_ecdsa_p256_signs_for_a_device() {
        let tablet = identity();
        let (cert, key) = made(&rcgen::PKCS_ED25519);
        let pc = Identity::new(cert, key).unwrap();
        let server = tablet.server_config(only(pc.fingerprint())).unwrap();
        assert!(handshake(&pc.client_config(only(tablet.fingerprint())).unwrap(), &server).is_err());
    }
}
