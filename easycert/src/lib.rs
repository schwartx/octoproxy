use anyhow::Result;
use clap::{Parser, Subcommand};

mod gen;

/// EasyCert commandline args
#[derive(Parser)]
pub struct Cmd {
    #[command(subcommand)]
    cmd: EasyCertCmd,
}

impl Cmd {
    pub fn run(self) -> Result<()> {
        match self.cmd {
            EasyCertCmd::Gen(cmd) => cmd.run(),
        }
    }
}

#[derive(Subcommand)]
enum EasyCertCmd {
    /// generate certicates
    Gen(gen::Gen),
}
