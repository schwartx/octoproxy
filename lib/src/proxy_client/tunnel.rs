use bytes::{buf::Buf, BytesMut};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::proxy_client::io_err;

macro_rules! try_ready {
    ($x:expr) => {
        match $x {
            core::task::Poll::Ready(Ok(x)) => x,
            core::task::Poll::Ready(Err(e)) => return core::task::Poll::Ready(Err(e.into())),
            core::task::Poll::Pending => return core::task::Poll::Pending,
        }
    };
}

pub(crate) struct TunnelConnect {
    buf: BytesMut,
}

impl TunnelConnect {
    /// Change stream
    pub fn with_stream<S>(self, stream: S) -> Tunnel<S> {
        Tunnel {
            buf: self.buf,
            stream: Some(stream),
            state: TunnelState::Writing,
        }
    }
}

pub(crate) struct Tunnel<S> {
    buf: BytesMut,
    stream: Option<S>,
    state: TunnelState,
}

#[derive(Debug)]
enum TunnelState {
    Writing,
    Reading,
}

/// Creates a new tunnel through proxy
pub(crate) fn new(host: &str) -> TunnelConnect {
    let buf = format!("CONNECT {0} HTTP/1.1\r\nHost: {0}\r\n\r\n", host).into_bytes();

    TunnelConnect {
        buf: buf.as_slice().into(),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Future for Tunnel<S> {
    type Output = Result<S, io::Error>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.stream.is_none() {
            panic!("must not poll after future is complete")
        }

        let this = self.get_mut();

        loop {
            if let TunnelState::Writing = &this.state {
                let fut = this.stream.as_mut().unwrap().write_buf(&mut this.buf);
                futures_util::pin_mut!(fut);
                let n = try_ready!(fut.poll(ctx));

                if !this.buf.has_remaining() {
                    this.state = TunnelState::Reading;
                    this.buf.truncate(0);
                } else if n == 0 {
                    return Poll::Ready(Err(io_err("unexpected EOF while tunnel writing")));
                }
            } else {
                let fut = this.stream.as_mut().unwrap().read_buf(&mut this.buf);
                futures_util::pin_mut!(fut);
                let n = try_ready!(fut.poll(ctx));

                if n == 0 {
                    return Poll::Ready(Err(io_err("unexpected EOF while tunnel reading")));
                } else {
                    let read = &this.buf[..];
                    if read.len() > 12 {
                        if read.starts_with(b"HTTP/1.1 200") || read.starts_with(b"HTTP/1.0 200") {
                            if read.ends_with(b"\r\n\r\n") {
                                return Poll::Ready(Ok(this.stream.take().unwrap()));
                            }
                        // else read more
                        } else {
                            let len = read.len().min(16);
                            return Poll::Ready(Err(io_err(format!(
                                "unsuccessful tunnel ({})",
                                String::from_utf8_lossy(&read[0..len])
                            ))));
                        }
                    }
                }
            }
        }
    }
}
