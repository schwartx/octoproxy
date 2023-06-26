use anyhow::{bail, Context, Result};
use futures::{ready, Future, FutureExt, TryFutureExt};
use http::{Request, Uri};
use hyper::client::conn::http2;
use hyper::upgrade::Upgraded;
use hyper::Body;
use octoproxy_lib::proxy::{ConnectHeadBuf, QuicBidiStream};
use rustls::{ClientConfig, ServerName};
use std::pin::Pin;
use std::{sync::Arc, time::Duration};
use tokio::io::AsyncWriteExt;
use tokio::macros::support::poll_fn;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio::{net::TcpStream, sync::Mutex};
use tokio_rustls::TlsConnector;
use tracing::debug;

use anyhow::anyhow;

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::net::lookup_host;

type H2SendRequest = http2::SendRequest<Body>;

pub(crate) enum MixClient {
    H2(H2Client),
    Quic(QuicClient),
}

impl MixClient {
    pub(crate) async fn reset_client(&self) {
        match &self {
            MixClient::H2(client) => client.reset_client().await,
            MixClient::Quic(client) => client.reset_client().await,
        }
    }

    pub(crate) async fn new_stream_fut(
        &self,
        host: Option<ConnectHeadBuf>,
    ) -> Result<PinBoxFut<Result<BidiStream>>> {
        match &self {
            MixClient::H2(client) => client.new_stream_fut(host).await,
            MixClient::Quic(client) => client.new_stream_fut(host).await,
        }
    }
}

pub(crate) type H2Client = Client<H2ConnConfig, H2SendRequest>;

type QuicSendRequest = quinn::Connection;

pub(crate) type QuicClient = Client<QuicConnConfig, QuicSendRequest>;

pub(crate) enum BidiStream {
    H2(Upgraded),
    Quic(QuicBidiStream),
    Alive,
}

type PinBoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) trait ConnConfig<SR> {
    /// Establishing a connection, whether it be HTTP/2 or HTTP/3.
    /// A single connection possesses the capability of multiplexing
    fn new_connection(&self) -> PinBoxFut<Result<(JoinHandle<()>, SR)>>;

    /// Constructing a corresponding future without executing
    /// Initiating a stream
    fn new_stream(&self, client: SR, host: Option<ConnectHeadBuf>) -> PinBoxFut<Result<BidiStream>>
    where
        SR: Clone;
}

pub(crate) struct QuicConnConfig {
    use_0rtt: bool,
    quinn_config: quinn::ClientConfig,
    server_name: String,
    remote_address: String,
    last_addrs: parking_lot::Mutex<Option<(SocketAddr, SocketAddr)>>,
    ipv4_only: bool,
}

impl QuicConnConfig {
    pub(crate) fn new(
        quinn_config: quinn::ClientConfig,
        server_name: String,
        remote_address: String,
        ipv4_only: bool,
        use_0rtt: bool,
    ) -> Self {
        Self {
            quinn_config,
            use_0rtt,
            server_name,
            remote_address,
            ipv4_only,
            last_addrs: None.into(),
        }
    }

    async fn connect_addr(&self) -> anyhow::Result<(quinn::Connection, quinn::Endpoint)> {
        let mut last_err = None;

        for remote_addr in lookup_host(&self.remote_address).await? {
            let endpoint_addr = match remote_addr {
                SocketAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
                SocketAddr::V6(_) => {
                    if self.ipv4_only {
                        continue;
                    }
                    Ipv6Addr::UNSPECIFIED.into()
                }
            };

            let endpoint_addr = SocketAddr::new(endpoint_addr, 0);

            match self.try_connect(remote_addr, endpoint_addr).await {
                Ok(quinn_conn_endpoint) => {
                    let mut last_addrs = self.last_addrs.lock();
                    *last_addrs = Some((remote_addr, endpoint_addr));

                    return Ok(quinn_conn_endpoint);
                }
                Err(err) => last_err = Some(err),
            };
        }

        Err(last_err.unwrap_or_else(|| anyhow!("could not resolve to any address")))
    }

