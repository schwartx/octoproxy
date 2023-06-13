use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::Display,
    net::SocketAddr,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct BackendMetric<'a> {
    pub backend_name: Cow<'a, str>,
    pub domain: Cow<'a, str>,
    pub address: Cow<'a, str>,
    pub metric: MetricData,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackendStatus {
    #[default]
    Normal,
    /// `Closed` status will be changed to `Normal`
    Closed,
    Unknown,
    /// set by manual, will never up automatically
    ForceClosed,
}

#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum BackendProtocol {
    HTTP2,
    #[default]
    Quic,
}

impl Display for BackendProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            BackendProtocol::HTTP2 => "http2",
            BackendProtocol::Quic => "quic",
        };

        write!(f, "{:<12}", s)
    }
}

impl Display for BackendStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            BackendStatus::Normal => "NORMAL",
            BackendStatus::Closed => "CLOSED",
            BackendStatus::Unknown => "UNKNOWN",
            BackendStatus::ForceClosed => "FORCECLOSED",
        };
        write!(f, "{:<12}", s)
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct MetricData {
    pub active_connections: u32,
    pub protocol: BackendProtocol,

    #[serde(default)]
    pub active_peers: HashMap<SocketAddr, u32>,
    pub status: BackendStatus,
    pub connection_time: Duration,

    #[serde(skip)]
    pub peak_ewma: PeakEWMA,
    pub transmission_time: Duration,
    pub incoming_bytes: u64,
    pub outgoing_bytes: u64,
    pub total_incoming_bytes: u64,
    pub total_outgoing_bytes: u64,
    pub failures: u32,
}

/// Copy from Sozu: <https://github.com/sozu-proxy/sozu/blob/c92a92e47ce79a9dfe23530244dc0ce8604acbcd/lib/src/lib.rs#L975>
/// exponentially weighted moving average with high sensibility to latency bursts
///
/// cf Finagle for the original implementation: <https://github.com/twitter/finagle/blob/9cc08d15216497bb03a1cafda96b7266cfbbcff1/finagle-core/src/main/scala/com/twitter/finagle/loadbalancer/PeakEwma.scala>
#[derive(Debug, PartialEq, Clone)]
pub struct PeakEWMA {
    /// decay in nanoseconds
    ///
    /// higher values will make the EWMA decay slowly to 0
    pub decay: f64,
    /// estimated RTT in nanoseconds
    ///
    /// must be set to a high enough default value so that new backends do not
    /// get all the traffic right away
    pub rtt: f64,
    /// last modification
    pub last_event: Instant,
}

impl Default for PeakEWMA {
    fn default() -> Self {
        Self::new()
    }
}

impl PeakEWMA {
    // hardcoded default values for now
    pub fn new() -> Self {
        PeakEWMA {
            // 1s
            decay: 1_000_000_000f64,
            // 50ms
            rtt: 50_000_000f64,
            last_event: Instant::now(),
        }
    }

    pub fn observe(&mut self, rtt: f64) {
        let now = Instant::now();
        let dur = now - self.last_event;

        // if latency is rising, we will immediately raise the cost
        if rtt > self.rtt {
            self.rtt = rtt;
        } else {
            // new_rtt = old_rtt * e^(-elapsed/decay) + observed_rtt * (1 - e^(-elapsed/decay))
            let weight = (-1.0 * dur.as_nanos() as f64 / self.decay).exp();
            self.rtt = self.rtt * weight + rtt * (1.0 - weight);
        }

        self.last_event = now;
    }

    pub fn get(&self, active_requests: u32) -> f64 {
        let now = Instant::now();
        let dur = now - self.last_event;
        let weight = (-1.0 * dur.as_nanos() as f64 / self.decay).exp();
        (active_requests + 1) as f64 * self.rtt * weight
    }

    pub fn get_mut(&mut self, active_requests: u32) -> f64 {
        // decay the current value
        // (we might not have seen a request in a long time)
        self.observe(0.0);

        (active_requests + 1) as f64 * self.rtt
    }
}

#[derive(Serialize, Deserialize)]
pub enum MetricApiReq {
    SwitchBackendStatus { backend_id: usize },
    SwitchBackendProtocol { backend_id: usize },
    ResetBackend { backend_id: usize },
    AllBackends,
}

#[derive(Serialize, Deserialize)]
pub enum MetricApiResp<'a> {
    SwitchBackendStatus,
    SwitchBackendProtocol,
    ResetBackend,
    AllBackends { items: Vec<BackendMetric<'a>> },
    Error { msg: String },
}
