//! Command-line interface for webhookr.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::{self, Write};

use crate::cloudflare;
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
        /// Start the web admin UI for this run only (not persisted)
        #[arg(long)]
        web: bool,
        /// Do not start the web admin UI, even if it is enabled in config
        #[arg(long, conflicts_with = "web")]
        no_web: bool,
        /// Override the admin UI port for this run only
        #[arg(long)]
        web_port: Option<u16>,
    },
    /// Manage the web admin UI
    Web {
        #[command(subcommand)]
        action: WebAction,
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
        /// Git repository URL; cloned automatically when path is absent
        #[arg(long, default_value = "")]
        repository: String,
        /// custom, compose_build, compose_pull, or compose_up
        #[arg(long, default_value = "custom")]
        preset: String,
        /// Compose file relative to the project path
        #[arg(long, default_value = "compose.yaml")]
        compose_file: String,
        /// Compose profile; repeat for multiple profiles
        #[arg(long = "compose-profile")]
        compose_profiles: Vec<String>,
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
        #[arg(long)]
        repository: Option<String>,
        #[arg(long)]
        preset: Option<String>,
        #[arg(long)]
        compose_file: Option<String>,
        #[arg(long = "compose-profile")]
        compose_profiles: Vec<String>,
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
    /// Pull the latest source, then run the project's deployment
    Run {
        #[arg(long)]
        id: String,
        /// Skip the Git update and deploy the checkout as-is
        #[arg(long)]
        no_pull: bool,
    },
    /// Clone or pull the latest source, then deploy it
    Update {
        #[arg(long)]
        id: String,
    },
    /// Provision a Cloudflare Tunnel and public hostname
    Cloudflare {
        /// Scoped API token (or set CLOUDFLARE_API_TOKEN)
        #[arg(long, env = "CLOUDFLARE_API_TOKEN", hide_env_values = true)]
        api_token: String,
        /// Public hostname for the webhook listener, e.g. hooks.example.com.
        /// Omit it to serve everything from --admin-hostname instead.
        #[arg(long)]
        hostname: Option<String>,
        /// Public hostname for the web admin UI. On its own it also serves
        /// webhooks at /webhook/<id>.
        #[arg(long)]
        admin_hostname: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum WebAction {
    /// Turn the admin UI on (persisted to config.json)
    Enable {
        /// Bind address. Loopback still works through a Cloudflare Tunnel.
        #[arg(long, default_value = "127.0.0.1:9001")]
        addr: String,
        /// Public hostname to route to the admin UI through the tunnel
        #[arg(long)]
        hostname: Option<String>,
    },
    /// Turn the admin UI off
    Disable,
    /// Show admin UI status and URLs
    Status,
}

/// Parse args and dispatch to the appropriate module.
pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => tui::run(),
        Some(Commands::Serve {
            port,
            web,
            no_web,
            web_port,
        }) => {
            server::serve(
                port,
                server::WebOverride {
                    enable: web,
                    disable: no_web,
                    port: web_port,
                },
            )
            .await
        }
        Some(cmd) => handle(cmd).await,
    }
}

