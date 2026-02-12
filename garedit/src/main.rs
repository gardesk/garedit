mod app;

use anyhow::{Context, Result};
use app::{App, AppConfig};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs;
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
    #[arg(long)]
    font_family: Option<String>,
    /// Editor font size
    #[arg(long)]
    font_size: Option<f64>,
    /// Tab width in spaces
    #[arg(long)]
    tab_width: Option<usize>,
    /// Force line numbers on
    #[arg(long, action = clap::ArgAction::SetTrue)]
    line_numbers: bool,
    /// Force line numbers off
    #[arg(long, action = clap::ArgAction::SetTrue)]
    no_line_numbers: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct EditorConfig {
    font_family: String,
    font_size: f64,
    tab_width: usize,
    show_line_numbers: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            font_family: "monospace".to_string(),
            font_size: 14.0,
            tab_width: 4,
            show_line_numbers: true,
        }
    }
}

fn config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("unable to resolve config directory")?;
    Ok(base.join("garedit").join("config.toml"))
}

fn load_or_create_config() -> Result<EditorConfig> {
    let path = config_path()?;

    if path.exists() {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("unable to read {}", path.display()))?;
        match toml::from_str::<EditorConfig>(&raw) {
            Ok(config) => return Ok(config),
            Err(err) => {
                tracing::warn!(
                    "failed to parse {}, using defaults: {}",
                    path.display(),
                    err
                );
                return Ok(EditorConfig::default());
            }
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("unable to create {}", parent.display()))?;
    }

    let config = EditorConfig::default();
    let serialized = toml::to_string_pretty(&config).context("unable to serialize config")?;
    fs::write(&path, serialized).with_context(|| format!("unable to write {}", path.display()))?;
    Ok(config)
}

fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();
    if args.line_numbers && args.no_line_numbers {
        anyhow::bail!("cannot pass both --line-numbers and --no-line-numbers");
    }

    let config = load_or_create_config()?;
    let show_line_numbers = if args.line_numbers {
        true
    } else if args.no_line_numbers {
        false
    } else {
        config.show_line_numbers
    };

    let mut app = App::new(AppConfig {
        width: args.width,
        height: args.height,
        font_family: args.font_family.unwrap_or(config.font_family),
        font_size: args.font_size.unwrap_or(config.font_size),
        tab_width: args.tab_width.unwrap_or(config.tab_width).max(1),
        show_line_numbers,
        file: args.file,
    })?;
    app.run()
}
