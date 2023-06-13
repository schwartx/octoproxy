use anyhow::bail;
use futures::{SinkExt, StreamExt};

use left_right::{Absorb, ReadHandleFactory, WriteHandle};
use octoproxy_lib::metric::{
    BackendMetric, BackendProtocol, BackendStatus, MetricApiReq, MetricApiResp, MetricData,
};
use parking_lot::Mutex;
use std::{borrow::Cow, collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use crate::{backends::Backend, config::Config, proxy::retry_forever};
use axum::{
    extract::{ws::Message, State, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// listening forever for metric
/// handles REST api service that returns all the backends status
pub(crate) async fn listening_metric(config: Arc<Config>) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(ws_handler))
        .with_state(config.clone());

    let addr = config.metric_address;

    info!("metric service listening on {}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}

async fn ws_handler(State(config): State<Arc<Config>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let (mut sender, mut receiver) = socket.split();

        let recv_task = tokio::spawn(async move {
            while let Some(Ok(msg)) = receiver.next().await {
                let res = match msg {
                    Message::Text(req) => match serde_json::from_str::<MetricApiReq>(&req) {
                        Ok(req) => backend_metric_switcher(req, &config).await,
                        Err(e) => MetricApiResp::Error { msg: e.to_string() },
                    },
                    Message::Binary(req) => match rmp_serde::from_slice::<MetricApiReq>(&req) {
                        Ok(req) => backend_metric_switcher(req, &config).await,
                        Err(e) => MetricApiResp::Error { msg: e.to_string() },
                    },
                    Message::Ping(_) | Message::Pong(_) => {
                        continue;
                    }
                    Message::Close(_) => {
                        return;
                    }
                };
                let s = serde_json::to_string(&res).unwrap();
                if sender.send(Message::Text(s)).await.is_err() {
                    warn!("the sender dropped");
                    return;
                }
            }
        });

        if let Err(e) = recv_task.await {
            warn!("fail to handle ws: {}", e)
        }
    })
}

async fn backend_metric_switcher<'a>(
    req: MetricApiReq,
    config: &'a Arc<Config>,
) -> MetricApiResp<'a> {
    match req {
        MetricApiReq::AllBackends => {
            let backends = get_all_backends(&config);
            MetricApiResp::AllBackends { items: backends }
        }
        MetricApiReq::SwitchBackendStatus { backend_id } => {
            if let Err(e) = switch_backend_status(backend_id, &config).await {
                MetricApiResp::Error { msg: e.to_string() }
            } else {
                debug!("switching backend: {} status done", backend_id);
                MetricApiResp::SwitchBackendStatus
            }
        }
        MetricApiReq::ResetBackend { backend_id } => {
            if let Err(e) = reset_backend(backend_id, &config).await {
                MetricApiResp::Error { msg: e.to_string() }
            } else {
                debug!("resetting backend: {} done", backend_id);
                MetricApiResp::ResetBackend
            }
        }
        MetricApiReq::SwitchBackendProtocol { backend_id } => {
            if let Err(e) = switch_backend_protocol(backend_id, &config).await {
                MetricApiResp::Error { msg: e.to_string() }
            } else {
                debug!("switching backend: {} protocol done", backend_id);
                MetricApiResp::SwitchBackendProtocol
            }
        }
    }
}

async fn switch_backend_status(backend_id: usize, config: &Arc<Config>) -> anyhow::Result<()> {
    debug!("switching backend: {} status..", backend_id);
    let backends = find_backend_by_id(backend_id, config);

    if let Some(backend) = backends {
        backend.read().await.cancel();
        let backend_guard = backend.read().await;
        match backend_guard.get_status() {
            BackendStatus::ForceClosed => {
                backend_guard.set_status(BackendStatus::Unknown);
                // drop this backend first, so it would not deadlock
                drop(backend_guard);

                // renew the cancellation_token
                backend.write().await.renew_cancellation_token();
                tokio::spawn(async move {
                    retry_forever(backend).await;
                });
                Ok(())
            }
            BackendStatus::Normal | BackendStatus::Closed | BackendStatus::Unknown => {
                backend_guard.set_status(BackendStatus::ForceClosed);
                // no need to renew the cancellation_token
                Ok(())
            }
        }
    } else {
        bail!("no suck backends")
    }
}

