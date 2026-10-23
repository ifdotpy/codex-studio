//! Preserve the client's WebPKI trust roots across reqwest API updates.
use std::sync::Arc;

pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    // Ring supports the default TLS 1.2/1.3 versions, so this configuration
    // cannot fail. These are the versions reqwest 0.12 enabled by default.
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    reqwest::Client::builder().tls_backend_preconfigured(tls)
}
