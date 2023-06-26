use std::{net::SocketAddr, time::Duration};

use clap::Parser;
use tracing_subscriber::fmt::format::FmtSpan;
use warp::Filter;

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
struct Opts {
    /// http proxy server listening port
    #[arg(short = 'p', default_value_t = 3030)]
    port: u16,
    #[arg(long = "tls")]
    tls: bool,
}

#[derive(Debug)]
struct NotUtf8;
impl warp::reject::Reject for NotUtf8 {}

#[tokio::main]
async fn main() {
    let opts = Opts::parse();
    // Filter traces based on the RUST_LOG env var, or, if it's not set,
    // default to show the output of the example.
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "tracing=info,warp=debug".to_owned());

    // Configure the default `tracing` subscriber.
    // The `fmt` subscriber from the `tracing-subscriber` crate logs `tracing`
    // events to stdout. Other subscribers are available for integrating with
    // distributed tracing systems such as OpenTelemetry.
    tracing_subscriber::fmt()
        // Use the filter we built above to determine which traces to record.
        .with_env_filter(filter)
        // Record an event when each span closes. This can be used to time our
        // routes' durations!
        .with_span_events(FmtSpan::CLOSE)
        .init();

    let hello = warp::get()
        .map(|| "Hello, World!") // Wrap all the routes with a filter that creates a `tracing` span for
        // each request we receive, including data about the request.
        .with(warp::trace::request());

    let sleep = warp::get()
        .and(warp::path("sleep"))
        .and(warp::path::param())
        .map(|s: usize| {
            std::thread::sleep(Duration::from_secs(s as u64));
            "Hello, World!"
        })
        .with(warp::trace::request());

    let hello_body = warp::post()
        .and(warp::body::content_length_limit(1024 * 16))
        .and(
            warp::body::bytes().and_then(|body: bytes::Bytes| async move {
                std::str::from_utf8(&body)
                    .map(String::from)
                    .map_err(|_e| warp::reject::custom(NotUtf8))
            }),
        )
        .map(warp::reply::html)
        .with(warp::trace::request());

    let addr: SocketAddr = ([127, 0, 0, 1], opts.port).into();
    if opts.tls {
        // let cert = include_bytes!("../.././example-certs/server/server.crt");
        // let key = include_bytes!("../.././example-certs/server/server.key");
        // println!("Listening(tls) on https://{}", addr);
        // warp::serve(hello).tls().cert(cert).key(key).run(addr).await;
    } else {
        println!("Listening on http://{}", addr);
        warp::serve(hello_body.or(sleep).or(hello)).run(addr).await;
    }
}
