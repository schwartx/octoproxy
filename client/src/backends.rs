use std::{
    net::SocketAddr,
    ops::ControlFlow,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context};

use hyper::client;
use octoproxy_lib::{
    metric::{BackendProtocol, BackendStatus},
    proxy::TokioExec,
    tls::{build_tls_client_config, QUIC_ALPN},
};
use quinn::{congestion, TransportConfig};
use rustls::{ClientConfig, ServerName};
use serde::Deserialize;
use tokio::io::{copy_bidirectional, AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{
    connector::{BidiStream, H2Client, H2ConnConfig, MixClient, QuicClient, QuicConnConfig},
    metric::Metric,
};

#[derive(Debug, Deserialize)]
pub(crate) struct FileBackendConfig {
    status: Option<BackendStatus>,
    protocol: BackendProtocol,
    address: String,

    #[serde(default = "default_retry_timeout")]
    retry_timeout: u64,
    #[serde(default = "default_max_retries")]
    max_retries: u32,

    cacert: Option<PathBuf>,
    client_key: PathBuf,
    client_cert: PathBuf,
    #[serde(default = "default_h2_file_config")]
    http2: H2FileConfig,
    #[serde(default = "default_quic_file_config")]
    quic: QuicFileConfig,
}

#[derive(Debug, Deserialize, Clone)]
struct QuicFileConfig {
    max_idle_timeout: Option<u64>,
    keep_alive_interval: Option<u64>,
    ipv4_only: Option<bool>,
    use_0rtt: Option<bool>,
    /// ms
    expected_rtt: u32,
    /// bytes/s
    max_stream_bandwidth: u32,
    congestion: QuicCongestionFactory,
}

fn default_quic_file_config() -> QuicFileConfig {
    QuicFileConfig {
        max_idle_timeout: None,
        keep_alive_interval: None,
        ipv4_only: None,
        use_0rtt: None,
        expected_rtt: 100,
        max_stream_bandwidth: 12500 * 1000,
        congestion: QuicCongestionFactory::default(),
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "lowercase")]
enum QuicCongestionFactory {
    #[default]
    Cubic,
    Bbr,
}

impl QuicFileConfig {
    fn init_quic_transport_config(&self) -> TransportConfig {
        let stream_rwnd: u32 = self.max_stream_bandwidth / 1000 * self.expected_rtt;
        let mut transport_config = quinn::TransportConfig::default();
        transport_config.stream_receive_window(stream_rwnd.into());
        transport_config.send_window((8 * stream_rwnd).into());
        // per spec, intentionally distinct from EXPECTED_RTT
        transport_config.initial_rtt(Duration::from_millis((self.expected_rtt * 3).into()));
        transport_config.datagram_receive_buffer_size(Some(stream_rwnd as usize));
        match self.congestion {
            QuicCongestionFactory::Bbr => {
                transport_config
                    .congestion_controller_factory(Arc::new(congestion::BbrConfig::default()));
            }
            QuicCongestionFactory::Cubic => {
                transport_config
                    .congestion_controller_factory(Arc::new(congestion::CubicConfig::default()));
            }
        }

        transport_config
    }
    fn new_builder(
        &self,
        mut tls_config: ClientConfig,
        domain: ServerName,
        address: &str,
    ) -> anyhow::Result<QuicClient> {
        let mut transport_config = self.init_quic_transport_config();

        transport_config.max_idle_timeout(
            self.max_idle_timeout
                .and_then(|x| if x == 0 { None } else { Some(x) })
                .map(|s| quinn::IdleTimeout::try_from(Duration::from_secs(s)))
                .transpose()?,
        );

        transport_config.keep_alive_interval(
            self.keep_alive_interval
                .and_then(|x| if x == 0 { None } else { Some(x) })
                .map(Duration::from_secs),
        );

        tls_config.enable_early_data = true;
        tls_config.alpn_protocols = vec![QUIC_ALPN.into()];
        let mut quinn_config = quinn::ClientConfig::new(Arc::new(tls_config));

        quinn_config.transport_config(Arc::new(transport_config));
        let server_name = match domain {
            ServerName::DnsName(serve_name) => serve_name.as_ref().to_string(),
            ServerName::IpAddress(serve_name) => serve_name.to_string(),
            _ => unreachable!(),
        };

        let use_0rtt = self.use_0rtt.unwrap_or(true);
        let ipv4_only = self.ipv4_only.unwrap_or(false);

        let quic_config = QuicConnConfig::new(
            quinn_config,
            server_name,
            address.to_owned(),
            ipv4_only,
            use_0rtt,
        );
        let client = QuicClient::new(quic_config);
        Ok(client)
    }
}

#[derive(Debug, Deserialize, Clone)]
struct H2FileConfig {
    /// Sets an interval for HTTP2 Ping frames should be sent to keep a connection alive.
    keep_alive_interval: u64,
    /// Sets a timeout for receiving an acknowledgement of the keep-alive ping.
    /// If the ping is not acknowledged within the timeout, the connection will be closed
    keep_alive_timeout: u64,

    /// Sets whether HTTP2 keep-alive should apply while the connection is idle.
    /// If disabled, keep-alive pings are only sent while there are open request/responses streams.
    /// If enabled, pings are also sent when no streams are active.
    keep_alive_while_idle: bool,
}

impl H2FileConfig {
    fn new_builder(
        &self,
        tls_config: ClientConfig,
        domain: ServerName,
        address: &str,
    ) -> anyhow::Result<H2Client> {
        let h2builder = client::conn::http2::Builder::new(TokioExec)
            .keep_alive_interval(Duration::from_secs(self.keep_alive_interval))
            .keep_alive_timeout(Duration::from_secs(self.keep_alive_timeout))
            .keep_alive_while_idle(self.keep_alive_while_idle)
            .to_owned();

        let tls_config = Arc::new(tls_config);
        let h2config = H2ConnConfig::new(
            h2builder,
            address.to_owned(),
            domain,
            tls_config,
            Duration::from_secs(5),
        )?;
        let client = H2Client::new(h2config);
        Ok(client)
    }
}

fn default_h2_file_config() -> H2FileConfig {
    H2FileConfig {
        keep_alive_interval: 10,
        keep_alive_timeout: 20,
        keep_alive_while_idle: false,
    }
}

fn default_retry_timeout() -> u64 {
    10
}

fn default_max_retries() -> u32 {
    10
}

impl FileBackendConfig {
    fn build_tls_client_config(&self) -> anyhow::Result<ClientConfig> {
        let tls_config =
            build_tls_client_config(&self.cacert, &self.client_cert, &self.client_key)?;

        Ok(tls_config)
    }

    fn build_client(&self) -> anyhow::Result<(MixClient, String)> {
        let domain = parse_domain(&self.address)?;
        let tls_config = self
            .build_tls_client_config()
            .context("Could not build tls config from backend file config")?;

        let client = match self.protocol {
            BackendProtocol::HTTP2 => {
                let client = self
                    .http2
                    .new_builder(tls_config, domain.clone(), &self.address)?;
                MixClient::H2(client)
            }
            BackendProtocol::Quic => {
                let client = self
                    .quic
                    .new_builder(tls_config, domain.clone(), &self.address)?;
                MixClient::Quic(client)
            }
        };

        let domain = format!("{:?}", domain);
        Ok((client, domain))
    }
}

fn parse_domain(address: &str) -> anyhow::Result<ServerName> {
    let domain = format!("http://{}", address)
        .parse::<http::Uri>()
        .with_context(|| format!("cannot parse authority, got wrong address: {}", address))?;
    let domain = domain
        .host()
        .ok_or(anyhow!("cannot parse host, got wrong address: {}", address))?;
    let domain = ServerName::try_from(domain)
        .with_context(|| format!("cannot parse server name, got wrong address: {}", address))?;
    Ok(domain)
}

pub(crate) struct Backend {
    pub(crate) retry_timeout: u64,
    pub(crate) max_retries: u32,
    pub(crate) metric: Metric,

    client: Arc<MixClient>,
    retrying: AtomicBool,
    cancellation_token: CancellationToken,

    file_backend_config: FileBackendConfig,
}

impl Backend {
    pub(crate) fn new(
        backend_name: String,
        file_backend_config: FileBackendConfig,
    ) -> anyhow::Result<Self> {
        let retry_timeout = file_backend_config.retry_timeout;
        let max_retries = file_backend_config.max_retries;

        let (client, domain) = file_backend_config
            .build_client()
            .context("Could not build client from backend file")?;

        let status = file_backend_config.status.unwrap_or(BackendStatus::Normal);
        let metric = Metric::new(
            &backend_name,
            &file_backend_config.address,
            &domain,
            status,
            file_backend_config.protocol,
        );
        let cancellation_token = CancellationToken::new();

        let client = Arc::new(client);
        Ok(Self {
            retrying: AtomicBool::new(false),
            cancellation_token,
            client,
            metric,
            retry_timeout,
            max_retries,
            file_backend_config,
        })
    }

    pub(crate) async fn switch_protocol(&mut self) -> anyhow::Result<()> {
        let next_proto = match &self.file_backend_config.protocol {
            BackendProtocol::HTTP2 => BackendProtocol::Quic,
            BackendProtocol::Quic => BackendProtocol::HTTP2,
        };
        self.file_backend_config.protocol = next_proto;

        let (client, _) = self
            .file_backend_config
            .build_client()
            .context("Could not build client from backend file")?;
        self.client = Arc::new(client);
        self.renew_cancellation_token();
        self.metric.set_protocol(next_proto);
        self.reset_client().await;
        Ok(())
    }

    /// force reset backend client
    pub(crate) async fn reset_client(&self) {
        self.client.reset_client().await;
        self.metric.reset();
    }

    pub(crate) fn set_status(&self, status: BackendStatus) {
        self.metric.set_status(status)
    }

    pub(crate) async fn retry_connect(&self) -> ControlFlow<(), ()> {
        match self.get_status() {
            BackendStatus::Normal => {
                // exit this function, normal status doesn't need to retry
                ControlFlow::Break(())
            }
            BackendStatus::ForceClosed => {
                // dont retry if status is manually close
                ControlFlow::Break(())
            }
            BackendStatus::Closed | BackendStatus::Unknown => {
                if self.retrying.load(Ordering::Acquire) {
                    // exit this function, this backend is retrying
                    return ControlFlow::Break(());
                };
                self.retrying.store(true, Ordering::Release);
                debug!("begin retrying");

                self.client.reset_client().await;

                match self.try_connect(None).await {
                    Ok(_) => {
                        // good connection
                        self.set_status(BackendStatus::Normal);
                        debug!("backend: {} connection is back.", self.get_backend_name());

                        self.retrying.store(false, Ordering::Release);
                        // exit this function
                        ControlFlow::Break(())
                    }
                    Err(err) => {
                        if self.get_status() != BackendStatus::Closed {
                            self.set_status(BackendStatus::Closed);
                        }
                        debug!(
                            "backend: {} fail to connect: {}",
                            self.get_backend_name(),
                            err
                        );

                        self.retrying.store(false, Ordering::Release);
                        ControlFlow::Continue(())
                    }
                }
            }
        }
    }

    pub(crate) async fn try_connect(&self, host: Option<String>) -> anyhow::Result<BidiStream> {
        debug!("try connecting backend id: {}", self.get_backend_name());

        let client = self.client.clone();
        let cancellation_token = self.cancellation_token.clone();

        let fut = async move {
            let stream = client.new_stream_fut(host).await?.await?;
            anyhow::Ok(stream)
        };

        let stream = tokio::select! {
            stream = fut => {
                Some(stream)
            }
            _ = cancellation_token.cancelled() => {
                None
            }
        }
        .ok_or(anyhow!("try_connect is cancelled"))??;

        Ok(stream)
    }

    /// this function connects to the remote server, and performs the tls handshake
    /// record the summary to its metric
    pub(crate) async fn transfer<I>(
        &self,
        outbound: BidiStream,
        mut inbound: I,
        connection_time: Duration,
        peer_addr: SocketAddr,
    ) -> anyhow::Result<()>
    where
        I: AsyncRead + AsyncWrite + Unpin,
    {
        let transmission_time = Instant::now();

        let cancellation_token = self.cancellation_token.clone();

        let copy_fut = async move {
            match outbound {
                BidiStream::H2(mut outbound) => copy_bidirectional(&mut inbound, &mut outbound)
                    .await
                    .map_err(|x| anyhow!("{}", x)),
                BidiStream::Quic(mut outbound) => copy_bidirectional(&mut inbound, &mut outbound)
                    .await
                    .map_err(|x| anyhow!("{}", x)),
                BidiStream::Alive => unreachable!(),
            }
        };

        let count = tokio::select! {
            count = copy_fut => {
                count
            }
            _ = cancellation_token.cancelled() => {
                bail!("tunnel is cancelled")
            }
        };

        let (from_client_bytes, from_server_bytes) = match count {
            Ok((from_client, from_server)) => (from_client, from_server),
            Err(e) => {
                bail!("fail to transfer: {}", e);
            }
        };

        let transmission_time = transmission_time.elapsed();
        debug!(
            "connection time: {}ms",
            connection_time.as_secs_f64() * 1000.0
        );
        debug!(
            "transmission time: {}ms",
            transmission_time.as_secs_f64() * 1000.0
        );

        self.metric.summary(
            connection_time,
            transmission_time,
            from_client_bytes,
            from_server_bytes,
            peer_addr,
        );

        Ok(())
    }

    pub(crate) fn get_status(&self) -> BackendStatus {
        self.metric.get_status()
    }

    pub(crate) fn get_backend_name(&self) -> &str {
        &self.metric.backend_name
    }

    pub(crate) fn cancel(&self) {
        self.cancellation_token.cancel()
    }

    pub(crate) fn renew_cancellation_token(&mut self) {
        // cancel all jobs
        self.cancellation_token = CancellationToken::new();
        // reset retrying status
        self.retrying.store(false, Ordering::Release);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn new_test_backend() -> anyhow::Result<Backend> {
        let file_backend_config = FileBackendConfig {
            status: None,
            address: "localhost:8443".to_owned(),
            retry_timeout: 1,
            max_retries: 0,
            cacert: Some(PathBuf::from("assets/certs/ca.crt")),
            client_key: PathBuf::from("assets/certs/client.key"),
            client_cert: PathBuf::from("assets/certs/client.crt"),
            http2: H2FileConfig {
                keep_alive_interval: 1,
                keep_alive_timeout: 1,
                keep_alive_while_idle: true,
            },
            quic: default_quic_file_config(),
            protocol: BackendProtocol::HTTP2,
        };

        Backend::new("test0".to_owned(), file_backend_config)
    }

    #[test]
    fn test_new_backend() {
        new_test_backend().unwrap();
    }

    #[test]
    fn test_status() {
        let backend = new_test_backend().unwrap();
        assert_eq!(backend.get_status(), BackendStatus::Normal);
        backend.set_status(BackendStatus::Closed);
        assert_eq!(backend.get_status(), BackendStatus::Closed);
        backend.set_status(BackendStatus::ForceClosed);
        assert_eq!(backend.get_status(), BackendStatus::ForceClosed);
    }

    #[test]
    fn test_parse_domain() {
        let res = parse_domain("example.com:80");
        assert_eq!(
            res.ok().unwrap(),
            ServerName::try_from("example.com").unwrap()
        );

        let res = parse_domain("example.com");
        assert_eq!(
            res.ok().unwrap(),
            ServerName::try_from("example.com").unwrap()
        );

        let res = parse_domain("");
        assert!(res.is_err());
    }
}