    async fn try_connect(
        &self,
        remote_addr: SocketAddr,
        endpoint_addr: SocketAddr,
    ) -> anyhow::Result<(quinn::Connection, quinn::Endpoint)> {
        let mut client_endpoint = quinn::Endpoint::client(endpoint_addr)?;
        client_endpoint.set_default_client_config(self.quinn_config.clone());

        let connecting = client_endpoint.connect(remote_addr, &self.server_name)?;
        let quinn_conn = if self.use_0rtt {
            match connecting.into_0rtt() {
                Ok((conn, _)) => conn,
                Err(conn) => conn.await?,
            }
        } else {
            connecting.await?
        };

        Ok((quinn_conn, client_endpoint))
    }
}

impl ConnConfig<QuicSendRequest> for QuicConnConfig {
    fn new_connection(&self) -> PinBoxFut<Result<(JoinHandle<()>, QuicSendRequest)>> {
        async move {
            let last_addrs = { *self.last_addrs.lock() };

            let (quinn_conn, client_endpoint) = match last_addrs {
                Some((remote_addr, endpoint_addr)) => {
                    debug!(
                        "Using last addrs: remote: {:?}, endpoint: {:?}",
                        remote_addr, endpoint_addr
                    );
                    self.try_connect(remote_addr, endpoint_addr).await?
                }
                None => self.connect_addr().await?,
            };

            let conn = tokio::spawn(async move { client_endpoint.wait_idle().await });

            Ok((conn, quinn_conn))
        }
        .boxed()
    }

    fn new_stream(
        &self,
        quinn_conn: QuicSendRequest,
        host: Option<ConnectHeadBuf>,
    ) -> PinBoxFut<Result<BidiStream>> {
        async move {
            match host {
                Some(host) => {
                    let (mut send, mut recv) = quinn_conn
                        .open_bi()
                        .await
                        .map_err(|e| anyhow!("failed to open stream: {}", e))?;

                    let mut state = QuicStreamState::Init;
                    poll_fn(|cx| loop {
                        match &state {
                            QuicStreamState::Init => {
                                state = QuicStreamState::Write(host.clone());
                            }
                            QuicStreamState::Write(buf) => {
                                let mut w = Box::pin((send).write_all(buf.as_ref()));
                                ready!(w.as_mut().poll(cx))?;
                                drop(w);
                                state = QuicStreamState::Read;
                            }
                            QuicStreamState::Read => {
                                let mut r = Box::pin((recv).read_chunk(2, true));
                                ready!(r.as_mut().poll(cx))?;
                                return std::task::Poll::Ready(anyhow::Ok(()));
                            }
                        };
                    })
                    .await?;

                    Ok(BidiStream::Quic(QuicBidiStream { send, recv }))
                }
                None => Ok(BidiStream::Alive),
            }
        }
        .boxed()
    }
}

enum QuicStreamState {
    Init,
    Write(ConnectHeadBuf),
    Read,
}

pub(crate) struct H2ConnConfig {
    tls: TlsConnector,
    domain: ServerName,
    remote_address: String,
    remote_uri: Uri,
    connect_time: Duration,
    builder: http2::Builder,
}

impl H2ConnConfig {
    pub(crate) fn new(
        builder: http2::Builder,
        remote_address: String,
        domain: ServerName,
        tls_config: Arc<ClientConfig>,
        connect_time: Duration,
    ) -> Result<Self> {
        let remote_uri = remote_address
            .parse()
            .context("Could not parse remote address into uri")?;

        Ok(Self {
            builder,
            tls: TlsConnector::from(tls_config),
            domain,
            remote_address,
            remote_uri,
            connect_time,
        })
    }
}

