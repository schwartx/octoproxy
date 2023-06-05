use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use clap::Parser;
use octoproxy_lib::{
    config::{parse_log_level_str, parse_socket_address, TomlFileConfig},
    tls::{build_tls_server_config, NeverProducesTickets, QUIC_ALPN},
};
use serde::Deserialize;
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, log::warn, metadata::LevelFilter, trace, trace_span};

use crate::proxy::{handle_h2_connection, handle_quic_connection};

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
pub struct Opts {
    /// http proxy server listening address
    #[arg(short = 'l', long = "listen")]
    listen_address: Option<String>,
    /// trusted client cert
    #[arg(long = "auth")]
    auth: Option<PathBuf>,
    /// config
    #[arg(short = 'c', long = "config", default_value = "config.toml")]
    config: PathBuf,
}

/// Config Toml
#[derive(Debug, Deserialize)]
struct FileConfig {
    log_level: String,
    listen_address: String,
    /// server key
    auth_key: PathBuf,
    /// server cert
    auth_cert: PathBuf,
    /// trusted client cert
    client_ca: Option<PathBuf>,

    #[serde(default = "default_max_concurrent_tasks")]
    max_concurrent_tasks: usize,
    quic: Option<bool>,
    use_0rtt: Option<bool>,
}

fn default_max_concurrent_tasks() -> usize {
    32
}

impl TomlFileConfig for FileConfig {}

/// Global config
pub struct Config {
    pub log_level: LevelFilter,

    listen_address: SocketAddr,
    max_concurrent_tasks: usize,

    acceptor: TlsAcceptor,
    quic_config: Option<QuicConfig>,
    use_0rtt: bool,
}

pub struct QuicConfig {
    server_config: quinn::ServerConfig,
}

impl Config {
    /// command line option overwrites listen_address
    pub fn new(opts: Opts) -> anyhow::Result<Self> {
        let file_config = FileConfig::load_from_file(opts.config)?;

        let listen_address = match opts.listen_address {
            Some(listen_address) => listen_address,
            None => file_config.listen_address,
        };

        let client_ca = match opts.auth {
            Some(client_ca) => Some(client_ca),
            None => file_config.client_ca,
        };

        let log_level = parse_log_level_str(&file_config.log_level);

        let listen_address = parse_socket_address(&listen_address)?;

        let tls_config =
            build_tls_server_config(file_config.auth_cert, file_config.auth_key, client_ca)?;

        let use_0rtt = file_config.use_0rtt.unwrap_or(true);
        let quic_config = match file_config.quic {
            Some(true) => {
                let mut tls_config = tls_config.clone();
                tls_config.max_early_data_size = u32::MAX;
                tls_config.ticketer = Arc::new(NeverProducesTickets {});

                tls_config.alpn_protocols = vec![QUIC_ALPN.into()];
                let server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
                let quic_config = QuicConfig { server_config };
                Some(quic_config)
            }
            None | Some(false) => None,
        };

        let acceptor = TlsAcceptor::from(Arc::new(tls_config));

        let config = Self {
            use_0rtt,
            log_level,
            listen_address,
            acceptor,
            max_concurrent_tasks: file_config.max_concurrent_tasks,
            quic_config,
        };

        Ok(config)
    }

    pub async fn run_h2(&self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(self.listen_address).await?;

        let semaphore = Arc::new(Semaphore::new(self.max_concurrent_tasks));

        let acceptor = self.acceptor.clone();
        info!("listening: {}", self.listen_address);

        while let Ok((inbound, peer_addr)) = listener.accept().await {
            debug!("Accepting new connection from {:?}", peer_addr);
            let permit = Arc::clone(&semaphore).acquire_owned().await;

            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_h2_connection(acceptor, inbound).await {
                    trace!("{:?}", e);
                }
                drop(permit)
            });
        }
        Ok(())
    }

    pub async fn run_quic(&self) -> anyhow::Result<()> {
        let server_config = match &self.quic_config {
            None => {
                info!("Not using quic connection");
                return Ok(());
            }
            Some(quic_config) => quic_config.server_config.clone(),
        };

        let endpoint = quinn::Endpoint::server(server_config, self.listen_address)?;

        info!("quic proxy listening on {}", self.listen_address);
        let use_0rtt = self.use_0rtt;
        while let Some(conn) = endpoint.accept().await {
            trace_span!("New connection being attempted");

            tokio::spawn(async move {
                let connection = if use_0rtt {
                    match conn.into_0rtt() {
                        Ok((connection, _established)) => connection,
                        Err(conn) => {
                            debug!("Failed to init 0-rtt");
                            match conn.await {
                                Ok(connection) => connection,
                                Err(e) => {
                                    warn!("Failed to connect: {}", e);
                                    return;
                                }
                            }
                        }
                    }
                } else {
                    match conn.await {
                        Ok(connection) => connection,
                        Err(e) => {
                            warn!("Failed to connect: {}", e);
                            return;
                        }
                    }
                };

                if let Err(err) = handle_quic_connection(connection).await {
                    debug!("quic connection error: {:?}", err)
                }
            });
        }

        Ok(())
    }
}
