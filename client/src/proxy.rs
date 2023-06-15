use http::{Method, Request, Response};
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::Body;
use hyper::Server;
use octoproxy_lib::metric::BackendStatus;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

use anyhow::bail;

use hyper::upgrade::Upgraded;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::metric::PeerInfo;
use crate::{backends::Backend, config::Config};

/// listening forever for proxy
pub(crate) async fn listening_proxy(config: Arc<Config>) -> anyhow::Result<()> {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let listener = listen_incoming_connection(sender, config.listen_address);

    let handle = async move {
        while let Some((inbound, peer)) = receiver.recv().await {
            tokio::spawn(handle_connection(config.clone(), inbound, peer));
        }
        Ok(())
    };

    tokio::try_join!(listener, handle)?;
    Ok(())
}

#[derive(Debug)]
enum IncomingConnection {
    ConnectMethod(Upgraded),
    NonConnectMethod(tokio::sync::oneshot::Sender<Response<Body>>, Request<Body>),
}

/// Establish a tunnel connection between incoming connections and the current program
async fn listen_incoming_connection(
    sender: UnboundedSender<(IncomingConnection, PeerInfo)>,
    addr: SocketAddr,
) -> anyhow::Result<()> {
    let make_service = make_service_fn(move |peer: &AddrStream| {
        let peer_addr = peer.remote_addr();
        let sender = sender.clone();
        async move {
            Ok::<_, hyper::Error>(service_fn(move |req| {
                let sender = sender.clone();
                async move {
                    let host =
                        if let Some(host) = req.uri().authority().map(|auth| auth.to_string()) {
                            host
                        } else {
                            warn!("CONNECT host is not socket addr: {:?}", req.uri());
                            let mut resp =
                                Response::new(Body::from("CONNECT must be to a socket address"));
                            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
                            return Ok::<_, hyper::Error>(resp);
                        };

                    let peer = PeerInfo::new(host, peer_addr);

                    if req.method() == Method::CONNECT {
                        tokio::spawn(async move {
                            match hyper::upgrade::on(req).await {
                                Ok(upgraded) => {
                                    sender
                                        .send((IncomingConnection::ConnectMethod(upgraded), peer))
                                        .unwrap();
                                }
                                Err(e) => warn!("incoming connection upgrade error: {}", e),
                            }
                        });
                        Ok::<_, hyper::Error>(Response::new(Body::empty()))
                    } else {
                        let (tx, rx) = tokio::sync::oneshot::channel::<Response<Body>>();
                        sender
                            .send((IncomingConnection::NonConnectMethod(tx, req), peer))
                            .unwrap();
                        let res = rx.await.unwrap();
                        Ok::<_, hyper::Error>(res)
                    }
                }
            }))
        }
    });

    info!("proxy listening on {}", addr);
    let server = Server::bind(&addr).serve(make_service);
    server.await?;
    Ok(())
}

// handles each connection for proxy
async fn handle_connection(
    config: Arc<Config>,
    inbound: IncomingConnection,
    peer: PeerInfo,
) -> anyhow::Result<()> {
    // begin transfer
    match inbound {
        IncomingConnection::NonConnectMethod(tx, req) => {
            let res = match config.non_connect_method_client.request(req).await {
                Ok(res) => res,
                Err(e) => Response::new(Body::from(e.to_string())),
            };
            if tx.send(res).is_err() {
                warn!("the receiver dropped");
            }
            Ok(())
        }
        IncomingConnection::ConnectMethod(inbound) => {
            let (outbound, connection_time, backend) = loop {
                let backend = {
                    match config.next_available_backend(&peer).await {
                        crate::config::AvailableBackend::GotBackend(backend) => backend,
                        crate::config::AvailableBackend::Block => {
                            return Ok(());
                        }
                        crate::config::AvailableBackend::NoBackend => {
                            debug!("no backend!");
                            return Ok(());
                        }
                    }
                };

                // try connect to remote server
                // ok => break loop
                // err => try again and mark this backend as fail
                let backend_guard = backend.read().await;
                let start = Instant::now();
                // host rewrite
                let host = config.rewrite_host(&peer);
                match backend_guard.try_connect(Some(host)).await {
                    Ok(outbound) => {
                        break (outbound, start.elapsed(), backend.clone());
                    }
                    Err(err) => {
                        debug!("connection/stream error: {:?}", err);
                    }
                };
                // set this backend as down and begin retries
                backend_guard.set_status(BackendStatus::Closed);
                tokio::spawn(retry_forever(backend.clone()));
            };
            // gain this backend's read control
            let backend = backend.read().await;
            let metric = backend.metric.clone();
            // increase active connection at the same time
            let metric_incr = metric.clone();
            let peer_addr = peer.get_addr();
            tokio::spawn(async move { metric_incr.incr_connection(peer_addr) });

            if let Err(err) = backend
                .transfer(outbound, inbound, connection_time, peer_addr)
                .await
            {
                metric.add_failures(peer_addr);
                bail!(err)
            };
            Ok(())
        }
    }
}

/// when a backend fail to connect the remote server, will go to this function,
/// trying to connect it at interval.
/// only acquire lock when it's time to connect, otherwise the metric would be
/// blocked for waiting backend read lock to acquire
///
/// this is the only way turn a backend to `Normal` from other status
pub(crate) async fn retry_forever(backend: Arc<RwLock<Backend>>) {
    let (mut retry_interval, max_retries) = {
        let backend_guard = backend.read().await;
        let retry_interval = tokio::time::interval(tokio::time::Duration::from_secs(
            backend_guard.retry_timeout,
        ));

        (retry_interval, backend_guard.max_retries)
    };
    let max_retries = if max_retries == 0 {
        u32::MAX
    } else {
        max_retries
    };

    for _ in 0..max_retries {
        retry_interval.tick().await;

        let backend_guard = backend.read().await;
        match backend_guard.retry_connect().await {
            std::ops::ControlFlow::Continue(_) => {
                continue;
            }
            std::ops::ControlFlow::Break(_) => {
                return;
            }
        }
    }
    debug!("max tries exceed")
}
