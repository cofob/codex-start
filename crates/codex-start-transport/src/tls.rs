//! Pinned TLS for both the direct and overlay transports.

use crate::{Error, Stream};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};
use std::{path::Path, sync::Arc};
use tokio_rustls::{TlsAcceptor, TlsConnector};

#[derive(Debug)]
struct PinnedVerifier {
    fingerprint: Option<String>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = fingerprint(cert.as_ref())
            .map_err(|_| rustls::Error::General("invalid server certificate".into()))?;
        if self
            .fingerprint
            .as_ref()
            .is_some_and(|expected| *expected != actual)
        {
            return Err(rustls::Error::General(
                "server identity changed; scan a new trusted invitation".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

///
/// # Errors
/// Returns an error if the certificate, private key, identity pin, or TLS connection is invalid.
pub fn fingerprint(cert: &[u8]) -> Result<String, Error> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert)
        .map_err(|_| Error::Protocol("invalid TLS certificate".into()))?;
    Ok(codex_start_remote::crypto::digest(cert.public_key().raw))
}

///
/// # Errors
/// Returns an error if the certificate, private key, identity pin, or TLS connection is invalid.
pub fn identity(root: &Path) -> Result<(TlsAcceptor, String), Error> {
    let key_path = root.join("tls-key.der");
    let cert_path = root.join("tls-cert.der");
    if !key_path.exists() && !cert_path.exists() {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["cs.fob.wtf".into()])
                .map_err(|e| Error::Protocol(e.to_string()))?;
        private_write(&key_path, &signing_key.serialize_der())?;
        private_write(&cert_path, cert.der().as_ref())?;
    }
    let cert = std::fs::read(cert_path)?;
    let key = std::fs::read(key_path)?;
    let fp = fingerprint(&cert)?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::Protocol(e.to_string()))?
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert)],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
    )
    .map_err(|e| Error::Protocol(e.to_string()))?;
    Ok((TlsAcceptor::from(Arc::new(config)), fp))
}

/// `None` requires an authenticated Yggdrasil stream, or direct discovery/manual pairing.
/// Direct discovery must not send invitation secrets without a pin.
///
/// # Errors
/// Returns an error if the certificate, private key, identity pin, or TLS connection is invalid.
pub async fn connect(stream: Stream, pin: Option<String>) -> Result<(Stream, String), Error> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Protocol(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
            fingerprint: pin,
            provider,
        }))
        .with_no_client_auth();
    let stream = TlsConnector::from(Arc::new(config))
        .connect(
            ServerName::try_from("cs.fob.wtf").map_err(|e| Error::Protocol(e.to_string()))?,
            stream,
        )
        .await?;
    let cert = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or_else(|| Error::Protocol("missing server certificate".into()))?;
    let fp = fingerprint(cert.as_ref())?;
    Ok((Box::new(stream), fp))
}

///
/// # Errors
/// Returns an error if the certificate, private key, identity pin, or TLS connection is invalid.
pub fn private_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
