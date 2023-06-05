use hyper::client::connect::{Connected, Connection};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A Proxy Stream wrapper
pub struct ProxyStream<R>(pub R);

impl<R: AsyncRead + AsyncWrite + Unpin> AsyncRead for ProxyStream<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<R: AsyncRead + AsyncWrite + Unpin> AsyncWrite for ProxyStream<R> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl<R: AsyncRead + AsyncWrite + Connection + Unpin> Connection for ProxyStream<R> {
    fn connected(&self) -> Connected {
        self.0.connected().proxy(true)
    }
}
