use anyhow::Context;
use futures::stream::{self, StreamExt};
use hashring::HashRing;
use hyper::client::HttpConnector;
use hyper::Client;
use octoproxy_lib::metric::BackendStatus;
use octoproxy_lib::proxy_client::ProxyConnector;
use tracing::metadata::LevelFilter;

use std::collections::BTreeMap;
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
    host_rule: Option<PathBuf>,
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

    host_rule: Option<HostRuler>,

    pub(crate) non_connect_method_client: Client<ProxyConnector<HttpConnector>>,
}

#[derive(Clone)]
pub(crate) struct ConfigBackend {
    pub(crate) backend: Arc<RwLock<Backend>>,
    pub(crate) metric: Arc<Metric>,
}

#[derive(Debug, Deserialize)]
struct HostRuler(BTreeMap<String, HostRuleSection>);

#[derive(Debug, Deserialize)]
struct HostRuleSection {
    rewrite: Option<String>,
    backend: Option<String>,
}

impl TomlFileConfig for HostRuler {}

impl HostRuler {
    fn new(host_file: impl std::io::Read) -> anyhow::Result<Option<Self>> {
        let mut this = Self::load_from_read(host_file).context("Failed to parse host rule toml")?;

        let mut map = BTreeMap::new();
        for (sec_name, sec) in this.0 {
            if sec.rewrite.is_none() && sec.backend.is_none() {
                continue;
            }
            map.insert(sec_name, sec);
        }
        this.0 = map;

        if this.0.is_empty() {
            return Ok(None);
        }
        Ok(Some(this))
    }

    fn rewrite(&self, req_host: &str) -> Option<&str> {
        if let Some(&HostRuleSection {
            rewrite: Some(ref r),
            backend: _,
        }) = self.0.get(req_host)
        {
            return Some(r);
        }
        None
    }

    fn route(&self, req_host: &str) -> Option<&str> {
        if let Some(&HostRuleSection {
            rewrite: _,
            backend: Some(ref r),
        }) = self.0.get(req_host)
        {
            return Some(r);
        }
        None
    }

    fn choose_backend(
        &self,
        peer: &PeerInfo,
        backends: std::slice::Iter<'_, ConfigBackend>,
    ) -> AvailableBackend {
        if let Some(spec_backend_name) = self.route(peer.get_hostname()) {
            for b in backends {
                if b.metric.backend_name == spec_backend_name {
                    return AvailableBackend::GotBackend(b.backend.clone());
                }
            }
            // not found
            return AvailableBackend::Block;
        }
        // ignore
        AvailableBackend::NoBackend
    }
}

pub(crate) enum AvailableBackend {
    Block,
    NoBackend,
    GotBackend(Arc<RwLock<Backend>>),
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

        let host_ruler = if let Some(host_ruler) = file_config.host_rule {
            let host_ruler =
                std::fs::File::open(host_ruler).context("Failed to open host rewrite file")?;

            HostRuler::new(host_ruler).context("Failed to parse host rewrite config")?
        } else {
            None
        };

