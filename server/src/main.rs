use config::{Config, Opts};
use tracing_subscriber::EnvFilter;

use clap::Parser;

mod config;
mod proxy;

fn main() -> anyhow::Result<()> {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Result::Err(e) => {
            eprintln!("cannot init tokio runtime: {}", e);
            return Ok(());
        }
    };

    runtime.block_on(run_main())?;
    Ok(())
}

async fn run_main() -> anyhow::Result<()> {
    let opts = Opts::parse();
    let config = Config::new(opts)?;

    let env = EnvFilter::builder()
        .with_default_directive(config.log_level.into())
        .from_env_lossy();

    tracing_subscriber::fmt().with_env_filter(env).init();

    let h2_task = config.run_h2();
    let quic_task = config.run_quic();

    let _ = tokio::join!(quic_task, h2_task);

    Ok(())
}
