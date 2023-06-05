use anyhow::Result;
use clap::Parser;
use clap::Subcommand;

#[derive(Subcommand)]
enum OctoCmd {
    Client(octoproxy_client::Cmd),
    Server(octoproxy_server::Cmd),
    EasyCert(octoproxy_easycert::Cmd),
    TUI(octoproxy_tui::Cmd),
}

impl OctoCmd {
    fn run(self) -> Result<()> {
        match self {
            OctoCmd::Client(cmd) => cmd.run(),
            OctoCmd::Server(cmd) => cmd.run(),
            OctoCmd::EasyCert(cmd) => cmd.run(),
            OctoCmd::TUI(cmd) => cmd.run(),
        }
    }
}

#[derive(Parser)]
struct Octo {
    #[command(subcommand)]
    cmd: OctoCmd,
}

fn main() -> Result<()> {
    Octo::parse().cmd.run()
}
