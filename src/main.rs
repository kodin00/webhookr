mod cli;
mod cloudflare;
mod config;
mod executor;
mod github;
mod server;
mod state;
mod telegram;
mod tui;
mod update;
mod util;
mod web;

#[tokio::main]
async fn main() {
    if let Err(e) = cli::run().await {
        eprintln!("webhookr: {e:#}");
        std::process::exit(1);
    }
}
