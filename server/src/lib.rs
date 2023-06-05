use anyhow::{Context, Result};
use std::path::PathBuf;

use config::Config;
use octoproxy_lib::build_rt;
use tracing_subscriber::EnvFilter;

use clap::Parser;

mod config;
mod proxy;

/// Server commandline args
#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cmd {
    /// http proxy server listening address
    #[arg(short = 'l', long = "listen")]
    listen_address: Option<String>,
    /// trusted client cert
    #[arg(long = "auth")]
    auth: Option<PathBuf>,
    /// config
    #[arg(short = 'c', long = "config", default_value = "config.toml")]
    config: PathBuf,
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
        let h2_task = config.run_h2();
        let quic_task = config.run_quic();

        let _ = tokio::join!(quic_task, h2_task);
        Ok(())
    }
}
