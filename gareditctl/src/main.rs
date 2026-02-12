use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use garedit_ipc::{Command, ResponseData};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "gareditctl")]
#[command(about = "Control utility for garedit")]
#[command(version)]
struct Args {
    /// Print the IPC response as JSON.
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
    let response = garedit_ipc::send_command(&command).with_context(|| {
        format!(
            "failed to connect to {}",
            garedit_ipc::socket_path().display()
        )
    })?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        print_human_response(&response);
    }

    if !response.success {
        anyhow::bail!(
            response
                .error
                .unwrap_or_else(|| "request failed".to_string())
        );
    }
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

fn print_human_response(response: &garedit_ipc::Response) {
    if let Some(data) = &response.data {
        match data {
            ResponseData::Status {
                visible,
                open_documents,
                focused_document,
            } => {
                let focused = focused_document
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "[scratch]".to_string());
                println!("visible: {visible}");
                println!("open_documents: {open_documents}");
                println!("focused_document: {focused}");
            }
        }
        return;
    }

    if response.success {
        println!("ok");
    } else {
        println!(
            "error: {}",
            response
                .error
                .as_deref()
                .unwrap_or("request failed without message")
        );
    }
}
