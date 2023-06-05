use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser;
use futures::TryFutureExt;
use http::Request;
use hyper::{client::HttpConnector, Client};
use octoproxy_lib::proxy_client;
use proxy_client::ProxyConnector;
use tokio::time::interval;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
struct Opts {
    /// proxy to send e.g. http://localhost:8081
    #[arg(short = 'x')]
    proxy: String,
    #[arg(short = 'k')]
    skip_ssl_verified: bool,

    /// Send requests at intervals of n seconds.
    #[arg(short = 'n', default_value_t = 1)]
    interval: u64,

    /// Send requests at once
    #[arg(long = "once", default_value_t = false)]
    once: bool,

    /// Number of multiple requests to perform at a time.
    #[arg(short = 'c', default_value_t = 1)]
    concurrent: usize,

    /// reuse client for each request
    #[arg(long = "reuse", default_value_t = false)]
    reuse: bool,

    #[arg(value_name = "URL")]
    url: String,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init()
        .unwrap();

    let opts = Opts::parse();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(run(opts));
}

async fn run(opts: Opts) {
    let mut interval = interval(Duration::from_secs(opts.interval));

    let opts = Arc::new(opts);
    let client = build_client(opts.clone()).expect("create client");
    if opts.once {
        _ = each_run(0, opts, client.clone())
            .map_err(|e| warn!("{}", e))
            .await;
        return;
    }

    loop {
        let mut handles = vec![];
        for i in 0..opts.concurrent {
            handles.push(tokio::spawn(
                each_run(
                    i,
                    opts.clone(),
                    if opts.reuse {
                        client.clone()
                    } else {
                        build_client(opts.clone()).expect("create client")
                    },
                )
                .map_err(|e| warn!("{}", e)),
            ));
        }
        futures::future::join_all(handles).await;

        interval.tick().await;
    }
}

async fn each_run(
    id: usize,
    opts: Arc<Opts>,
    client: Client<ProxyConnector<HttpConnector>>,
) -> anyhow::Result<()> {
    let start = Instant::now();

    let req = Request::get(opts.url.clone()).body(hyper::Body::empty())?;
    let res = client.request(req).await?;
    info!("time elasped: {:?},id: {}: {:?}", start.elapsed(), id, res);
    Ok(())
}

fn build_client(opts: Arc<Opts>) -> anyhow::Result<Client<ProxyConnector<HttpConnector>>> {
    let proxy = {
        let proxy_uri = opts.proxy.parse().unwrap();
        let connector = HttpConnector::new();
        ProxyConnector::new(connector, proxy_uri)
    };
    let client = hyper::client::Client::builder().build::<_, hyper::Body>(proxy);
    Ok(client)
}