impl ConnConfig<H2SendRequest> for H2ConnConfig {
    fn new_stream(
        &self,
        mut client: H2SendRequest,
        host: Option<ConnectHeadBuf>,
    ) -> PinBoxFut<Result<BidiStream>> {
        async move {
            let req = Request::connect(&self.remote_uri)
                .body(hyper::Body::empty())
                .context("failed to build CONNECT method reqest")?;

            poll_fn(|cx| client.poll_ready(cx)).await?;

            let mut stream = client
                .send_request(req)
                .and_then(hyper::upgrade::on)
                .await
                .context("failed to upgrade CONNECT request")?;

            match host {
                Some(host) => {
                    let buf = &host.as_ref();
                    stream.write_all(buf).await?;

                    Ok(BidiStream::H2(stream))
                }
                None => Ok(BidiStream::Alive),
            }
        }
        .boxed()
    }

    fn new_connection(&self) -> PinBoxFut<Result<(JoinHandle<()>, H2SendRequest)>> {
        async move {
            let conn = TcpStream::connect(&self.remote_address);

            let outbound = match timeout(self.connect_time, conn).await {
                Ok(Ok(outbound)) => outbound,
                Ok(Err(e)) => {
                    bail!("Fail to connect remote: {:?}", e);
                }
                Err(e) => {
                    bail!("connect remote timeout: {:?}", e);
                }
            };

            outbound.set_nodelay(true)?;
            let outbound = self
                .tls
                .connect(self.domain.to_owned(), outbound)
                .await
                .context("Could not perform tls handshake")?;

            let (client, conn) = self
                .builder
                .clone()
                .handshake::<_, Body>(outbound)
                .await
                .context("http2 handshake failure")?;

            let conn = tokio::spawn(async move {
                if let Err(e) = conn.await {
                    debug!("connection error: {:?}", e);
                }
            });

            Ok((conn, client))
        }
        .boxed()
    }
}

pub(crate) struct Client<C, SR: Clone> {
    inner: Mutex<ClientState<SR>>,
    config: C,
}

impl<C, SR> Client<C, SR>
where
    C: ConnConfig<SR>,
    SR: Clone,
{
    pub(crate) fn new(config: C) -> Self {
        Self {
            inner: Mutex::new(ClientState::NotConnected),
            config,
        }
    }

    async fn reset_client(&self) {
        let mut inner = self.inner.lock().await;

        match *inner {
            ClientState::Connected {
                ref mut conn,
                client: _,
            } => {
                conn.abort();
            }
            ClientState::NotConnected => {
                return;
            }
            ClientState::Poisoned => {}
        }

        *inner = ClientState::NotConnected;
    }

    /// - Connected -> NotConnected: Connection interrupted, attempting reconnection
    /// - NotConnected -> Connected: initial establishment of a connection
    /// - NotConnected -> Poisoned: Connection error(connection refused maybe)
    async fn new_stream_fut(
        &self,
        host: Option<ConnectHeadBuf>,
    ) -> Result<PinBoxFut<Result<BidiStream>>> {
        let mut inner_guard = self.inner.lock().await;

        loop {
            match *inner_guard {
                ClientState::Poisoned => {
                    bail!("connection is poisoned")
                }
                ClientState::Connected {
                    ref conn,
                    ref mut client,
                } => {
                    if conn.is_finished() {
                        *inner_guard = ClientState::NotConnected;
                        continue;
                    }
                    let res = self.config.new_stream(client.clone(), host);
                    return Ok(res);
                }
                ClientState::NotConnected => match self.config.new_connection().await {
                    Err(err) => {
                        *inner_guard = ClientState::Poisoned;
                        bail!(err)
                    }
                    Ok((conn, client)) => {
                        *inner_guard = ClientState::Connected { conn, client };
                    }
                },
            }
        }
    }
}

