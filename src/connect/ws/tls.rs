use rustls::ClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error, SignatureScheme};
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

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
                // Hostname mismatch is expected: devices use IP addresses.
                // The CA chain was still validated by the inner verifier before
                // it rejected the hostname; the cert is authentic.
                //
                // Logged to surface a LAN MITM in traces: a legitimate
                // device mismatch happens once per connection attempt, while
                // a hostile peer could trigger a flood of these.
                crate::vprintln!(
                    "[connect::ws::tls] Accepted TIDAL-signed cert despite hostname mismatch: {:?}",
                    server_name
                );
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

/// Build a TLS acceptor using the embedded TIDAL Connect server cert+key.
#[cfg(has_connect_server_key)]
pub(super) fn build_tls_acceptor() -> anyhow::Result<TlsAcceptor> {
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    static CERT_PEM: &[u8] = include_bytes!("certs/tidal_server_cert.pem");
    static KEY_PEM: &[u8] = include_bytes!("certs/tidal_server_key.pem");

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut &CERT_PEM[..]).collect::<Result<Vec<_>, _>>()?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &KEY_PEM[..])
        .map_err(|e| anyhow::anyhow!("Failed to parse private key: {}", e))?
        .ok_or_else(|| anyhow::anyhow!("No private key found in PEM"))?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Stand-in for a build the server key never reached, `include_bytes!` having no way to
/// take a file that is absent. `ConnectReceiver::start` gives up on this error before it
/// reaches the mDNS advertiser, leaving such a build to announce no receiver at all.
#[cfg(not(has_connect_server_key))]
pub(super) fn build_tls_acceptor() -> anyhow::Result<TlsAcceptor> {
    anyhow::bail!("no TIDAL Connect server key was bundled at build time")
}