async fn reset_backend(backend_id: usize, config: &Arc<Config>) -> anyhow::Result<()> {
    debug!("reseting backend: {} ..", backend_id);

    if let Some(backend) = find_backend_by_id(backend_id, config) {
        {
            let backend_guard = backend.read().await;
            backend_guard.cancel();
            backend_guard.reset_client().await;
        }

        backend.write().await.renew_cancellation_token();

        return Ok(());
    }
    bail!("no suck backends")
}

async fn switch_backend_protocol(backend_id: usize, config: &Arc<Config>) -> anyhow::Result<()> {
    debug!("reseting backend: {} ..", backend_id);

    if let Some(backend) = find_backend_by_id(backend_id, config) {
        {
            backend.read().await.cancel();
        }
        return backend.write().await.switch_protocol().await;
    }
    bail!("no suck backends")
}

fn find_backend_by_id(backend_id: usize, config: &Arc<Config>) -> Option<Arc<RwLock<Backend>>> {
    config.backends.get(backend_id).map(|b| b.backend.clone())
}

fn get_all_backends(config: &Arc<Config>) -> Vec<BackendMetric<'_>> {
    config
        .backends
        .iter()
        .map(|b| b.metric.as_ref())
        .filter_map(|metric| metric.get())
        .collect::<Vec<_>>()
}

#[derive(Debug, Hash)]
pub(crate) struct PeerInfo {
    pub(crate) host: String,
    pub(crate) addr: SocketAddr,
}

enum MetricOp {
    AddPeer(SocketAddr),
    SubPeer(SocketAddr),
    IncrActiveConnection,
    DecrActiveConnection,
    ChangeStatus(BackendStatus),
    ChangeProtocol(BackendProtocol),
    Summary(Duration, Duration, u64, u64),
    Reset,
    AddFailures,
}

impl Absorb<MetricOp> for MetricData {
    fn absorb_first(&mut self, operation: &mut MetricOp, _other: &Self) {
        match operation {
            MetricOp::IncrActiveConnection => {
                self.active_connections += 1;
            }
            MetricOp::DecrActiveConnection => {
                self.active_connections -= 1;
            }
            MetricOp::Summary(
                connection_time,
                transmission_time,
                incoming_bytes,
                outgoing_bytes,
            ) => {
                self.connection_time = *connection_time;
                // set peak ewma
                self.peak_ewma.observe(connection_time.as_nanos() as f64);
                self.transmission_time = *transmission_time;
                self.incoming_bytes = *incoming_bytes;
                self.outgoing_bytes = *outgoing_bytes;

                if let Some(n) = self.total_incoming_bytes.checked_add(*incoming_bytes) {
                    self.total_incoming_bytes = n;
                };

                if let Some(n) = self.total_outgoing_bytes.checked_add(*outgoing_bytes) {
                    self.total_outgoing_bytes = n;
                };
            }
            MetricOp::AddFailures => self.failures += 1,
            MetricOp::Reset => {
                self.failures = 0;
                self.active_connections = 0;
                self.active_peers = HashMap::new();
            }
            MetricOp::AddPeer(peer) => {
                self.active_peers
                    .entry(*peer)
                    .and_modify(|counter| *counter += 1)
                    .or_insert(1);
            }
            MetricOp::SubPeer(peer) => {
                let mut delete = false;
                self.active_peers.entry(*peer).and_modify(|counter| {
                    if *counter == 1 {
                        delete = true;
                    } else {
                        *counter -= 1
                    }
                });
                if delete {
                    self.active_peers.remove(peer);
                }
            }
            MetricOp::ChangeStatus(status) => self.status = *status,
            MetricOp::ChangeProtocol(protocol) => self.protocol = *protocol,
        }
    }

    fn sync_with(&mut self, first: &Self) {
        *self = first.clone()
    }
}

struct MetricWriter {
    writer: WriteHandle<MetricData, MetricOp>,
}

impl MetricWriter {
    fn incr_connection(&mut self, peer: SocketAddr) {
        self.writer.append(MetricOp::IncrActiveConnection);
        self.writer.append(MetricOp::AddPeer(peer));
        self.writer.publish();
    }

    fn set_protocol(&mut self, protocol: BackendProtocol) {
        self.writer.append(MetricOp::ChangeProtocol(protocol));
        self.writer.publish();
    }

    fn set_status(&mut self, status: BackendStatus) {
        self.writer.append(MetricOp::ChangeStatus(status));
        self.writer.publish();
    }

    fn reset(&mut self) {
        self.writer.append(MetricOp::Reset);
        self.writer.publish();
    }

