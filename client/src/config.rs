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
use crate::metric::{Metric, PeerBackendRule, PeerInfo};
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

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct HostRuleSection {
    pub(crate) rewrite: Option<String>,
    pub(crate) backend: Option<String>,
    #[serde(default = "default_is_not_direct")]
    pub(crate) direct: bool,
}

fn default_is_not_direct() -> bool {
    false
}

#[derive(Debug)]
pub(crate) enum HostRules {
    Unknown,
    NoRule,
    GotRule(HostRuleSection),
}

impl TomlFileConfig for HostRuler {}

impl HostRuler {
    fn new(host_file: impl std::io::Read) -> anyhow::Result<Option<Self>> {
        let mut this = Self::load_from_read(host_file).context("Failed to parse host rule toml")?;

        let mut map = BTreeMap::new();
        for (sec_name, sec) in this.0 {
            if sec.rewrite.is_none() && sec.backend.is_none() && !sec.direct {
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

    fn find_rule(&self, peer: &mut PeerInfo) {
        if matches!(peer.get_rule(), HostRules::NoRule) {
            return;
        }

        let rule = self.0.get(peer.get_hostname());
        peer.set_rule(match rule {
            Some(rule) => HostRules::GotRule(rule.clone()),
            None => HostRules::NoRule,
        })
    }
}

pub(crate) enum AvailableBackend {
    Direct,
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

        let balance = populate_load_balance(file_config.balance, file_config.backends.len());
        let backends =
            populate_backends(file_config.backends).context("Could not setup backends")?;

        Ok(Self {
            listen_address,
            balance,
            backends,
            metric_address,
            log_level,
            host_rule: host_ruler,
            non_connect_method_client,
        })
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
        match peer.get_backend_name_by_rule() {
            PeerBackendRule::NoRule => { /* ignore */ }
            PeerBackendRule::GotBackend(spec_backend_name) => {
                for config_backend in self.backends.iter() {
                    if config_backend.metric.backend_name == spec_backend_name {
                        if config_backend.backend.read().await.get_status() == BackendStatus::Normal
                        {
                            return AvailableBackend::GotBackend(config_backend.backend.clone());
                        } else {
                            return AvailableBackend::Block;
                        }
                    }
                }
                // not found
                return AvailableBackend::Block;
            }
            PeerBackendRule::Direct => {
                return AvailableBackend::Direct;
            }
        };

        let backends = self.available_backends().await;
        if backends.is_empty() {
            return AvailableBackend::NoBackend;
        }

        if backends.len() == 1 {
            AvailableBackend::GotBackend(backends.first().unwrap().backend.clone())
        } else {
            match self.balance.next_available_backend(&backends, peer) {
                Some(config_backend) => AvailableBackend::GotBackend(config_backend.backend),
                None => AvailableBackend::NoBackend,
            }
        }
    }

    pub(crate) fn find_rule_for_peer(&self, peer: &mut PeerInfo) {
        if let Some(ref h) = self.host_rule {
            h.find_rule(peer)
        }
    }
}

fn populate_load_balance(
    balance: Balance,
    backends_len: usize,
) -> Box<dyn LoadBalancingAlgorithm<ConfigBackend> + Send + Sync + 'static> {
    info!("load balance algorithm: {:?}", balance);
    match balance {
        Balance::RoundRobin => Box::new(RoundRobin::new()),
        Balance::Random => Box::new(Random {}),
        Balance::LeastLoadedTime => Box::new(LeastLoadedTime {}),
        Balance::UriHash => {
            let mut ring: HashRing<usize> = HashRing::new();
            for backend_id in 0..backends_len {
                ring.add(backend_id)
            }
            Box::new(UriHash::new(ring))
        }
        Balance::Frist => Box::new(Frist),
    }
}
fn populate_backends(
    mut file_backend_configs: HashMap<String, FileBackendConfig>,
) -> anyhow::Result<Vec<ConfigBackend>> {
    let mut backends = Vec::new();
    for (file_backend_name, file_backend_config) in file_backend_configs.drain() {
        let backend = Backend::new(file_backend_name, file_backend_config)
            .context("Fail to initialize backend")?;

        backends.push(ConfigBackend {
            metric: Arc::new(backend.metric.clone()),
            backend: Arc::new(RwLock::new(backend)),
        });
    }
    Ok(backends)
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        net::{IpAddr, Ipv4Addr},
        path::PathBuf,
    };

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
        let next_backend = config
            .next_available_backend(&mut empty_host_peer_info())
            .await;
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
        let next_backend = config
            .next_available_backend(&mut empty_host_peer_info())
            .await;

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
[hello]
direct = true

[world]
        "#;
        let host_ruler_file = build_config_reader(host_ruler_file);
        let host_ruler = HostRuler::new(host_ruler_file).unwrap();
        assert!(host_ruler.is_some(), "parse direct sections config");

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
    fn test_new_host_ruler_find_rule() {
        let eg_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let host_ruler_file = r#"
["example.com"]
rewrite = "hello.com"
backend = "local1"

[hello]
rewrite = "127.0.0.1"

[world]

[fedora.local10]
direct = true

[fedora.local101]
direct = false

        "#;
        let host_ruler_file = build_config_reader(host_ruler_file);
        let host_ruler = HostRuler::new(host_ruler_file).unwrap();

        let host_ruler = host_ruler.unwrap();

        let mut peer = PeerInfo::new("example.com:8080".to_owned(), eg_addr);
        assert!(matches!(peer.get_rule(), HostRules::Unknown));
        host_ruler.find_rule(&mut peer);
        assert!(matches!(
            peer.get_rule(),
            HostRules::GotRule(HostRuleSection {
                rewrite: Some(_),
                backend: Some(_),
                direct: _,
            })
        ));

        let mut peer = PeerInfo::new("hello:8080".to_owned(), eg_addr);
        assert!(matches!(peer.get_rule(), HostRules::Unknown));
        host_ruler.find_rule(&mut peer);
        assert!(matches!(
            peer.get_rule(),
            HostRules::GotRule(HostRuleSection {
                rewrite: Some(_),
                backend: None,
                direct: _,
            })
        ));

        let mut peer = PeerInfo::new("google.com:8080".to_owned(), eg_addr);
        assert!(matches!(peer.get_rule(), HostRules::Unknown));
        host_ruler.find_rule(&mut peer);
        assert!(matches!(peer.get_rule(), HostRules::NoRule));
    }
}
