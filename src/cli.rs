//! Command-line interface for webhookr.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::{self, Write};

use crate::config::{self, ProjectConfig};
use crate::executor;
use crate::server;
use crate::state;
use crate::tui;
use crate::util;

#[derive(Parser)]
#[command(name = "webhookr", version, about = "Self-hosted webhook runner", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run the webhook daemon in the foreground (use systemd to daemonize)
    Serve {
        /// Override the listen port
        #[arg(long, short)]
        port: Option<u16>,
    },
    /// Show daemon + project status
    Status,
    /// List configured projects
    List,
    /// Add a new project (prompts for anything not provided)
    Add {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long)]
        command: Option<String>,
        #[arg(long, default_value = "github")]
        verify_mode: String,
    },
    /// Edit an existing project
    Edit {
        #[arg(long)]
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        verify_mode: Option<String>,
    },
    /// Remove a project
    Remove {
        #[arg(long)]
        id: String,
        #[arg(long)]
        yes: bool,
    },
    /// Show or rotate a project's webhook secret
    Key {
        #[arg(long)]
        id: String,
        #[arg(long)]
        rotate: bool,
    },
    /// Tail the latest run log for a project
    Logs {
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 50)]
        lines: usize,
    },
    /// Manually trigger a project's pull + command
    Run {
        #[arg(long)]
        id: String,
    },
}

/// Parse args and dispatch to the appropriate module.
pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => tui::run(),
        Some(Commands::Serve { port }) => server::serve(port).await,
        Some(cmd) => handle(cmd).await,
    }
}

/// Dispatch a non-`serve` subcommand.
async fn handle(cmd: Commands) -> Result<()> {
    match cmd {
        Commands::Serve { .. } => unreachable!("serve is handled in run()"),
        Commands::Status => cmd_status(),
        Commands::List => cmd_list(),
        Commands::Add { name, id, path, branch, command, verify_mode } => {
            cmd_add(name, id, path, branch, command, verify_mode)
        }
        Commands::Edit { id, name, path, branch, command, verify_mode } => {
            cmd_edit(id, name, path, branch, command, verify_mode)
        }
        Commands::Remove { id, yes } => cmd_remove(id, yes),
        Commands::Key { id, rotate } => cmd_key(id, rotate),
        Commands::Logs { id, lines } => cmd_logs(id, lines),
        Commands::Run { id } => cmd_run(id).await,
    }
}

/// Parse the port out of a `host:port` listen address, defaulting to 9000.
fn parse_port(addr: &str) -> u16 {
    addr.rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .unwrap_or(9000)
}

/// Read a trimmed line from stdin.
fn read_line() -> Result<String> {
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

/// Prompt for a value, printing `label: ` first.
fn prompt(label: &str) -> Result<String> {
    print!("{label}: ");
    io::stdout().flush()?;
    read_line()
}

/// Resolve a required field from a flag, prompting when it's missing/empty.
fn required(value: Option<String>, label: &str) -> Result<String> {
    match value {
        Some(v) if !v.trim().is_empty() => Ok(v),
        _ => prompt(label),
    }
}

/// Derive a URL slug from a name: lowercase, spaces to `-`, strip everything
/// that isn't alphanumeric, `-`, or `_`.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else if c == ' ' {
            out.push('-');
        }
    }
    out
}

fn print_project(p: &ProjectConfig) {
    println!("  id:          {}", p.id);
    println!("  name:        {}", p.name);
    println!("  path:        {}", p.path);
    println!("  branch:      {}", p.branch);
    println!("  command:     {}", p.command);
    println!("  verify_mode: {}", p.verify_mode);
}

fn cmd_status() -> Result<()> {
    let cfg = config::load_config()?;
    let port = parse_port(&cfg.listen_addr);
    println!("listen address: {}", cfg.listen_addr);
    let running = std::net::TcpStream::connect(("127.0.0.1", port)).is_ok();
    if running {
        println!("daemon: running (port {port})");
    } else {
        println!("daemon: not running");
    }

    if cfg.projects.is_empty() {
        println!("no projects configured");
        return Ok(());
    }

    for p in &cfg.projects {
        let last = state::latest_run(&p.id)
            .map(|r| r.status)
            .unwrap_or_else(|| "no runs".to_string());
        println!();
        println!("  {} ({})", p.name, p.id);
        println!("    webhook:     http://{}/hooks/{}", cfg.listen_addr, p.id);
        println!("    branch:      {}", p.branch);
        println!("    command:     {}", p.command);
        println!("    verify_mode: {}", p.verify_mode);
        println!("    last run:    {last}");
    }
    Ok(())
}