    /// summary current transmission
    fn summary(
        &mut self,
        connection_time: Duration,
        transmission_time: Duration,
        incoming_bytes: u64,
        outgoing_bytes: u64,
        peer: SocketAddr,
    ) {
        self.writer.append(MetricOp::DecrActiveConnection);
        self.writer.append(MetricOp::SubPeer(peer));
        self.writer.append(MetricOp::Summary(
            connection_time,
            transmission_time,
            incoming_bytes,
            outgoing_bytes,
        ));
        self.writer.publish();
    }

    fn add_failures(&mut self, peer: SocketAddr) {
        self.writer.append(MetricOp::AddFailures);
        self.writer.append(MetricOp::SubPeer(peer));
        self.writer.append(MetricOp::DecrActiveConnection);
        self.writer.publish();
    }
}

#[derive(Clone)]
pub(crate) struct Metric {
    // backend_name, backend_address,domain is fixed
    backend_name: String,
    backend_address: String,
    domain: String,

    reader_factory: ReadHandleFactory<MetricData>,
    writer: Arc<Mutex<MetricWriter>>,
}

impl Metric {
    pub(crate) fn new(
        backend_name: &str,
        backend_address: &str,
        domain: &str,
        status: BackendStatus,
        protocol: BackendProtocol,
    ) -> Self {
        let (w, r) = left_right::new::<MetricData, MetricOp>();
        let writer = Arc::new(Mutex::new(MetricWriter { writer: w }));
        {
            let mut w = writer.lock();
            w.set_status(status);
            w.set_protocol(protocol);
        }
        Self {
            backend_address: backend_address.to_owned(),
            backend_name: backend_name.to_owned(),
            domain: domain.to_owned(),
            writer,
            reader_factory: r.factory(),
        }
    }

    pub(crate) fn set_protocol(&self, protocol: BackendProtocol) {
        self.writer.lock().set_protocol(protocol)
    }

    pub(crate) fn set_status(&self, status: BackendStatus) {
        self.writer.lock().set_status(status)
    }

    pub(crate) fn summary(
        &self,
        connection_time: Duration,
        transmission_time: Duration,
        incoming_bytes: u64,
        outgoing_bytes: u64,
        peer: SocketAddr,
    ) {
        self.writer.lock().summary(
            connection_time,
            transmission_time,
            incoming_bytes,
            outgoing_bytes,
            peer,
        )
    }

    pub(crate) fn add_failures(&self, peer: SocketAddr) {
        self.writer.lock().add_failures(peer)
    }

    pub(crate) fn incr_connection(&self, peer: SocketAddr) {
        self.writer.lock().incr_connection(peer)
    }

    pub(crate) fn reset(&self) {
        self.writer.lock().reset()
    }

    pub(crate) fn get_peak_ewma(&self) -> f64 {
        match self.reader_factory.handle().enter() {
            Some(r) => r.peak_ewma.get(r.active_connections),
            None => 1000000.0,
        }
    }

    pub(crate) fn get_status(&self) -> BackendStatus {
        match self.reader_factory.handle().enter() {
            Some(r) => r.status,
            None => BackendStatus::Unknown,
        }
    }

    fn get(&self) -> Option<BackendMetric> {
        let r = self.reader_factory.handle();
        let metric_reader = r.enter();

        metric_reader.map(|metric_data| BackendMetric {
            backend_name: Cow::Borrowed(&self.backend_name),
            address: Cow::Borrowed(&self.backend_address),
            domain: Cow::Borrowed(&self.domain),
            metric: metric_data.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_metric() {
        let metric = Metric::new(
            "test_backend",
            "localhost:8080",
            "example.com",
            BackendStatus::Normal,
            BackendProtocol::HTTP2,
        );

        let mock_addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        for _ in 0..10 {
            metric.incr_connection(mock_addr);
        }

        for _ in 0..3 {
            metric.add_failures(mock_addr);
        }

        let res = metric.get().unwrap();
        // 10(active connections) - 3(failures) = 7
        assert_eq!(
            res.metric.active_connections, 7,
            "incorrect active_connections count"
        );
        assert_eq!(res.metric.failures, 3, "incorrect failures count");
        assert_eq!(
            res.metric.active_peers.len(),
            1,
            "incorrect active peers count"
        );

        metric.reset();

        let res = metric.get().unwrap();
        assert_eq!(
            res.metric.active_connections, 0,
            "incorrect failures count after reset"
        );
        assert_eq!(
            res.metric.failures, 0,
            "incorrect failures count after reset"
        );
        assert_eq!(
            res.metric.active_peers.len(),
            0,
            "incorrect active peers count"
        );
    }
}
