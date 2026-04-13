use rustls::ClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error, SignatureScheme};
use std::sync::Arc;

const TIDAL_TS_CA_PEM: &str = include_str!("certs/tidal_ts_ca.pem");
const TIDAL_ROOT_CA_PEM: &str = include_str!("certs/tidal_root_ca.pem");

/// Build a rustls ClientConfig that trusts only the TIDAL CA bundle
/// and skips hostname verification (devices are addressed by IP, not DNS).
pub(crate) fn tidal_client_tls_config() -> anyhow::Result<Arc<ClientConfig>> {
    let mut root_store = rustls::RootCertStore::empty();

    for pem in [TIDAL_TS_CA_PEM, TIDAL_ROOT_CA_PEM] {
        let mut reader = std::io::BufReader::new(pem.as_bytes());
        for cert in rustls_pemfile::certs(&mut reader) {
            root_store.add(cert?)?;
        }
    }

    // Build a standard WebPKI verifier, then wrap it to skip hostname check
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(root_store),
        provider.clone(),
    )
    .build()?;

    let verifier = TidalCertVerifier { inner };

    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();

    Ok(Arc::new(config))
}

/// Custom certificate verifier that delegates CA chain validation to the
/// standard WebPKI verifier but skips hostname verification.
///
/// SAFETY: Hostname check is intentionally skipped because TIDAL Connect
/// devices are discovered via mDNS and addressed by IP, not DNS hostname.
/// The server certificate is signed by TIDAL's private CA, which is
/// sufficient to authenticate the device.
#[derive(Debug)]
struct TidalCertVerifier {
    inner: Arc<dyn ServerCertVerifier>,
}

impl ServerCertVerifier for TidalCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        // Try standard verification first (works if server cert has matching SAN)
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(v) => Ok(v),
            Err(Error::InvalidCertificate(rustls::CertificateError::NotValidForName)) => {
                // Hostname mismatch is expected - devices use IP addresses.
                // The CA chain was still validated by the inner verifier before
                // it rejected the hostname, so the cert is authentic.
                Ok(ServerCertVerified::assertion())
            }
            Err(e) => Err(e),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}
