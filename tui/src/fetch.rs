use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::Result;
use crossbeam_channel::{tick, unbounded, Receiver, Select, Sender};
use octoproxy_lib::metric::MetricApiReq;
use tungstenite::{connect, Message};
use url::Url;

use crate::MetricApiResp;

pub static BACKENDS_FETCHER_INTERVAL: Duration = Duration::from_millis(500);

pub struct Fetcher {
    url: String,
    sender: Sender<MetricApiResp>,
    receiver: Receiver<MetricApiResp>,
    inner_sender: Sender<MetricApiReq>,
    pending: Arc<AtomicBool>,
    pending_on_id: usize,
}

impl Fetcher {
    pub fn new(url: String, close_tx: Sender<()>) -> Self {
        let (sender, receiver) = unbounded();
        let (inner_sender, inner_receiver) = unbounded();
        let f = Self {
            url,
            sender,
            inner_sender,
            receiver,
            pending: Arc::new(AtomicBool::new(false)),
            pending_on_id: 0,
        };

        let app_sender = f.sender.clone();

        let pending = f.pending.clone();
        let url = f.url.clone();
        thread::spawn(move || {
            match run_loop(&url, app_sender.clone(), inner_receiver, pending, close_tx) {
                Ok(_) => {}
                Err(e) => {
                    app_sender
                        .send(MetricApiResp::Error {
                            msg: format!("{:?}", e),
                        })
                        .unwrap();
                }
            }
        });

        f
    }

    pub fn get_receiver(&self) -> Receiver<MetricApiResp> {
        self.receiver.clone()
    }

    pub fn get_pending_on_id(&self) -> usize {
        self.pending_on_id
    }

    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed)
    }

    pub fn reset_backend(&mut self, selected: usize) {
        if self.backend_action(MetricApiReq::ResetBackend {
            backend_id: selected,
        }) {
            self.pending_on_id = selected;
        }
    }

    pub fn switch_backend_protocol(&mut self, selected: usize) {
        if self.backend_action(MetricApiReq::SwitchBackendProtocol {
            backend_id: selected,
        }) {
            self.pending_on_id = selected;
        }
    }

    pub fn switch_backend_status(&mut self, selected: usize) {
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
    tx: Sender<MetricApiResp>,
    inner_receiver: Receiver<MetricApiReq>,
    pending: Arc<AtomicBool>,
    close_tx: Sender<()>,
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
                    let backends = serde_json::from_str::<MetricApiResp>(&res)?;
                    match backends {
                        MetricApiResp::SwitchBackendProtocol
                        | MetricApiResp::SwitchBackendStatus
                        | MetricApiResp::ResetBackend => pending.store(false, Ordering::Relaxed),
                        MetricApiResp::AllBackends { items } => {
                            tx.send(MetricApiResp::AllBackends { items }).unwrap();
                        }
                        MetricApiResp::Error { msg } => {
                            tx.send(MetricApiResp::Error { msg }).unwrap();
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
