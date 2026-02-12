use anyhow::Result;
use clap::{Parser, Subcommand};
use garedit_ipc::Command;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "gareditctl")]
#[command(about = "Control utility for garedit")]
#[command(version)]
struct Args {
    /// Print the generated IPC command as JSON.
    #[arg(long)]
    json: bool,

    #[command(subcommand)]
    command: CtlCommand,
}

#[derive(Subcommand, Debug)]
enum CtlCommand {
    Open {
        path: PathBuf,
        #[arg(long)]
        line: Option<usize>,
        #[arg(long)]
        column: Option<usize>,
    },
    Show,
    Hide,
    Toggle,
    Status,
    Quit,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let command = to_ipc_command(args.command);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&command)?);
        return Ok(());
    }

    eprintln!("gareditctl IPC transport is planned for sprint 04.");
    eprintln!("Generated command: {}", serde_json::to_string(&command)?);
    Ok(())
}

fn to_ipc_command(command: CtlCommand) -> Command {
    match command {
        CtlCommand::Open { path, line, column } => Command::Open { path, line, column },
        CtlCommand::Show => Command::Show,
        CtlCommand::Hide => Command::Hide,
        CtlCommand::Toggle => Command::Toggle,
        CtlCommand::Status => Command::Status,
        CtlCommand::Quit => Command::Quit,
    }
}
