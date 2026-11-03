mod api;
mod auth;
mod config;
mod preview;
mod sidebar;
mod sync;
mod token_ui;
mod tree;
mod ui;
use anyhow::Result;
use clap::{Parser, Subcommand};
#[derive(Parser)]
#[command(version, about = "Minimal TickTick client with Vim navigation")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    Auth,
    /// Print a complete config for your dotfiles
    Config,
    /// Explore the interface without credentials or network requests
    Demo,
}
fn main() -> Result<()> {
    match Cli::parse().command {
        Some(Command::Auth) => auth::login(),
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
