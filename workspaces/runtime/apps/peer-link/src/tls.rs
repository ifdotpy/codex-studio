//! TLS 1.3 with raw public keys (RFC 7250): each side presents its Ed25519 pairing key, and
//! the other side accepts only a key that the Studio pairing records hold.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use rustls::client::AlwaysResolvesClientRawPublicKeys;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature_with_raw_key,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime,
};
use rustls::server::AlwaysResolvesServerRawPublicKeys;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error, PeerIncompatible,
    SignatureScheme,
};

pub const ALPN: &[u8] = b"studio-peer/1";

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The DER SubjectPublicKeyInfo of a PEM public key, as the pairing records store it.
pub fn spki(pem: &str) -> Result<Vec<u8>, String> {
    SubjectPublicKeyInfoDer::from_pem_slice(pem.as_bytes())
        .map(|key| key.as_ref().to_vec())
        .map_err(|_| "The public key is not a PEM SubjectPublicKeyInfo".to_owned())
}

/// The local pairing key as a raw public key credential.
pub fn certified_key(
    provider: &CryptoProvider,
    private_pem: &str,
    public_pem: &str,
) -> Result<Arc<CertifiedKey>, String> {
    let private = PrivateKeyDer::from_pem_slice(private_pem.as_bytes())
        .map_err(|_| "The private key is not PEM PKCS#8".to_owned())?;
    let signer = provider
        .key_provider
        .load_private_key(private)
        .map_err(|_| "The private key is not an Ed25519 key".to_owned())?;
    let public = spki(public_pem)?;
    if signer.public_key().map(|key| key.as_ref().to_vec()) != Some(public.clone()) {
        return Err("The public key does not belong to the private key".to_owned());
    }
    Ok(Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(public)],
        signer,
    )))
}

fn tls13_signature(
    algorithms: &WebPkiSupportedAlgorithms,
    message: &[u8],
    key: &CertificateDer<'_>,
    signature: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, Error> {
    verify_tls13_signature_with_raw_key(
        message,
        &SubjectPublicKeyInfoDer::from(key.as_ref()),
        signature,
        algorithms,
    )
}

/// The client side: the server must present the key of the paired server it dials.
#[derive(Debug)]
struct PinnedServer {
    key: Vec<u8>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if intermediates.is_empty() && end_entity.as_ref() == self.key.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::InvalidCertificate(CertificateError::UnknownIssuer))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(
            PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        tls13_signature(&self.algorithms, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// The keys of the paired servers. The backend replaces the set when a pairing changes.
pub type PairedKeys = Arc<RwLock<HashSet<Vec<u8>>>>;

/// The server side: the client must present the key of a paired server.
#[derive(Debug)]
struct PairedClients {
    keys: PairedKeys,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for PairedClients {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        let paired = self
            .keys
            .read()
            .map(|keys| keys.contains(end_entity.as_ref()))
            .unwrap_or(false);
        if intermediates.is_empty() && paired {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(Error::InvalidCertificate(CertificateError::UnknownIssuer))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(
            PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        tls13_signature(&self.algorithms, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

pub fn server_config(
    provider: Arc<CryptoProvider>,
    key: Arc<CertifiedKey>,
    keys: PairedKeys,
) -> Result<rustls::ServerConfig, String> {
    let algorithms = provider.signature_verification_algorithms;
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| error.to_string())?
        .with_client_cert_verifier(Arc::new(PairedClients { keys, algorithms }))
        .with_cert_resolver(Arc::new(AlwaysResolvesServerRawPublicKeys::new(key)));
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(config)
}

pub fn client_config(
    provider: Arc<CryptoProvider>,
    key: Arc<CertifiedKey>,
    server: Vec<u8>,
) -> Result<rustls::ClientConfig, String> {
    let algorithms = provider.signature_verification_algorithms;
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| error.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServer {
            key: server,
            algorithms,
        }))
        .with_client_cert_resolver(Arc::new(AlwaysResolvesClientRawPublicKeys::new(key)));
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(config)
}
