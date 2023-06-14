#![allow(unused)]
use anyhow::{bail, Context};
use futures::stream::{self, StreamExt};
use hashring::HashRing;
use hyper::client::HttpConnector;
use hyper::Client;
use octoproxy_lib::metric::BackendStatus;
use octoproxy_lib::proxy_client::ProxyConnector;
use tracing::metadata::LevelFilter;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::read_to_string;
use std::io::BufRead;
use std::path::PathBuf;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use octoproxy_lib::config::{parse_log_level_str, parse_socket_address, TomlFileConfig};

use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::info;

use crate::backends::{Backend, FileBackendConfig};
use crate::balance::{
    Balance, Frist, LeastLoadedTime, LoadBalancingAlgorithm, Random, RoundRobin, UriHash,
};
use crate::metric::{Metric, PeerInfo};
use crate::Cmd;

/// Config Toml
#[derive(Debug, Deserialize)]
struct FileConfig {
    listen_address: String,
    log_level: String,
    balance: Balance,
    host_rewrite: Option<PathBuf>,
    /// listen address for metric service
    metric_address: String,
    backends: HashMap<String, FileBackendConfig>,
}

impl TomlFileConfig for FileConfig {}

/// Global config
pub(crate) struct Config {
    pub(crate) listen_address: SocketAddr,
    pub(crate) metric_address: SocketAddr,
    pub(crate) log_level: LevelFilter,
    pub(crate) balance: Box<dyn LoadBalancingAlgorithm<ConfigBackend> + Send + Sync + 'static>,
    pub(crate) backends: Vec<ConfigBackend>,

    host_rewriter: Option<HostRewriter>,

    pub(crate) non_connect_method_client: Client<ProxyConnector<HttpConnector>>,
}

#[derive(Clone)]
pub(crate) struct ConfigBackend {
    pub(crate) backend: Arc<RwLock<Backend>>,
    pub(crate) metric: Arc<Metric>,
}

struct HostRewriter {
    hosts: BTreeMap<String, String>,
}

impl HostRewriter {
    fn new(host_rewrite_file: impl std::io::Read) -> anyhow::Result<Option<Self>> {
        let buf = std::io::BufReader::new(host_rewrite_file);
        let mut hosts = BTreeMap::new();

        for line in buf.lines() {
            let line = line.context("Failed to get line")?;
            if line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.split(' ').collect();
            if parts.len() != 2 {
                // ignore this
                continue;
            }

            hosts.insert(parts[0].to_string(), parts[1].to_string());
        }

        if hosts.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self { hosts }))
    }

    fn rewrite(&self, request_host: &str) -> Option<&str> {
        self.hosts.get(request_host).map(|x| x.as_str())
    }
}

impl Config {
    /// command line option overwrites listen_address
    pub(crate) fn new(opts: Cmd) -> anyhow::Result<Self> {
        let file_config =
            FileConfig::load_from_file(opts.config).context("Could not load config file")?;

        let listen_address = match opts.listen_address {
            Some(listen_address) => listen_address,
            None => file_config.listen_address,
        };

        let listen_address = parse_socket_address(&listen_address)
            .context("Could not parse proxy listen address")?;
        let metric_address = parse_socket_address(&file_config.metric_address)
            .context("Could not parse metric listen address")?;

        let log_level = parse_log_level_str(&file_config.log_level);

        let proxy = {
            let proxy_uri = format!("http://{}", listen_address)
                .parse()
                .context("Could not parse proxy listen address")?;
            let connector = HttpConnector::new();
            ProxyConnector::new(connector, proxy_uri)
        };

        let non_connect_method_client =
            hyper::client::Client::builder().build::<_, hyper::Body>(proxy);

        let host_rewriter = if let Some(host_rewriter) = file_config.host_rewrite {
            let host_rewriter =
                std::fs::File::open(host_rewriter).context("Failed to open host rewrite file")?;

            HostRewriter::new(host_rewriter).context("Failed to parse host rewrite config")?
        } else {
            None
        };

        let mut config = Self {
            listen_address,
            backends: Vec::new(),
            balance: Box::new(Frist),
            metric_address,
            log_level,
            host_rewriter,
            non_connect_method_client,
        };

        config
            .populate_backends(file_config.backends)
            .context("Could not setup backends")?;

        config.balance = match file_config.balance {
            Balance::RoundRobin => Box::new(RoundRobin::new()),
            Balance::Random => Box::new(Random {}),
            Balance::LeastLoadedTime => Box::new(LeastLoadedTime {}),
            Balance::UriHash => {
                let mut ring: HashRing<usize> = HashRing::new();
                for backend_id in 0..config.backends.len() {
                    ring.add(backend_id)
                }
                Box::new(UriHash::new(ring))
            }
            Balance::Frist => Box::new(Frist),
        };

        info!("load balance algorithm: {:?}", file_config.balance);

        Ok(config)
    }

