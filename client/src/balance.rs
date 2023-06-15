use std::sync::Arc;

use hashring::HashRing;
use rand::{seq::SliceRandom, thread_rng};
use serde::Deserialize;
use tracing::info;

use crate::{config::ConfigBackend, metric::PeerInfo};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Balance {
    RoundRobin,
    Random,
    LeastLoadedTime,
    UriHash,
    Frist,
}

pub(crate) trait LoadBalancingAlgorithm<T> {
    fn next_available_backend(&self, items: &[T], info: &PeerInfo) -> Option<T>;
}

/// always return first
pub(crate) struct Frist;

impl<T: Clone> LoadBalancingAlgorithm<T> for Frist {
    // this will cause a divisor of zero error if backends.len()==0
    fn next_available_backend(&self, items: &[T], _: &PeerInfo) -> Option<T> {
        items.first().cloned()
    }
}

pub(crate) struct RoundRobin {
    next_item_index: Arc<parking_lot::Mutex<usize>>,
}

impl RoundRobin {
    pub(crate) fn new() -> Self {
        Self {
            next_item_index: Arc::new(parking_lot::Mutex::new(0)),
        }
    }
}

impl<T: Clone> LoadBalancingAlgorithm<T> for RoundRobin {
    // this will cause a divisor of zero error if backends.len()==0
    fn next_available_backend(&self, items: &[T], _: &PeerInfo) -> Option<T> {
        let mut next_item = self.next_item_index.lock();
        let next_backend_id = *next_item;
        let backend = items.get(next_backend_id).cloned();
        *next_item = (next_backend_id + 1) % items.len();
        backend
    }
}

pub(crate) struct Random;

impl<T: Clone> LoadBalancingAlgorithm<T> for Random {
    fn next_available_backend(&self, items: &[T], _: &PeerInfo) -> Option<T> {
        items.choose(&mut thread_rng()).cloned()
    }
}

pub(crate) struct LeastLoadedTime;

impl LoadBalancingAlgorithm<ConfigBackend> for LeastLoadedTime {
    fn next_available_backend(
        &self,
        backends: &[ConfigBackend],
        _: &PeerInfo,
    ) -> Option<ConfigBackend> {
        let mut b = None;
        for backend in backends {
            let cost = backend.metric.get_peak_ewma();
            match b.take() {
                None => b = Some((cost, backend)),
                Some((cost_t, bk)) => {
                    if cost_t <= cost {
                        b = Some((cost_t, bk));
                    } else {
                        b = Some((cost, backend));
                    }
                }
            }
        }

        b.map(|(_cost, backend)| backend.clone())
    }
}

pub(crate) struct UriHash {
    ring: HashRing<usize>,
}

impl UriHash {
    pub(crate) fn new(ring: HashRing<usize>) -> Self {
        Self { ring }
    }
}

impl<T: Clone> LoadBalancingAlgorithm<T> for UriHash {
    fn next_available_backend(&self, items: &[T], info: &PeerInfo) -> Option<T> {
        let item_id = self.ring.get(&info);

        // get backend by id
        if let Some(backend_id) = item_id {
            let mut bid = *backend_id;
            for _ in 0..=items.len() {
                if let Some(item) = items.get(bid) {
                    info!("uri_hash: selecting backen_id: {}", bid);
                    return Some(item.clone());
                } else {
                    bid = (bid + 1) % items.len();
                }
            }
        }
        info!("uri_hash: fallback select the first");
        items.get(0).cloned()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Clone, PartialEq, Debug)]
    struct Mock {
        id: usize,
    }

    fn new_host_peer_info(host: &str) -> PeerInfo {
        PeerInfo::new(host.to_owned(), "127.0.0.1:8080".parse().unwrap())
    }

    fn empty_host_peer_info() -> PeerInfo {
        PeerInfo::new("".to_owned(), "127.0.0.1:8080".parse().unwrap())
    }

    #[test]
    fn test_first() {
        let items: Vec<Mock> = (0..=3).into_iter().map(|i| Mock { id: i }).collect();
        let load_balance = Frist {};
        for _ in 0..=5 {
            let res = load_balance.next_available_backend(&items, &empty_host_peer_info());
            assert_eq!(res, Some(Mock { id: 0 }))
        }

        let items: Vec<Mock> = Vec::new();
        for _ in 0..=5 {
            let res = load_balance.next_available_backend(&items, &empty_host_peer_info());
            assert_eq!(res, None)
        }
    }

    #[test]
    fn test_round_robin() {
        let count = 3;
        let items: Vec<Mock> = (0..=count).into_iter().map(|i| Mock { id: i }).collect();

        let load_balance = RoundRobin::new();

        for i in 0..=count {
            let res = load_balance.next_available_backend(&items, &empty_host_peer_info());
            assert_eq!(res, Some(Mock { id: i }))
        }
    }

    #[test]
    fn test_random() {
        let items: Vec<Mock> = (0..=2).into_iter().map(|i| Mock { id: i }).collect();

        let load_balance = Random {};
        let res = load_balance.next_available_backend(&items, &empty_host_peer_info());
        assert!(res.is_some());

        let items: Vec<Mock> = Vec::new();
        let res = load_balance.next_available_backend(&items, &empty_host_peer_info());
        assert!(res.is_none());
    }

    #[test]
    fn test_uri_hash() {
        let count = 3;
        let items: Vec<Mock> = (0..count).into_iter().map(|i| Mock { id: i }).collect();
        let mut ring: HashRing<usize> = HashRing::new();

        for i in 0..items.len() {
            ring.add(i)
        }

        let load_balance = UriHash::new(ring);

        let targets: Vec<_> = (0..100)
            .into_iter()
            .map(|i| format!("{0}.{0}.{0}.{0}:80", i))
            .collect();

        let targets_map: HashMap<_, _> = targets
            .iter()
            .map(|u| {
                (
                    u,
                    load_balance.next_available_backend(&items, &new_host_peer_info(u)),
                )
            })
            .collect();

        let mut rng = thread_rng();

        // randomly select half of the items
        // The result should be the same as the previous one
        for u in targets.choose_multiple(&mut rng, targets.len() / 2) {
            let res = load_balance
                .next_available_backend(&items, &new_host_peer_info(u))
                .unwrap();
            let want = targets_map.get(u).unwrap().as_ref().unwrap();
            assert_eq!(res.id, want.id);
        }
    }

    #[test]
    fn test_least_loaded_time() {
        // todo!()
    }
}
