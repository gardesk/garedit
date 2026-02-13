use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use garedit_ipc::{Command, ResponseData};
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::thread;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "gareditctl")]
#[command(about = "Control utility for garedit")]
#[command(version)]
struct Args {
    /// Print the IPC response as JSON.
    #[arg(long)]
    json: bool,
    /// Start `garedit --daemon` automatically if no IPC listener is available.
    #[arg(long)]
    start_daemon: bool,

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
    let can_autostart = matches!(
        args.command,
        CtlCommand::Open { .. } | CtlCommand::Show | CtlCommand::Status
    );
    let command = to_ipc_command(args.command);
    let response =
        send_command_with_optional_daemon_start(&command, args.start_daemon && can_autostart)
            .with_context(|| {
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

fn send_command_with_optional_daemon_start(
    command: &Command,
    allow_autostart: bool,
) -> io::Result<garedit_ipc::Response> {
    match garedit_ipc::send_command(command) {
        Ok(response) => Ok(response),
        Err(err) if allow_autostart && is_ipc_connect_error(&err) => {
            spawn_garedit_daemon()?;
            wait_for_daemon_and_send(command)
        }
        Err(err) => Err(err),
    }
}

fn spawn_garedit_daemon() -> io::Result<()> {
    let binary = std::env::var("GAREDIT_BIN").unwrap_or_else(|_| "garedit".to_string());
    std::process::Command::new(binary)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

fn wait_for_daemon_and_send(command: &Command) -> io::Result<garedit_ipc::Response> {
    let mut last_error: Option<io::Error> = None;
    for _ in 0..20 {
        match garedit_ipc::send_command(command) {
            Ok(response) => return Ok(response),
            Err(err) if is_ipc_connect_error(&err) => {
                last_error = Some(err);
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for garedit daemon socket",
        )
    }))
}

fn is_ipc_connect_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
    )
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
