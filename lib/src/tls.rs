use std::{io::BufReader, path::PathBuf};

use anyhow::{bail, Context};
use rustls::{
    client::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    server::{AllowAnyAuthenticatedClient, NoClientAuth, ServerSessionMemoryCache},
    ClientConfig, DigitallySignedStruct, OwnedTrustAnchor, RootCertStore, ServerConfig,
};

pub const DEFAULT_FORMARD_PORT: u16 = 8443;

pub struct NoVerifier;

pub static QUIC_ALPN: &[u8] = b"h3";

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::Certificate,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::Certificate,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
}

pub fn load_private_key(filename: &PathBuf) -> anyhow::Result<rustls::PrivateKey> {
    let keyfile = std::fs::File::open(filename).context("cannot open private key file")?;
    let mut reader = std::io::BufReader::new(keyfile);

    loop {
        match rustls_pemfile::read_one(&mut reader)? {
            Some(rustls_pemfile::Item::RSAKey(key)) => return Ok(rustls::PrivateKey(key)),
            Some(rustls_pemfile::Item::PKCS8Key(key)) => return Ok(rustls::PrivateKey(key)),
            Some(rustls_pemfile::Item::ECKey(key)) => return Ok(rustls::PrivateKey(key)),
            None => break,
            _ => {}
        }
    }

    bail!("no keys found (encrypted keys not supported)")
}

/// load cert
/// TODO support many files?
pub fn load_certs(filename: &PathBuf) -> anyhow::Result<Vec<rustls::Certificate>> {
    let certfile = std::fs::File::open(filename).context("cannot open certificate file")?;
    let mut reader = BufReader::new(certfile);
    Ok(rustls_pemfile::certs(&mut reader)?
        .iter()
        .map(|v| rustls::Certificate(v.clone()))
        .collect())
}

pub fn build_client_root_store(cafile: &Option<PathBuf>) -> anyhow::Result<RootCertStore> {
    let mut root_store = RootCertStore::empty();
    if let Some(cafile) = cafile {
        let mut pem =
            std::io::BufReader::new(std::fs::File::open(cafile).context("cannot open ca file")?);
        let certs = rustls_pemfile::certs(&mut pem)?;
        let (added, _skipped) = root_store.add_parsable_certificates(&certs);
        if added == 0 {
            bail!("cannot parse cert");
        }
    } else {
        root_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
            OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));
    }
    Ok(root_store)
}

pub fn build_tls_server_config(
    certs: PathBuf,
    key: PathBuf,
    client_ca: Option<PathBuf>,
) -> anyhow::Result<ServerConfig> {
    let certs = load_certs(&certs)?;
    let keys = load_private_key(&key)?;

    let tls_config = rustls::ServerConfig::builder().with_safe_defaults();

    // add trusted client certs
    let client_verifier = match client_ca {
        Some(client_certs) => {
            let client_certs = load_certs(&client_certs)?;
            let mut client_auth_roots = RootCertStore::empty();
            for cert in client_certs {
                client_auth_roots.add(&cert)?;
            }
            AllowAnyAuthenticatedClient::new(client_auth_roots).boxed()
        }
        None => {
            // dangerous
            // TODO throw a warning if not given client ca or put this into `dangerous` feature
            NoClientAuth::boxed()
        }
    };

    let mut tls_config = tls_config
        // .with_cipher_suites(&[rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256])
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certs, keys)?;

    // using ticket
    tls_config.ticketer = rustls::Ticketer::new()?;
    tls_config.session_storage = ServerSessionMemoryCache::new(256);
    Ok(tls_config)
}

pub fn build_tls_client_config(
    cacert: &Option<PathBuf>,
    client_cert: &PathBuf,
    client_key: &PathBuf,
) -> anyhow::Result<ClientConfig> {
    let root_store = build_client_root_store(cacert)?;

    let tls_config = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(root_store);

    let certs = load_certs(client_cert)?;
    let key = load_private_key(client_key)?;
    let tls_config = tls_config.with_single_cert(certs, key)?;

    Ok(tls_config)
}

pub struct NeverProducesTickets {}

impl rustls::server::ProducesTickets for NeverProducesTickets {
    fn enabled(&self) -> bool {
        false
    }
    fn lifetime(&self) -> u32 {
        0
    }
    fn encrypt(&self, _bytes: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn decrypt(&self, _bytes: &[u8]) -> Option<Vec<u8>> {
        None
    }
}