    fn populate_backends(
        &mut self,
        mut file_backend_configs: HashMap<String, FileBackendConfig>,
    ) -> anyhow::Result<()> {
        for (file_backend_name, file_backend_config) in file_backend_configs.drain() {
            let backend = Backend::new(file_backend_name, file_backend_config)
                .context("Fail to initialize backend")?;

            self.backends.push(ConfigBackend {
                metric: Arc::new(backend.metric.clone()),
                backend: Arc::new(RwLock::new(backend)),
            });
        }
        Ok(())
    }

    async fn available_backends(&self) -> Vec<ConfigBackend> {
        stream::iter(self.backends.iter())
            .filter_map(|backend| async move {
                if backend.backend.read().await.get_status() == BackendStatus::Normal {
                    Some(backend.clone())
                } else {
                    None
                }
            })
            .collect()
            .await
    }

    pub(crate) async fn next_available_backend(
        &self,
        peer: &PeerInfo,
    ) -> Option<Arc<RwLock<Backend>>> {
        let backends = self.available_backends().await;
        if backends.is_empty() {
            None
        } else if backends.len() == 1 {
            backends.get(0).map(|b| b.backend.clone())
        } else {
            self.balance
                .next_available_backend(&backends, peer)
                .map(|b| b.backend)
        }
    }

    pub fn rewrite_host(&self, peer_info: &mut PeerInfo) {
        if let Some(ref h) = self.host_rewriter {
            let (host, port_str, is_default_port) = host_checker(&peer_info.host);

            if let Some(host) = h.rewrite(host) {
                info!("host is rewritten: {}", host);

                let port_str = port_str.to_string();
                peer_info.host.clear();
                // "example.com" + ":" + "8080"
                peer_info.host.push_str(host);
                peer_info.host.push_str(":");
                peer_info.host.push_str(&port_str);
            } else if is_default_port {
                let port_str = port_str.to_string();
                let host = host.to_owned();
                peer_info.host.clear();
                peer_info.host.push_str(&host);
                peer_info.host.push_str(":");
                peer_info.host.push_str(&port_str);
            }
        }
    }
}

