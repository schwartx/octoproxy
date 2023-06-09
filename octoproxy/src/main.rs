use anyhow::Result;
use clap::Parser;
use clap::Subcommand;

#[derive(Subcommand)]
enum OctoCmd {
    Client(octoproxy_client::Cmd),
    Server(octoproxy_server::Cmd),
    Easycert(octoproxy_easycert::Cmd),
    Tui(octoproxy_tui::Cmd),
}

impl OctoCmd {
    fn run(self) -> Result<()> {
        match self {
            OctoCmd::Client(cmd) => cmd.run(),
            OctoCmd::Server(cmd) => cmd.run(),
            OctoCmd::Easycert(cmd) => cmd.run(),
            OctoCmd::Tui(cmd) => cmd.run(),
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
