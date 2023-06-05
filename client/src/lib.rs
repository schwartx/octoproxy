mod backends;
mod balance;
pub mod command;
mod config;
mod connector;
mod metric;
mod proxy;

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use config::Config;
use octoproxy_lib::build_rt;
use tracing_subscriber::EnvFilter;

use crate::{metric::listening_metric, proxy::listening_proxy};

/// Client commandline args
#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cmd {
    /// http proxy server listening port
    #[arg(short = 'l', long = "listen")]
    pub(crate) listen_address: Option<String>,

    /// config
    #[arg(short = 'c', long = "config", default_value = "config.toml")]
    pub(crate) config: PathBuf,
}

impl Cmd {
    pub fn run(self) -> Result<()> {
        let rt = build_rt();
        rt.block_on(self.run_main())
    }

    /// client run function, distribute the listener service, and help selecting backend,
    /// and spawn the each transmission action
    async fn run_main(self) -> Result<()> {
        let config = Config::new(self).context("Could not init config")?;

        let env = EnvFilter::builder()
            .with_default_directive(config.log_level.into())
            .from_env_lossy();

        tracing_subscriber::fmt().with_env_filter(env).init();
        let config = Arc::new(config);

        tokio::try_join!(listening_proxy(config.clone()), listening_metric(config))?;
        Ok(())
    }
}
