mod app;

use anyhow::Result;
use app::{App, AppConfig};
use clap::Parser;
use std::path::PathBuf;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(name = "garedit")]
#[command(about = "Prototype native text editor for gardesk")]
#[command(version)]
struct Args {
    /// Optional file to open on startup.
    file: Option<PathBuf>,

    /// Initial window width
    #[arg(long, default_value_t = 980)]
    width: u32,
    /// Initial window height
    #[arg(long, default_value_t = 680)]
    height: u32,
    /// Editor font family
    #[arg(long, default_value = "monospace")]
    font_family: String,
    /// Editor font size
    #[arg(long, default_value_t = 14.0)]
    font_size: f64,
}

fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();
    let mut app = App::new(AppConfig {
        width: args.width,
        height: args.height,
        font_family: args.font_family,
        font_size: args.font_size,
        file: args.file,
    })?;
    app.run()
}
