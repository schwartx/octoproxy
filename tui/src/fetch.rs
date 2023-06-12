use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock, RwLockReadGuard,
    },
    thread,
    time::Duration,
};

use anyhow::Result;
use crossbeam_channel::{tick, unbounded, Receiver, Select, Sender};
use octoproxy_lib::metric::{BackendMetric, MetricApiReq, MetricApiResp};
use tungstenite::{connect, Message};
use url::Url;

use crate::MetricApiNotify;

pub static BACKENDS_FETCHER_INTERVAL: Duration = Duration::from_millis(500);

pub struct Fetcher {
    inner_sender: Sender<MetricApiReq>,
    pending: Arc<AtomicBool>,
    pending_on_id: usize,
    backends: Arc<RwLock<Vec<BackendMetric<'static>>>>,
}

impl Fetcher {
    pub(crate) fn new(
        url: String,
        tx_fetcher: Sender<MetricApiNotify>,
        close_tx: Sender<()>,
    ) -> Self {
        let (inner_sender, inner_receiver) = unbounded();
        let pending = Arc::new(AtomicBool::new(false));
        let pending_t = pending.clone();

        let backends: Arc<RwLock<Vec<BackendMetric>>> = Arc::new(RwLock::new(Vec::new()));

        let f = Self {
            inner_sender,
            pending,
            pending_on_id: 0,
            backends: backends.clone(),
        };

        thread::spawn(move || {
            match run_loop(
                &url,
                tx_fetcher.clone(),
                inner_receiver,
                pending_t,
                close_tx,
                backends,
            ) {
                Ok(_) => {}
                Err(e) => {
                    tx_fetcher
                        .send(MetricApiNotify::Error(format!("{:?}", e)))
                        .unwrap_or(());
                }
            }
        });

        f
    }

    pub(crate) fn get_backends(&self) -> RwLockReadGuard<Vec<BackendMetric>> {
        self.backends.read().unwrap()
    }

    pub(crate) fn get_pending_on_id(&self) -> usize {
        self.pending_on_id
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed)
    }

    pub(crate) fn reset_backend(&mut self, selected: usize) {
        if self.backend_action(MetricApiReq::ResetBackend {
            backend_id: selected,
        }) {
            self.pending_on_id = selected;
        }
    }

    pub(crate) fn switch_backend_protocol(&mut self, selected: usize) {
        if self.backend_action(MetricApiReq::SwitchBackendProtocol {
            backend_id: selected,
        }) {
            self.pending_on_id = selected;
        }
    }

    pub(crate) fn switch_backend_status(&mut self, selected: usize) {
        if self.backend_action(MetricApiReq::SwitchBackendStatus {
            backend_id: selected,
        }) {
            self.pending_on_id = selected;
        }
    }

    fn backend_action(&self, msg: MetricApiReq) -> bool {
        if self.is_pending() {
            return false;
        }
        self.pending.store(true, Ordering::Relaxed);

        self.inner_sender.send(msg).unwrap();
        true
    }
}

fn run_loop(
    url: &str,
    tx: Sender<MetricApiNotify>,
    inner_receiver: Receiver<MetricApiReq>,
    pending: Arc<AtomicBool>,
    close_tx: Sender<()>,
    backends: Arc<RwLock<Vec<BackendMetric>>>,
) -> Result<()> {
    let (mut socket, _) = connect(Url::parse(url)?)?;
    let ticker = tick(BACKENDS_FETCHER_INTERVAL);

    loop {
        let mut sel = Select::new();
        sel.recv(&inner_receiver);
        sel.recv(&ticker);

        let oper = sel.select();
        let req = match oper.index() {
            0 => oper.recv(&inner_receiver),
            1 => oper.recv(&ticker).map(|_| MetricApiReq::AllBackends),
            _ => unreachable!(),
        }?;

        let req = serde_json::to_string(&req).unwrap();
        socket.write_message(Message::Text(req)).unwrap();

        match socket.read_message() {
            Ok(res) => match res {
                Message::Text(res) => {
                    let backends_resp = serde_json::from_str::<MetricApiResp>(&res)?;
                    match backends_resp {
                        MetricApiResp::SwitchBackendProtocol
                        | MetricApiResp::SwitchBackendStatus
                        | MetricApiResp::ResetBackend => pending.store(false, Ordering::Relaxed),
                        MetricApiResp::AllBackends { items } => {
                            let mut guard = backends.write().unwrap();
                            *guard = items;
                            tx.send(MetricApiNotify::AllBackends).unwrap();
                        }
                        MetricApiResp::Error { msg } => {
                            tx.send(MetricApiNotify::Error(msg)).unwrap();
                        }
                    }
                }
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                Message::Close(_) => {
                    close_tx.send(()).unwrap();
                    break;
                }
            },
            Err(e) => {
                log::trace!("{:?}", e);
                close_tx.send(()).unwrap();
                break;
            }
        }
    }
    Ok(())
}
