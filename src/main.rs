use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

mod config;
mod mail;
mod store;
mod ui;

#[derive(Parser)]
#[command(name = "epost", about = "Linux maildir email reader/composer")]
struct Args {
    /// Path to the TOML config file (default: $XDG_CONFIG_HOME/epost/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let path = args.config.unwrap_or_else(config::default_path);

    let cfg = match config::load(&path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("epost: failed to load config: {e:#}");
            return ExitCode::from(2);
        }
    };

    // TUI scaffold lands in build step 1 of the new (post-pivot) DESIGN.md.
    // For now, prove the binary boots, the config loads, and exit.
    println!(
        "epost: config loaded from {} ({} account(s)). TUI scaffold pending.",
        path.display(),
        cfg.accounts.len(),
    );
    ExitCode::SUCCESS
}
