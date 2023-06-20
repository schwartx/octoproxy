use std::{fmt::Display, pin::Pin};

use bytes::{BufMut, Bytes, BytesMut};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    net::TcpStream,
};
use tracing::debug;

pub async fn tunnel<I>(mut inbound: I) -> anyhow::Result<()>
where
    I: AsyncRead + AsyncWrite + Unpin + Send,
{
    let url_len = inbound.read_u16().await?;

    let mut buf = BytesMut::with_capacity(url_len as usize);
    inbound.read_buf(&mut buf).await?;

    let url = std::str::from_utf8(buf.as_ref())?;

    let mut outbound = TcpStream::connect(url).await?;
    debug!("Established tunnel: {}", url);
    tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
    Ok(())
}

#[derive(Clone)]
pub struct ConnectHeadBuf(Bytes);

impl AsRef<[u8]> for ConnectHeadBuf {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl From<&str> for ConnectHeadBuf {
    fn from(host: &str) -> Self {
        let mut buf = BytesMut::new();
        buf.put_u16(host.len() as u16);
        buf.put(host.as_bytes());
        Self(buf.freeze())
    }
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
