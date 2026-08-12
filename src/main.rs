mod cli;
mod config;
mod executor;
mod server;
mod state;
mod tui;
mod util;

#[tokio::main]
async fn main() {
    if let Err(e) = cli::run().await {
        eprintln!("webhookr: {e:#}");
        std::process::exit(1);
    }
}
