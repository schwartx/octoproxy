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
    host_modify: Option<PathBuf>,
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

    host_modifier: Option<HostModifier>,

    pub(crate) non_connect_method_client: Client<ProxyConnector<HttpConnector>>,
}

#[derive(Clone)]
pub(crate) struct ConfigBackend {
    pub(crate) backend: Arc<RwLock<Backend>>,
    pub(crate) metric: Arc<Metric>,
}

// modifier
#[derive(Debug, Deserialize)]
struct HostModifier(BTreeMap<String, HostModifySection>);

#[derive(Debug, Deserialize)]
struct HostModifySection {
    rewrite: Option<String>,
    backend: Option<String>,
}

impl TomlFileConfig for HostModifier {}

impl HostModifier {
    fn new(host_file: impl std::io::Read) -> anyhow::Result<Option<Self>> {
        let mut this =
            Self::load_from_read(host_file).context("Failed to parse host modify toml")?;

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
        if let Some(&HostModifySection {
            rewrite: Some(ref r),
            backend: _,
        }) = self.0.get(req_host)
        {
            return Some(r);
        }
        None
    }

    fn route(&self, req_host: &str) -> Option<&str> {
        if let Some(&HostModifySection {
            rewrite: _,
            backend: Some(ref r),
        }) = self.0.get(req_host)
        {
            return Some(r);
        }
        None
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

        let host_modifier = if let Some(host_modifier) = file_config.host_modify {
            let host_modifier =
                std::fs::File::open(host_modifier).context("Failed to open host rewrite file")?;

            HostModifier::new(host_modifier).context("Failed to parse host rewrite config")?
        } else {
            None
        };

        let mut config = Self {
            listen_address,
            backends: Vec::new(),
            balance: Box::new(Frist),
            metric_address,
            log_level,
            host_modifier,
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
            return None;
        }
        if backends.len() == 1 {
            backends
                .get(0)
                .filter(|config_backend| {
                    self.choose_backend(peer.get_host(), config_backend)
                        .is_some()
                })
                .map(|config_backend| config_backend.backend.clone())
        } else {
            self.balance
                .next_available_backend(&backends, peer)
                .filter(|config_backend| {
                    self.choose_backend(peer.get_host(), config_backend)
                        .is_some()
                })
                .map(|b| b.backend)
        }
    }

    pub(crate) fn rewrite_host(&self, peer_info: &PeerInfo) -> String {
        if let Some(ref h) = self.host_modifier {
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

    fn choose_backend<'a>(
        &self,
        req_host: &str,
        config_backend: &'a ConfigBackend,
    ) -> Option<&'a ConfigBackend> {
        if let Some(ref host_modifier) = self.host_modifier {
            if let Some(spec_backend_name) = host_modifier.route(req_host) {
                if spec_backend_name == config_backend.metric.backend_name {
                    info!("choosing backend: {}", spec_backend_name);
                    // give back the backend
                    return Some(config_backend);
                } else {
                    return None;
                }
            }
            // backend name not found, give back the backend
        }
        // no modifier, give back a backend
        Some(config_backend)
    }
}

pub(crate) enum PortStr<'a> {
    Default,
    NonDefault(&'a str),
}

impl PortStr<'_> {
    const DEFUALT_PORT_STR: &'static str = "80";
}

impl ToString for PortStr<'_> {
    fn to_string(&self) -> String {
        match self {
            PortStr::Default => PortStr::DEFUALT_PORT_STR.to_owned(),
            PortStr::NonDefault(port_str) => port_str.to_string(),
        }
    }
}

/// This checks if a port number is present. If it is not, it will add the port number 80.
/// It will also extract the host for checking and matching host rewriting rules.
pub(crate) fn host_checker(host: &str) -> (&str, PortStr) {
    match host.rsplit_once(':') {
        Some((host, port_str)) => (host, PortStr::NonDefault(port_str)),
        None => (host, PortStr::Default),
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
    fn test_new_host_modifier() {
        let host_modifier_file = r#"
["example.com"]
rewrite = "hello.com"
backend = "local1"

[hello]
rewrite = "127.0.0.1"

[world]

        "#;
        let host_modify_file = build_config_reader(host_modifier_file);
        let host_modifier = HostModifier::new(host_modify_file).unwrap();
        assert!(host_modifier.is_some(), "parse normal config");

        let host_modify_file = r#"

        "#;
        let host_modify_file = build_config_reader(host_modify_file);
        let host_modifier = HostModifier::new(host_modify_file).unwrap();
        assert!(host_modifier.is_none(), "parse empty config");

        let host_modify_file = r#"
[hello]

[world]
        "#;
        let host_modify_file = build_config_reader(host_modify_file);
        let host_modifier = HostModifier::new(host_modify_file).unwrap();
        assert!(host_modifier.is_none(), "parse empty sections config");

        let host_modify_file = r#"
aaaaaaaaaaaaaaaaa
        "#;
        let host_modify_file = build_config_reader(host_modify_file);
        let host_modifier = HostModifier::new(host_modify_file);
        assert!(host_modifier.is_err(), "parse invalid config");

        let host_modify_file = r#"
[hello]
[hello]
        "#;
        let host_modify_file = build_config_reader(host_modify_file);
        let host_modifier = HostModifier::new(host_modify_file);
        assert!(host_modifier.is_err(), "parse dup sections config")
    }

    #[test]
    fn test_host_modifier_rewrite() {
        let host_modify_file = r#"
["example.com"]
rewrite = "hello.com"
backend = "local1"

[hello]
rewrite = "127.0.0.1"
        "#;
        let host_modify_file = build_config_reader(host_modify_file);
        let host_modifier = HostModifier::new(host_modify_file).unwrap();
        assert!(host_modifier.is_some(), "parse normal config");

        let host_modifier = host_modifier.unwrap();

        let res = host_modifier.rewrite("example.com");
        assert!(res.is_some(), "example.com should be in the rule");
        let res = res.unwrap();
        assert_eq!(res, "hello.com", "rewrite example.com into hello.com");

        let res = host_modifier.rewrite("google.com");
        assert!(res.is_none(), "google.com should not be in the rule");
    }

    #[test]
    fn test_host_modifier_route() {
        let host_modify_file = r#"
["example.com"]
rewrite = "hello.com"
backend = "local1"

[hello]
rewrite = "127.0.0.1"
        "#;
        let host_modify_file = build_config_reader(host_modify_file);
        let host_modifier = HostModifier::new(host_modify_file).unwrap();
        assert!(host_modifier.is_some(), "parse normal config");

        let host_modifier = host_modifier.unwrap();

        let res = host_modifier.route("example.com");
        assert!(res.is_some(), "example.com should be in the rule");
        let res = res.unwrap();
        assert_eq!(res, "local1", "route example.com to local1");

        let res = host_modifier.route("google.com");
        assert!(res.is_none(), "google.com should not be in the rule");
    }

    #[test]
    fn test_host_checker() {
        let (host, port_str) = host_checker("example.com:80");
        assert_eq!(host, "example.com");
        assert!(matches!(port_str, PortStr::NonDefault("80")));

        let (host, port_str) = host_checker("example.com");
        assert_eq!(host, "example.com");
        assert!(matches!(port_str, PortStr::Default));

        let (host, port_str) = host_checker(":80");
        assert_eq!(host, "");
        assert!(matches!(port_str, PortStr::NonDefault("80")));

        let (host, port_str) = host_checker("example.com:8080");
        assert_eq!(host, "example.com");
        assert!(matches!(port_str, PortStr::NonDefault("8080")));
    }
}