fn cmd_list() -> Result<()> {
    let cfg = config::load_config()?;
    if cfg.projects.is_empty() {
        println!("no projects configured");
        return Ok(());
    }
    println!("{:<20} {:<20} {:<12} {:<24} {:<10} {}", "ID", "NAME", "BRANCH", "COMMAND", "VERIFY", "LAST RUN");
    for p in &cfg.projects {
        let last = state::latest_run(&p.id)
            .map(|r| r.status)
            .unwrap_or_else(|| "-".to_string());
        println!("{:<20} {:<20} {:<12} {:<24} {:<10} {}", p.id, p.name, p.branch, p.command, p.verify_mode, last);
    }
    Ok(())
}

fn cmd_add(
    name: Option<String>,
    id: Option<String>,
    path: Option<String>,
    branch: String,
    command: Option<String>,
    verify_mode: String,
) -> Result<()> {
    let name = required(name, "name")?;
    let path = required(path, "path")?;
    let command = required(command, "command")?;
    let id = match id {
        Some(i) if !i.trim().is_empty() => i,
        _ => {
            let raw = prompt("id (blank to derive from name)")?;
            if raw.trim().is_empty() { slugify(&name) } else { raw }
        }
    };

    let secret = util::generate_secret();
    let project = ProjectConfig::new(id, name, path, branch, command, secret, verify_mode);
    project.validate()?;

    let mut cfg = config::load_config()?;
    let listen_addr = cfg.listen_addr.clone();
    cfg.upsert(project.clone());
    config::save_config(&cfg)?;

    println!("created project:");
    print_project(&project);
    println!("  secret:      {}", project.secret);
    println!("  webhook:     http://{listen_addr}/hooks/{}", project.id);
    Ok(())
}

fn cmd_edit(
    id: String,
    name: Option<String>,
    path: Option<String>,
    branch: Option<String>,
    command: Option<String>,
    verify_mode: Option<String>,
) -> Result<()> {
    let mut cfg = config::load_config()?;
    {
        let p = cfg.get_mut(&id).context(format!("unknown project id: {id}"))?;
        if let Some(v) = name { p.name = v; }
        if let Some(v) = path { p.path = v; }
        if let Some(v) = branch { p.branch = v; }
        if let Some(v) = command { p.command = v; }
        if let Some(v) = verify_mode { p.verify_mode = v; }
        p.validate()?;
    }
    config::save_config(&cfg)?;

    println!("updated project:");
    print_project(cfg.get(&id).expect("project exists"));
    Ok(())
}

fn cmd_remove(id: String, yes: bool) -> Result<()> {
    let mut cfg = config::load_config()?;
    let p = cfg.remove(&id).context(format!("unknown project id: {id}"))?;

    if !yes {
        print!("Remove {}? [y/N]: ", p.name);
        io::stdout().flush()?;
        let answer = read_line()?.to_lowercase();
        if answer != "y" && answer != "yes" {
            println!("cancelled");
            return Ok(());
        }
    }

    config::save_config(&cfg)?;
    println!("removed project: {} ({})", p.name, p.id);
    Ok(())
}

fn cmd_key(id: String, rotate: bool) -> Result<()> {
    let mut cfg = config::load_config()?;
    {
        let p = cfg.get_mut(&id).context(format!("unknown project id: {id}"))?;
        if rotate {
            p.secret = util::generate_secret();
        }
    }
    if rotate {
        config::save_config(&cfg)?;
    }

    let p = cfg.get(&id).expect("project exists");
    println!("secret:  {}", p.secret);
    println!("webhook: http://{}/hooks/{}", cfg.listen_addr, p.id);
    Ok(())
}

fn cmd_logs(id: String, lines: usize) -> Result<()> {
    let run = state::latest_run(&id);
    let Some(run) = run else {
        println!("no runs yet");
        return Ok(());
    };

    let log = state::read_run_log(&run.id);
    let all_lines: Vec<&str> = log.lines().collect();
    let start = all_lines.len().saturating_sub(lines);
    for line in &all_lines[start..] {
        println!("{line}");
    }
    Ok(())
}

async fn cmd_run(id: String) -> Result<()> {
    let cfg = config::load_config()?;
    let p = cfg.get(&id).context(format!("unknown project id: {id}"))?;
    let record = executor::run_project(p).await?;

    if record.status == "success" {
        println!("✓ success: {}", record.message);
    } else {
        println!("✗ failed: {}", record.message);
    }
    println!("log: {}", state::run_log_path(&record.id).display());
    Ok(())
}
