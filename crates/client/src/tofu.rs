//! Server identity check, as with SSH: the first time, the certificate
//! fingerprint is saved (trust on first use). If it changes later, the
//! connection is refused, because someone may be impersonating the server.
//! The TLS signature check itself stays fully active, so the server must
//! actually hold the private key that belongs to the certificate.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

#[derive(Debug)]
pub struct TofuVerifier {
    pinned: Option<String>,
    seen: Mutex<Option<String>>,
    provider: Arc<CryptoProvider>,
}

impl TofuVerifier {
    pub fn new(pinned: Option<String>, provider: Arc<CryptoProvider>) -> Self {
        TofuVerifier { pinned, seen: Mutex::new(None), provider }
    }

    /// Fingerprint the server actually presented
    pub fn seen(&self) -> Option<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = tidespeak_proto::fingerprint(end_entity);
        *self.seen.lock().unwrap() = Some(fp.clone());
        match &self.pinned {
            Some(p) if !p.eq_ignore_ascii_case(&fp) => {
                Err(rustls::Error::General("Server-Fingerabdruck hat sich geändert".into()))
            }
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