/// Dispatch a non-`serve` subcommand.
async fn handle(cmd: Commands) -> Result<()> {
    match cmd {
        Commands::Serve { .. } => unreachable!("serve is handled in run()"),
        Commands::Web { action } => cmd_web(action),
        Commands::Status => cmd_status(),
        Commands::List => cmd_list(),
        Commands::Add {
            name,
            id,
            path,
            branch,
            command,
            verify_mode,
            repository,
            preset,
            compose_file,
            compose_profiles,
        } => cmd_add(
            name,
            id,
            path,
            branch,
            command,
            verify_mode,
            repository,
            preset,
            compose_file,
            compose_profiles,
        ),
        Commands::Edit {
            id,
            name,
            path,
            branch,
            command,
            verify_mode,
            repository,
            preset,
            compose_file,
            compose_profiles,
        } => cmd_edit(
            id,
            name,
            path,
            branch,
            command,
            verify_mode,
            repository,
            preset,
            compose_file,
            compose_profiles,
        ),
        Commands::Remove { id, yes } => cmd_remove(id, yes),
        Commands::Key { id, rotate } => cmd_key(id, rotate),
        Commands::Logs { id, lines } => cmd_logs(id, lines),
        Commands::Run { id, no_pull } => cmd_run(id, no_pull).await,
        Commands::Update { id } => cmd_update(id).await,
        Commands::Cloudflare {
            api_token,
            hostname,
            admin_hostname,
        } => cmd_cloudflare(api_token, hostname, admin_hostname),
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
pub(crate) fn slugify(name: &str) -> String {
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
    println!("  repository:  {}", display_or_dash(&p.repository));
    println!("  deployment:  {}", p.preset_label());
    if p.uses_compose() {
        println!("  compose:     {}", p.compose_file);
        println!(
            "  profiles:    {}",
            if p.compose_profiles.is_empty() {
                "-".to_string()
            } else {
                p.compose_profiles.join(",")
            }
        );
    }
}

fn display_or_dash(value: &str) -> &str {
    if value.trim().is_empty() {
        "-"
    } else {
        value
    }
}

fn cmd_status() -> Result<()> {
    let cfg = config::load_config()?;
    let port = parse_port(&cfg.listen_addr);
    println!("listen address: {}", cfg.listen_addr);
    match &cfg.cloudflare {
        Some(tunnel) => println!("public URL: https://{}", tunnel.hostname),
        None => println!("public URL: not configured"),
    }
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
        println!("    webhook:     {}", webhook_url(&cfg, &p.id));
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
    println!(
        "{:<20} {:<20} {:<12} {:<18} {:<10} LAST RUN",
        "ID", "NAME", "BRANCH", "DEPLOYMENT", "VERIFY"
    );
    for p in &cfg.projects {
        let last = state::latest_run(&p.id)
            .map(|r| r.status)
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<20} {:<20} {:<12} {:<18} {:<10} {}",
            p.id,
            p.name,
            p.branch,
            p.preset_label(),
            p.verify_mode,
            last
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_add(
    name: Option<String>,
    id: Option<String>,
    path: Option<String>,
    branch: String,
    command: Option<String>,
    verify_mode: String,
    repository: String,
    preset: String,
    compose_file: String,
    compose_profiles: Vec<String>,
) -> Result<()> {
    let name = required(name, "name")?;
    let path = required(path, "path")?;
    let command = if preset == "custom" {
        required(command, "command")?
    } else {
        command.unwrap_or_default()
    };
    let id = match id {
        Some(i) if !i.trim().is_empty() => i,
        _ => {
            let raw = prompt("id (blank to derive from name)")?;
            if raw.trim().is_empty() {
                slugify(&name)
            } else {
                raw
            }
        }
    };

    let secret = util::generate_secret();
    let mut project = ProjectConfig::new(id, name, path, branch, command, secret, verify_mode);
    project.repository = repository;
    project.deploy_preset = preset;
    project.compose_file = compose_file;
    project.compose_profiles = compose_profiles;
    project.validate()?;

    let mut cfg = config::load_config()?;
    let listen_addr = cfg.listen_addr.clone();
    cfg.upsert(project.clone());
    config::save_config(&cfg)?;

    println!("created project:");
    print_project(&project);
    println!("  secret:      {}", project.secret);
    let public_url = match cfg.cloudflare.as_ref() {
        Some(tunnel) => format!("https://{}/hooks/{}", tunnel.hostname, project.id),
        None => format!("http://{listen_addr}/hooks/{}", project.id),
    };
    println!("  webhook:     {public_url}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_edit(
    id: String,
    name: Option<String>,
    path: Option<String>,
    branch: Option<String>,
    command: Option<String>,
    verify_mode: Option<String>,
    repository: Option<String>,
    preset: Option<String>,
    compose_file: Option<String>,
    compose_profiles: Vec<String>,
) -> Result<()> {
    let mut cfg = config::load_config()?;
    {
        let p = cfg
            .get_mut(&id)
            .context(format!("unknown project id: {id}"))?;
        if let Some(v) = name {
            p.name = v;
        }
        if let Some(v) = path {
            p.path = v;
        }
        if let Some(v) = branch {
            p.branch = v;
        }
        if let Some(v) = command {
            p.command = v;
        }
        if let Some(v) = verify_mode {
            p.verify_mode = v;
        }
        if let Some(v) = repository {
            p.repository = v;
        }
        if let Some(v) = preset {
            p.deploy_preset = v;
        }
        if let Some(v) = compose_file {
            p.compose_file = v;
        }
        if !compose_profiles.is_empty() {
            p.compose_profiles = compose_profiles;
        }
        p.validate()?;
    }
    config::save_config(&cfg)?;

    println!("updated project:");
    print_project(cfg.get(&id).expect("project exists"));
    Ok(())
}

fn cmd_remove(id: String, yes: bool) -> Result<()> {
    let mut cfg = config::load_config()?;
    let p = cfg
        .remove(&id)
        .context(format!("unknown project id: {id}"))?;

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
        let p = cfg
            .get_mut(&id)
            .context(format!("unknown project id: {id}"))?;
        if rotate {
            p.secret = util::generate_secret();
        }
    }
    if rotate {
        config::save_config(&cfg)?;
    }

    let p = cfg.get(&id).expect("project exists");
    println!("secret:  {}", p.secret);
    println!("webhook: {}", webhook_url(&cfg, &p.id));
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

async fn cmd_run(id: String, no_pull: bool) -> Result<()> {
    let cfg = config::load_config()?;
    let p = cfg.get(&id).context(format!("unknown project id: {id}"))?;
    let record = if no_pull {
        executor::deploy_project(p).await?
    } else {
        executor::run_project(p).await?
    };

    print_run_result(record);
    Ok(())
}

async fn cmd_update(id: String) -> Result<()> {
    let cfg = config::load_config()?;
    let p = cfg.get(&id).context(format!("unknown project id: {id}"))?;
    let record = executor::run_project(p).await?;

    print_run_result(record);
    Ok(())
}

fn print_run_result(record: state::RunRecord) {
    if record.status == "success" {
        println!("[ok] success: {}", record.message);
    } else {
        println!("[!!] failed: {}", record.message);
    }
    println!("log: {}", state::run_log_path(&record.id).display());
}

fn cmd_web(action: WebAction) -> Result<()> {
    match action {
        WebAction::Enable { addr, hostname } => {
            config::update_config(|cfg| {
                cfg.web.enabled = true;
                cfg.web.listen_addr = addr.clone();
                if let Some(hostname) = &hostname {
                    cfg.web.hostname = Some(hostname.clone());
                }
                cfg.web.validate(&cfg.listen_addr)
            })?;

            println!("web admin UI enabled on http://{addr}");
            if let Some(hostname) = &hostname {
                println!("public hostname: https://{hostname}");
                println!("run 'webhookr cloudflare --hostname <webhook-host> --admin-hostname {hostname}' to route it");
            }
            println!();
            println!("!! The admin UI has NO authentication. Anyone who can reach it can set a");
            println!("   project's deploy command and run it as this user. Put Cloudflare Access");
            println!("   (or equivalent) in front of it before exposing it.");
            println!();
            println!("restart 'webhookr serve' to start the UI");
        }
        WebAction::Disable => {
            config::update_config(|cfg| {
                cfg.web.enabled = false;
                Ok(())
            })?;
            println!("web admin UI disabled; restart 'webhookr serve' to stop it");
        }
        WebAction::Status => {
            let cfg = config::load_config()?;
            if cfg.web.enabled {
                println!("web admin UI: enabled on http://{}", cfg.web.listen_addr);
            } else {
                println!("web admin UI: disabled");
            }
            match cfg
                .cloudflare
                .as_ref()
                .and_then(|tunnel| tunnel.admin_hostname.as_deref())
                .or(cfg.web.hostname.as_deref())
            {
                Some(hostname) => println!("public hostname: https://{hostname}"),
                None => println!("public hostname: (none)"),
            }
            println!(
                "access header required: {}",
                if cfg.web.require_access_header {
                    "yes"
                } else {
                    "no"
                }
            );
        }
    }
    Ok(())
}

fn cmd_cloudflare(
    api_token: String,
    hostname: Option<String>,
    admin_hostname: Option<String>,
) -> Result<()> {
    let mut cfg = config::load_config()?;
    // Fall back to the hostname already recorded for the admin UI, so a plain
    // re-run does not silently drop the admin ingress rule.
    let admin = admin_hostname.or_else(|| cfg.web.hostname.clone());
    let provisioned = cloudflare::provision(&api_token, hostname.as_deref(), admin.as_deref(), &cfg)?;
    let public_hostname = provisioned.config.hostname.clone();
    let admin_public = provisioned.config.admin_hostname.clone();
    cfg.web.hostname = admin_public.clone();
    cloudflare::save(provisioned, &mut cfg)?;

    if !public_hostname.is_empty() {
        println!("Cloudflare Tunnel configured: https://{public_hostname}");
    }
    if let Some(admin_public) = &admin_public {
        println!("admin UI: https://{admin_public}");
        println!("!! add a Cloudflare Access policy on that hostname — the UI has no login");
    }
    println!("restart 'webhookr serve' to start the connector");
    Ok(())
}

use crate::config::webhook_url;