        let mut config = Self {
            listen_address,
            backends: Vec::new(),
            balance: Box::new(Frist),
            metric_address,
            log_level,
            host_rule: host_ruler,
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

    /// - if `config` has `host_ruler`, use it to find the corresponding backend:
    ///   - If not found, return None
    ///   - If found, return the backend if backend's status is normal
    /// - If `config` does not have `host_ruler`, use load balancing algorithm.
    pub(crate) async fn next_available_backend(&self, peer: &PeerInfo) -> AvailableBackend {
        if let Some(ref host_ruler) = self.host_rule {
            match host_ruler.choose_backend(peer, self.backends.iter()) {
                AvailableBackend::NoBackend => {}
                AvailableBackend::GotBackend(b) => {
                    if b.read().await.get_status() == BackendStatus::Normal {
                        return AvailableBackend::GotBackend(b.clone());
                    } else {
                        return AvailableBackend::Block;
                    }
                }
                AvailableBackend::Block => {
                    return AvailableBackend::Block;
                }
            };
        };

        let backends = self.available_backends().await;
        if backends.is_empty() {
            return AvailableBackend::NoBackend;
        }

        if backends.len() == 1 {
            AvailableBackend::GotBackend(backends.get(0).unwrap().backend.clone())
        } else {
            match self.balance.next_available_backend(&backends, peer) {
                Some(config_backend) => AvailableBackend::GotBackend(config_backend.backend),
                None => AvailableBackend::NoBackend,
            }
        }
    }

    pub(crate) fn rewrite_host(&self, peer_info: &PeerInfo) -> String {
        if let Some(ref h) = self.host_rule {
            if let Some(host) = h.rewrite(peer_info.get_hostname()) {
                info!("host is rewritten: {}", host);

                let mut host = host.to_owned();
                host.push(':');
                host.push_str(&peer_info.get_port_str());
                return host;
            }
        }

        peer_info.get_valid_host()
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
        PeerInfo::new("".to_owned(), "127.0.0.1:8080".parse().unwrap())
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
        let next_backend = match next_backend {
            AvailableBackend::GotBackend(b) => b,
            _ => unreachable!("Should not be other value"),
        };
        assert_eq!(
            next_backend.read().await.backend_name,
            "first_backend".to_owned(),
            "should be a backend here"
        );

        // make the first backend down
        first_backend.read().await.set_status(BackendStatus::Closed);

        let res = config.available_backends().await;
        assert_eq!(res.len(), 0, "should be no backend");
        let next_backend = config.next_available_backend(&empty_host_peer_info()).await;

        assert!(
            matches!(next_backend, AvailableBackend::NoBackend),
            "should be no backend"
        );
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
    async fn test_config_host_rule() {
        let config = Config::new(Cmd {
            listen_address: Some("8080".to_owned()),
            config: PathBuf::from("assets/testconfig/host_rule_client.toml"),
        })
        .unwrap();

        let peer = PeerInfo::new(
            "hello.com:8080".to_string(),
            "127.0.0.1:14000".parse().unwrap(),
        );

        let new_host = config.rewrite_host(&peer);
        assert_eq!(new_host, "127.0.0.1:8080")
    }

    fn build_config_reader(s: &str) -> std::io::BufReader<Cursor<&str>> {
        std::io::BufReader::new(Cursor::new(s))
    }

    #[test]
    fn test_new_host_ruler() {
        let host_ruler_file = r#"
["example.com"]
rewrite = "hello.com"
backend = "local1"

[hello]
rewrite = "127.0.0.1"

[world]

        "#;
        let host_ruler_file = build_config_reader(host_ruler_file);
        let host_ruler = HostRuler::new(host_ruler_file).unwrap();
        assert!(host_ruler.is_some(), "parse normal config");

        let host_ruler_file = r#"

        "#;
        let host_ruler_file = build_config_reader(host_ruler_file);
        let host_ruler = HostRuler::new(host_ruler_file).unwrap();
        assert!(host_ruler.is_none(), "parse empty config");

        let host_ruler_file = r#"
[hello]

[world]
        "#;
        let host_ruler_file = build_config_reader(host_ruler_file);
        let host_ruler = HostRuler::new(host_ruler_file).unwrap();
        assert!(host_ruler.is_none(), "parse empty sections config");

        let host_ruler_file = r#"
aaaaaaaaaaaaaaaaa
        "#;
        let host_ruler_file = build_config_reader(host_ruler_file);
        let host_ruler = HostRuler::new(host_ruler_file);
        assert!(host_ruler.is_err(), "parse invalid config");

        let host_ruler_file = r#"
[hello]
[hello]
        "#;
        let host_ruler_file = build_config_reader(host_ruler_file);
        let host_ruler = HostRuler::new(host_ruler_file);
        assert!(host_ruler.is_err(), "parse dup sections config")
    }

    #[test]
    fn test_host_ruler_rewrite() {
        let host_ruler_file = r#"
["example.com"]
rewrite = "hello.com"
backend = "local1"

[hello]
rewrite = "127.0.0.1"
        "#;
        let host_ruler_file = build_config_reader(host_ruler_file);
        let host_ruler = HostRuler::new(host_ruler_file).unwrap();
        assert!(host_ruler.is_some(), "parse normal config");

        let host_ruler = host_ruler.unwrap();

        let res = host_ruler.rewrite("example.com");
        assert!(res.is_some(), "example.com should be in the rule");
        let res = res.unwrap();
        assert_eq!(res, "hello.com", "rewrite example.com into hello.com");

        let res = host_ruler.rewrite("google.com");
        assert!(res.is_none(), "google.com should not be in the rule");
    }

    #[test]
    fn test_host_ruler_route() {
        let host_ruler_file = r#"
["example.com"]
rewrite = "hello.com"
backend = "local1"

[hello]
rewrite = "127.0.0.1"
        "#;
        let host_ruler_file = build_config_reader(host_ruler_file);
        let host_ruler = HostRuler::new(host_ruler_file).unwrap();
        assert!(host_ruler.is_some(), "parse normal config");

        let host_ruler = host_ruler.unwrap();

        let res = host_ruler.route("example.com");
        assert!(res.is_some(), "example.com should be in the rule");
        let res = res.unwrap();
        assert_eq!(res, "local1", "route example.com to local1");

        let res = host_ruler.route("google.com");
        assert!(res.is_none(), "google.com should not be in the rule");
    }
}
