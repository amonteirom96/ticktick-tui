mod api;
mod auth;
mod config;
mod fields;
mod focus;
mod preview;
mod sidebar;
mod sync;
mod token_ui;
mod tree;
mod ui;
mod wellness;
use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
#[derive(Parser)]
#[command(version, about = "Minimal TickTick client with Vim navigation")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    Auth,
    /// Read the local focus timer without credentials or network access
    Focus {
        #[command(subcommand)]
        command: FocusCommand,
    },
    /// Print a complete config for your dotfiles
    Config,
    /// Explore the interface without credentials or network requests
    Demo,
}
#[derive(Subcommand)]
enum FocusCommand {
    /// Display the timer controlled by the TUI (read-only)
    Status {
        #[arg(long, value_enum, default_value = "text")]
        format: StatusFormat,
        /// Equivalent to --format json
        #[arg(long, conflicts_with = "format")]
        json: bool,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum StatusFormat {
    Text,
    Tmux,
    Json,
}
fn main() -> Result<()> {
    match Cli::parse().command {
        Some(Command::Auth) => auth::login(),
        Some(Command::Focus {
            command: FocusCommand::Status { format, json },
        }) => {
            let timer = wellness::Timer::read()?;
            if json || matches!(format, StatusFormat::Json) {
                println!("{}", timer.status());
            } else if matches!(format, StatusFormat::Tmux) {
                let text = timer.status_tmux()?;
                if !text.is_empty() {
                    println!("{text}");
                }
            } else {
                let text = timer.status_text();
                if !text.is_empty() {
                    println!("{text}");
                }
            }
            Ok(())
        }
        Some(Command::Config) => {
            print!("{}", toml::to_string_pretty(&config::Config::default())?);
            Ok(())
        }
        Some(Command::Demo) => ui::run(config::load()?, None, true),
        None => ui::run(
            config::load()?,
            config::token()?.map(api::Api::new).transpose()?,
            false,
        ),
    }
}
