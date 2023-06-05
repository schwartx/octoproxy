use futures_util::Future;
use hyper::service::Service;
use std::{
    fmt, io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite};

use http::Uri;

use self::stream::ProxyStream;

mod stream;
mod tunnel;

#[inline]
pub(crate) fn io_err<E: Into<Box<dyn std::error::Error + Send + Sync>>>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

/// A wrapper around `Proxy`s with a connector.
#[derive(Clone)]
pub struct ProxyConnector<C> {
    proxy: Uri,
    connector: C,
}

impl<C: fmt::Debug> fmt::Debug for ProxyConnector<C> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(
            f,
            "ProxyConnector (unsecured){{ proxies: {:?}, connector: {:?} }}",
            self.proxy, self.connector
        )
    }
}

impl<C> ProxyConnector<C> {
    /// Create a proxy connector and attach a particular proxy
    pub fn new(connector: C, proxy: Uri) -> Self {
        Self { proxy, connector }
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl<C> Service<Uri> for ProxyConnector<C>
where
    C: Service<Uri>,
    C::Response: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    C::Future: Send + 'static,
    C::Error: Into<BoxError>,
{
    type Response = ProxyStream<C::Response>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.connector.poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io_err(e.into()))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let host = uri.authority().map(|auth| auth.to_string()).unwrap();

        let tunnel = tunnel::new(&host);
        let connection =
            proxy_dst(uri, &self.proxy).map(|proxy_url| self.connector.call(proxy_url));

        Box::pin(async move {
            let proxy_stream = match connection {
                Ok(connection) => match connection.await.map_err(io_err) {
                    Ok(proxy_stream) => proxy_stream,
                    Err(e) => return Err(e),
                },
                Err(e) => return Err(e),
            };

            let tunnel_stream = match tunnel.with_stream(proxy_stream).await {
                Ok(tunnel_stream) => tunnel_stream,
                Err(e) => return Err(e),
            };

            Ok(ProxyStream(tunnel_stream))
        })
    }
}

fn proxy_dst(dst: Uri, proxy: &Uri) -> io::Result<Uri> {
    Uri::builder()
        .scheme(
            proxy
                .scheme_str()
                .ok_or_else(|| io_err(format!("proxy uri missing scheme: {}", proxy)))?,
        )
        .authority(
            proxy
                .authority()
                .ok_or_else(|| io_err(format!("proxy uri missing host: {}", proxy)))?
                .clone(),
        )
        .path_and_query(dst.path_and_query().unwrap().clone())
        .build()
        .map_err(|err| io_err(format!("other error: {}", err)))
}
