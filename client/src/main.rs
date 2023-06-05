//! this is for haproxy in the local computer

use crate::{metric::listening_metric, proxy::listening_proxy};
use anyhow::Context;
use clap::Parser;
use config::{Config, Opts};
use std::sync::Arc;
use tracing::error;

use tracing_subscriber::EnvFilter;

mod backends;
mod balance;
mod config;
mod connector;
mod metric;
mod proxy;

fn main() -> anyhow::Result<()> {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Result::Err(e) => {
            error!("cannot init tokio runtime: {}", e);
            return Ok(());
        }
    };

    runtime.block_on(run_main())?;
    Ok(())
}

/// main run function, distribute the listener service, and help selecting backend,
/// and spawn the each transmission action
async fn run_main() -> anyhow::Result<()> {
    let opts = Opts::parse();
    let config = Config::new(opts).context("Could not init config")?;

    let env = EnvFilter::builder()
        .with_default_directive(config.log_level.into())
        .from_env_lossy();

    tracing_subscriber::fmt().with_env_filter(env).init();
    let config = Arc::new(config);

    tokio::try_join!(listening_proxy(config.clone()), listening_metric(config))?;
    Ok(())
}
