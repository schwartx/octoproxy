use anyhow::{bail, Context};

use http::{Method, Response};
use hyper::{server::conn::http2, service::service_fn, Body};
use octoproxy_lib::proxy::{tunnel, QuicBidiStream, TokioExec};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

use tracing::{debug, debug_span, trace, warn, Instrument};

/// this function simply handles tls connection before begin
pub async fn handle_h2_connection(acceptor: TlsAcceptor, inbound: TcpStream) -> anyhow::Result<()> {
    // wrap inbound tcp stream into tls stream
    let inbound = acceptor.accept(inbound).await?;

    http2::Builder::new(TokioExec)
        .serve_connection(
            inbound,
            service_fn(|req| async move {
                if req.method() == Method::CONNECT {
                    tokio::spawn(async move {
                        match hyper::upgrade::on(req).await {
                            Ok(upgraded) => {
                                if let Err(e) = tunnel(upgraded).await {
                                    warn!("server io error: {}", e);
                                };
                            }
                            Err(e) => warn!("upgrade error: {}", e),
                        }
                    });
                }
                Ok::<_, hyper::Error>(Response::new(Body::empty()))
            }),
        )
        .await
        // frame=GoAway
        .context("current tcp connection")?;

    Ok(())
}

pub async fn handle_quic_connection(connection: quinn::Connection) -> anyhow::Result<()> {
    let span = debug_span!(
        "connection",
        remote = %connection.remote_address(),
        protocol = %connection
            .handshake_data()
            .unwrap()
            .downcast::<quinn::crypto::rustls::HandshakeData>().unwrap()
            .protocol
            .map_or_else(|| "<none>".into(), |x| String::from_utf8_lossy(&x).into_owned())
    );
    async {
        trace!("established");

        // Each stream initiated by the client constitutes a new request.
        loop {
            let (mut send, recv) = match connection.accept_bi().await {
                Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                    trace!("connection closed");
                    return Ok(());
                }
                Err(e) => {
                    bail!(e)
                }
                Ok(s) => s,
            };

            tokio::spawn(
                async move {
                    match send.write_all(b"OK").await {
                        Ok(_) => {}
                        Err(e) => {
                            debug!("Quic: Failed to send OK: {:?}", e);
                            return;
                        }
                    }
                    let stream = QuicBidiStream { send, recv };
                    if let Err(e) = tunnel(stream).await {
                        debug!("failed: {reason}", reason = e.to_string());
                    }
                }
                .instrument(debug_span!("request")),
            );
        }
    }
    .instrument(span)
    .await?;

    Ok(())
}