enum ClientState<SR: Clone> {
    Poisoned,
    NotConnected,
    Connected { conn: JoinHandle<()>, client: SR },
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, path::PathBuf};

    use hyper::client;
    use octoproxy_lib::{
        proxy::TokioExec,
        tls::{build_tls_client_config, build_tls_server_config, NeverProducesTickets, QUIC_ALPN},
    };
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tracing_subscriber::EnvFilter;

    use super::*;

    #[derive(Clone)]
    struct H2MockServer {
        acceptor: TlsAcceptor,
    }

    impl H2MockServer {
        fn new() -> Result<Self> {
            let certs = PathBuf::from("assets/certs/server.crt");
            let key = PathBuf::from("assets/certs/server.key");
            let client_ca = Some(PathBuf::from("assets/certs/ca.crt"));

            let tls_server_config = build_tls_server_config(certs, key, client_ca)?;

            let acceptor = TlsAcceptor::from(Arc::new(tls_server_config));

            Ok(Self { acceptor })
        }

        async fn run(&self, listener: TcpListener) {
            let acceptor = self.acceptor.clone();

            while let Ok((inbound, _)) = listener.accept().await {
                let _ = acceptor.accept(inbound).await;
            }
        }
    }

    fn h2_build_mock_client(server_addr: SocketAddr) -> H2Client {
        let h2config = h2_build_mock_connect_config(server_addr);
        let client = H2Client::new(h2config);
        client
    }

    fn h2_build_mock_connect_config(server_addr: SocketAddr) -> H2ConnConfig {
        let tls_config = build_tls_client_config(
            &Some(PathBuf::from("assets/certs/ca.crt")),
            &PathBuf::from("assets/certs/client.crt"),
            &PathBuf::from("assets/certs/client.key"),
        )
        .unwrap();

        let tls_config = Arc::new(tls_config);
        let domain = ServerName::try_from("localhost").unwrap();

        let builder = client::conn::http2::Builder::new(TokioExec)
            .keep_alive_interval(Duration::from_secs(1))
            .keep_alive_timeout(Duration::from_secs(1))
            .keep_alive_while_idle(true)
            .to_owned();
        let c = H2ConnConfig::new(
            builder,
            server_addr.to_string(),
            domain,
            tls_config,
            Duration::from_secs(1),
        )
        .unwrap();
        c
    }

    #[tokio::test]
    async fn test_h2_new_connect_config() {
        // spawn a server
        let mock_server = H2MockServer::new().unwrap();
        let mock_server_t = mock_server.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();
        let mock_server_job = tokio::spawn(async move {
            mock_server_t.run(listener).await;
        });

        let c = h2_build_mock_connect_config(server_addr);

        let _res = c.new_connection().await.unwrap();

        mock_server_job.abort();

        let res = c.new_connection().await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_h2_client() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .try_init();

        // spawn a server
        let mock_server = H2MockServer::new().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();

        let mock_server_t = mock_server.clone();
        let mock_server_job = tokio::spawn(async move {
            mock_server_t.run(listener).await;
        });

        let client = h2_build_mock_client(server_addr.to_owned());

        // the client is not connected
        {
            let state = client.inner.lock().await;
            assert!(matches!(*state, ClientState::NotConnected));
        }

        // make the client connected
        let res = client.new_stream_fut(None).await.unwrap();
        {
            let state = client.inner.lock().await;
            assert!(matches!(
                *state,
                ClientState::Connected { conn: _, client: _ }
            ));
        }

        let _ = res.await;

        // close server now, then the client would be poisoned
        mock_server_job.abort();
        let res = client.new_stream_fut(None).await;
        {
            let state = client.inner.lock().await;
            // println!("{:?}", *state);
            assert!(matches!(*state, ClientState::Poisoned));
        }
        assert!(res.is_err());

        // bring the server back
        let listener = TcpListener::bind(server_addr).await.unwrap();

        let mock_server_t = mock_server.clone();
        let _mock_server_job = tokio::spawn(async move {
            mock_server_t.run(listener).await;
        });

        // try connect again, would return Poisoned
        let res = client.new_stream_fut(None).await;
        assert_eq!(res.err().unwrap().to_string(), "connection is poisoned");
        {
            let state = client.inner.lock().await;
            // println!("{:?}", *state);
            assert!(matches!(*state, ClientState::Poisoned));
        }

        // reset client
        client.reset_client().await;
        // the connection is back
        let _res = client.new_stream_fut(None).await.unwrap();
        {
            let state = client.inner.lock().await;
            assert!(matches!(
                *state,
                ClientState::Connected { conn: _, client: _ }
            ));
        }
    }

    struct QuicMockServer {
        server_config: quinn::ServerConfig,
    }

    impl QuicMockServer {
        fn new() -> Result<Self> {
            let certs = PathBuf::from("assets/certs/server.crt");
            let key = PathBuf::from("assets/certs/server.key");
            let client_ca = Some(PathBuf::from("assets/certs/ca.crt"));

            let tls_config = build_tls_server_config(certs, key, client_ca)?;

            let mut tls_config = tls_config.clone();
            tls_config.max_early_data_size = u32::MAX;
            tls_config.ticketer = Arc::new(NeverProducesTickets {});

            tls_config.alpn_protocols = vec![QUIC_ALPN.into()];
            let server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));

            Ok(Self { server_config })
        }

        async fn run(&self, listen_address: SocketAddr) {
            let endpoint =
                quinn::Endpoint::server(self.server_config.clone(), listen_address).unwrap();

            while let Some(conn) = endpoint.accept().await {
                let (_connection, _) = conn.into_0rtt().unwrap();
            }
        }
    }

    fn quic_build_mock_connect_config(server_addr: SocketAddr) -> QuicConnConfig {
        let mut tls_config = build_tls_client_config(
            &Some(PathBuf::from("assets/certs/ca.crt")),
            &PathBuf::from("assets/certs/client.crt"),
            &PathBuf::from("assets/certs/client.key"),
        )
        .unwrap();

        let transport_config = quinn::TransportConfig::default();
        tls_config.enable_early_data = true;
        tls_config.alpn_protocols = vec![QUIC_ALPN.into()];

        let mut quinn_config = quinn::ClientConfig::new(Arc::new(tls_config));

        quinn_config.transport_config(Arc::new(transport_config));

        let quic_config = QuicConnConfig::new(
            quinn_config,
            "localhost".to_owned(),
            server_addr.to_string(),
            true,
            true,
        );
        quic_config
    }

    #[tokio::test]
    async fn test_quic_new_connect_config() {
        let mock_server = QuicMockServer::new().unwrap();
        let listener_addr = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();

        let mock_server_job = tokio::spawn(async move {
            mock_server.run(listener_addr).await;
        });

        let c = quic_build_mock_connect_config(listener_addr);
        let _res = c.new_connection().await.unwrap();
        mock_server_job.abort();
    }

    #[tokio::test]
    async fn test_quic_client() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .try_init();

        let mock_server = QuicMockServer::new().unwrap();
        let listener_addr = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();

        let mock_server_job = tokio::spawn(async move {
            mock_server.run(listener_addr).await;
        });

        let client = quic_build_mock_connect_config(listener_addr);
        let client = QuicClient::new(client);

        // the client is not connected
        {
            let state = client.inner.lock().await;
            assert!(matches!(*state, ClientState::NotConnected));
        }

        // make the client connected
        let res = client.new_stream_fut(None).await.unwrap();
        {
            let state = client.inner.lock().await;
            assert!(matches!(
                *state,
                ClientState::Connected { conn: _, client: _ }
            ));
        }

        let _ = res.await;

        // close server now, then the client would NOT be poisoned
        mock_server_job.abort();
        let res = client.new_stream_fut(None).await;
        {
            let state = client.inner.lock().await;
            // println!("{:?}", *state);
            assert!(matches!(
                *state,
                ClientState::Connected { conn: _, client: _ }
            ));
        }
        assert!(res.is_ok());
    }
}