/// This checks if a port number is present. If it is not, it will add the port number 80.
/// It will also extract the host for checking and matching host rewriting rules.
fn host_checker(host: &str) -> (&str, &str, bool) {
    match host.rsplit_once(':') {
        Some((host, port_str)) => (host, port_str, false),
        None => (host.as_ref(), "80", true),
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, path::PathBuf};

    use super::*;
    use tokio::{net::TcpListener, sync::Mutex};
    use tracing::debug;
    use tracing_subscriber::EnvFilter;

    use crate::proxy::retry_forever;

    #[test]
    fn test_new_config() {
        let cur_env = std::env::var("RUST_LOG").ok();
        let config = Config::new(Cmd {
            listen_address: Some("8080".to_owned()),
            config: PathBuf::from("assets/testconfig/simple_client.toml"),
        })
        .unwrap();

        assert_eq!(
            config.listen_address.port(),
            8080,
            "config listen address is wrong"
        );
        assert_eq!(config.backends.len(), 1, "config backend len is wrong");
        if let Some(cur_env) = cur_env {
            assert_eq!(
                config.log_level,
                parse_log_level_str(&cur_env),
                "config log is wrong"
            );
        }
    }

    fn empty_host_peer_info() -> PeerInfo {
        PeerInfo {
            host: "".to_owned(),
            addr: "127.0.0.1:8080".parse().unwrap(),
        }
    }

    #[tokio::test]
    async fn test_available_backends() {
        let mut config = Config::new(Cmd {
            listen_address: Some("8080".to_owned()),
            config: PathBuf::from("assets/testconfig/simple_client.toml"),
        })
        .unwrap();

        let config_backend = config.backends.first().unwrap();
        let first_backend = config_backend.backend.clone();
        first_backend.write().await.backend_name = "first_backend".to_owned();

        let file_backend: FileBackendConfig = toml::from_str(&format!(
            "
            address = \"localhost:8080\"
            retry_timeout = 1
            max_retries = 3
            protocol = \"http2\"
            cacert = \"assets/certs/ca.crt\"
            client_key = \"assets/certs/client.key\"
            client_cert = \"assets/certs/client.crt\"
            ",
        ))
        .unwrap();

        let second_backend = Backend::new("second_backend".to_owned(), file_backend).unwrap();

        // make the second backend down
        second_backend.set_status(BackendStatus::Closed);

        let second_config_backend = ConfigBackend {
            metric: Arc::new(second_backend.metric.clone()),
            backend: Arc::new(RwLock::new(second_backend)),
        };

        config.backends.push(second_config_backend);

        let res = config.available_backends().await;
        assert_eq!(res.len(), 1, "incorrect config available backends len");
        let next_backend = config.next_available_backend(&empty_host_peer_info()).await;
        assert_eq!(
            next_backend.unwrap().read().await.backend_name,
            "first_backend".to_owned(),
            "should be a backend here"
        );

        // make the first backend down
        first_backend.read().await.set_status(BackendStatus::Closed);

        let res = config.available_backends().await;
        assert_eq!(res.len(), 0, "should be no backend");
        let next_backend = config.next_available_backend(&empty_host_peer_info()).await;
        assert!(next_backend.is_none(), "should be no backend");
    }

    #[tokio::test]
    async fn test_retry_forever_max_retries() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .try_init();

        let mock_server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = mock_server.local_addr().unwrap().to_string();

        let retries = Arc::new(Mutex::new(0));
        let max_retries = 2;

        let file_backend: FileBackendConfig = toml::from_str(&format!(
            "
            address = \"{}\"
            retry_timeout = 1
            max_retries = {}
            protocol = \"http2\"
            cacert = \"assets/certs/ca.crt\"
            client_key = \"assets/certs/client.key\"
            client_cert = \"assets/certs/client.crt\"
            ",
            server_addr, max_retries
        ))
        .unwrap();

        let backend = Backend::new("backend1".to_owned(), file_backend).unwrap();
        backend.set_status(BackendStatus::Unknown);
        let backend = Arc::new(RwLock::new(backend));

        let retries_server = retries.clone();
        let mock_server = tokio::spawn(async move {
            while let Ok((_, peer)) = mock_server.accept().await {
                debug!("Accepting connection: {}", peer);
                let mut r = retries_server.lock().await;
                *r += 1;
                if *r == max_retries {
                    return;
                }
            }
        });

        let retry = tokio::spawn(async move {
            retry_forever(backend).await;
        });

        tokio::select! {
            _ = retry => {}
            _ = mock_server => {}
        };
        let retries = retries.lock().await;
        assert_eq!(max_retries, *retries, "max_retries is wrong");
    }

    #[tokio::test]
    async fn test_config_host_rewrite() {
        let config = Config::new(Cmd {
            listen_address: Some("8080".to_owned()),
            config: PathBuf::from("assets/testconfig/host_rewrite_client.toml"),
        })
        .unwrap();

        let mut peer = PeerInfo {
            host: "hello.com".to_string(),
            addr: "127.0.0.1:14000".parse().unwrap(),
        };
        config.rewrite_host(&mut peer);
        assert_eq!(peer.host, "127.0.0.1")
    }

    fn build_host_rewrite_config_reader(s: &str) -> std::io::BufReader<Cursor<&str>> {
        std::io::BufReader::new(Cursor::new(s))
    }

    #[test]
    fn test_new_host_rewriter() {
        let host_rewrite_file = r#"
example.com hello.com

hello 127.0.0.1
        "#;
        let host_rewrite_file = build_host_rewrite_config_reader(host_rewrite_file);
        let host_rewriter = HostRewriter::new(host_rewrite_file).unwrap();
        assert!(host_rewriter.is_some(), "parse normal config");

        let host_rewrite_file = r#"

        "#;
        let host_rewrite_file = build_host_rewrite_config_reader(host_rewrite_file);
        let host_rewriter = HostRewriter::new(host_rewrite_file).unwrap();
        assert!(host_rewriter.is_none(), "parse empty config");

        let host_rewrite_file = r#"
aaaaaaaaaaaaaaaaa
        "#;
        let host_rewrite_file = build_host_rewrite_config_reader(host_rewrite_file);
        let host_rewriter = HostRewriter::new(host_rewrite_file).unwrap();
        assert!(host_rewriter.is_none(), "parse invalid config")
    }

    #[test]
    fn test_host_rewriter_rewrite() {
        let host_rewrite_file = r#"
example.com hello.com

hello 127.0.0.1
        "#;
        let host_rewrite_file = build_host_rewrite_config_reader(host_rewrite_file);
        let host_rewriter = HostRewriter::new(host_rewrite_file).unwrap();
        assert!(host_rewriter.is_some(), "parse normal config");

        let host_rewriter = host_rewriter.unwrap();
        let res = host_rewriter.rewrite("example.com");
        assert!(res.is_some(), "example.com should be in the rule");
        let res = res.unwrap();
        assert_eq!(res, "hello.com", "rewrite example.com into hello.com");

        let res = host_rewriter.rewrite("google.com");
        assert!(res.is_none(), "google.com should not be in the rule");
    }

    #[test]
    fn test_host_checker() {
        let (host, port_str, is_default_port) = host_checker("example.com:80");
        assert_eq!(host, "example.com");
        assert_eq!(port_str, "80");
        assert_eq!(is_default_port, false);

        let (host, port_str, is_default_port) = host_checker("example.com");
        assert_eq!(host, "example.com");
        assert_eq!(port_str, "80");
        assert_eq!(is_default_port, true);

        let (host, port_str, is_default_port) = host_checker(":80");
        assert_eq!(host, "");
        assert_eq!(port_str, "80");
        assert_eq!(is_default_port, false);
    }
}
