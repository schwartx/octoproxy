#![allow(unused)]
use std::{fmt::Display, pin::Pin};

use anyhow::bail;
use bytes::{Bytes, BytesMut};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    net::TcpStream,
};
use tokio_stream::StreamExt;
use tokio_util::codec::{Decoder, Framed};
use tracing::{debug, trace};

struct HttpCodec;

struct ReqInfoGetter<I> {
    inbound: I,
}

impl<I> ReqInfoGetter<I>
where
    I: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn get(self) -> anyhow::Result<(RequestInfo, I)> {
        let mut transport = Framed::new(self.inbound, HttpCodec);
        let request_info = loop {
            match transport.next().await {
                Some(Ok(req)) => {
                    debug!("{}", req);
                    break req;
                }
                Some(Err(e)) => {
                    debug!("{:?}", e);
                    bail!(e);
                }
                None => {}
            }
        };

        let inbound = transport.into_inner();
        Ok((request_info, inbound))
    }
}

pub async fn tunnel<I>(mut inbound: I) -> anyhow::Result<()>
where
    I: AsyncRead + AsyncWrite + Unpin + Send,
{
    let url_len = inbound.read_u16().await?;

    let mut buf = BytesMut::with_capacity(url_len as usize);
    inbound.read_buf(&mut buf).await?;

    let url = std::str::from_utf8(&buf)?;

    let mut outbound = TcpStream::connect(url).await?;
    debug!("Established tunnel: {}", url);
    tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
    Ok(())
}

#[derive(Debug)]
struct RequestInfo {
    host: Option<String>,
    path: String,
    #[allow(unused)]
    header: Bytes,
    method: http::Method,
}

impl Display for RequestInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "host: {:?}\npath: {}\nmeth: {}\n",
            self.host, self.path, self.method
        )
    }
}

impl Decoder for HttpCodec {
    type Item = RequestInfo;

    type Error = anyhow::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if buf.is_empty() {
            bail!("parse called with empty buf");
        }

        let path;
        let host;
        let slice;
        let method;
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut headers);

        match req.parse(buf) {
            Ok(httparse::Status::Complete(parsed_len)) => {
                trace!("Request.parse Complete({})", parsed_len);
                method = http::Method::from_bytes(req.method.unwrap().as_bytes())?;

                path = match req.path {
                    Some(path) => <&str>::clone(&path).to_owned(),
                    None => String::from(""),
                };
                let hosts = req
                    .headers
                    .iter()
                    .filter_map(|s| {
                        if s.name.to_lowercase() == "host" {
                            Some(String::from_utf8_lossy(s.value).to_string())
                        } else {
                            None
                        }
                    })
                    .take(1)
                    .collect::<Vec<_>>();

                if hosts.len() == 1 {
                    host = Some(hosts[0].to_owned());
                } else {
                    host = None;
                }
                slice = buf.split_to(parsed_len);
            }
            Ok(httparse::Status::Partial) => return Ok(None),
            Err(err) => {
                bail!(err);
            }
        };
        Ok(Some(RequestInfo {
            host,
            header: slice.freeze(),
            method,
            path,
        }))
    }
}

#[derive(Clone)]
pub struct TokioExec;
impl<F> hyper::rt::Executor<F> for TokioExec
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, fut: F) {
        tokio::spawn(fut);
    }
}

pub struct QuicBidiStream {
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
}

impl AsyncWrite for QuicBidiStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::result::Result<usize, std::io::Error>> {
        Pin::new(&mut self.send).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), std::io::Error>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), std::io::Error>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}

impl AsyncRead for QuicBidiStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}
