//! Cloudflare Tunnel provisioning and connector supervision.

use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};

use crate::config::{self, AppConfig, CloudflareConfig, CloudflareCredentials};

const API_BASE: &str = "https://api.cloudflare.com/client/v4";

#[derive(Debug)]
pub struct ProvisionedTunnel {
    pub config: CloudflareConfig,
    pub credentials: CloudflareCredentials,
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    success: bool,
    result: Option<T>,
    #[serde(default)]
    errors: Vec<ApiMessage>,
}

#[derive(Debug, Deserialize)]
struct ApiMessage {
    code: Option<u64>,
    message: String,
}

#[derive(Debug, Deserialize)]
struct Zone {
    id: String,
    name: String,
    account: ZoneAccount,
}

#[derive(Debug, Deserialize)]
struct ZoneAccount {
    id: String,
}

#[derive(Debug, Deserialize)]
struct Tunnel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct DnsRecord {
    id: String,
}

#[derive(Debug, Serialize)]
struct TunnelCreate<'a> {
    name: &'a str,
    config_src: &'static str,
}

/// Create or update a remotely-managed tunnel and its proxied DNS record.
/// The broad API token is deliberately not returned or persisted.
pub fn provision(api_token: &str, hostname: &str, app: &AppConfig) -> Result<ProvisionedTunnel> {
    let hostname = normalize_hostname(hostname)?;
    if api_token.trim().is_empty() {
        bail!("Cloudflare API token is required");
    }
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to create Cloudflare API client")?;

    let zone = find_zone(&client, api_token, &hostname)?;
    let tunnel_name = tunnel_name(&hostname);
    let reusable = app
        .cloudflare
        .as_ref()
        .filter(|existing| existing.account_id == zone.account.id)
        .map(|existing| existing.tunnel_id.clone());
    let tunnel_id = match reusable {
        Some(id) => id,
        None => create_tunnel(&client, api_token, &zone.account.id, &tunnel_name)?,
    };

    let origin = format!("http://127.0.0.1:{}", listen_port(&app.listen_addr));
    put_tunnel_config(
        &client,
        api_token,
        &zone.account.id,
        &tunnel_id,
        &hostname,
        &origin,
    )?;
    upsert_dns(&client, api_token, &zone.id, &hostname, &tunnel_id)?;
    let tunnel_token = get_tunnel_token(&client, api_token, &zone.account.id, &tunnel_id)?;

    Ok(ProvisionedTunnel {
        config: CloudflareConfig {
            hostname,
            account_id: zone.account.id,
            zone_id: zone.id,
            tunnel_id,
            tunnel_name,
        },
        credentials: CloudflareCredentials { tunnel_token },
    })
}

fn find_zone(client: &Client, token: &str, hostname: &str) -> Result<Zone> {
    let labels: Vec<&str> = hostname.split('.').collect();
    for start in 0..labels.len().saturating_sub(1) {
        let candidate = labels[start..].join(".");
        let response: Vec<Zone> = api(client
            .get(format!("{API_BASE}/zones"))
            .bearer_auth(token)
            .query(&[("name", candidate.as_str()), ("status", "active")]))?;
        if let Some(zone) = response.into_iter().find(|zone| zone.name == candidate) {
            return Ok(zone);
        }
    }
    bail!(
        "no active Cloudflare zone matches {hostname}; grant Zone Read and add the domain to Cloudflare"
    )
}

fn create_tunnel(client: &Client, token: &str, account: &str, name: &str) -> Result<String> {
    let tunnel: Tunnel = api(client
        .post(format!("{API_BASE}/accounts/{account}/cfd_tunnel"))
        .bearer_auth(token)
        .json(&TunnelCreate {
            name,
            config_src: "cloudflare",
        }))?;
    Ok(tunnel.id)
}

fn put_tunnel_config(
    client: &Client,
    token: &str,
    account: &str,
    tunnel: &str,
    hostname: &str,
    origin: &str,
) -> Result<()> {
    let body = serde_json::json!({
        "config": {
            "ingress": [
                { "hostname": hostname, "service": origin, "originRequest": {} },
                { "service": "http_status:404" }
            ]
        }
    });
    let _: serde_json::Value = api(client
        .put(format!(
            "{API_BASE}/accounts/{account}/cfd_tunnel/{tunnel}/configurations"
        ))
        .bearer_auth(token)
        .json(&body))?;
    Ok(())
}

fn upsert_dns(
    client: &Client,
    token: &str,
    zone: &str,
    hostname: &str,
    tunnel: &str,
) -> Result<()> {
    let target = format!("{tunnel}.cfargotunnel.com");
    let existing: Vec<DnsRecord> = api(client
        .get(format!("{API_BASE}/zones/{zone}/dns_records"))
        .bearer_auth(token)
        .query(&[("name", hostname), ("type", "CNAME"), ("per_page", "50")]))?;
    let body = serde_json::json!({
        "type": "CNAME",
        "name": hostname,
        "content": target,
        "proxied": true,
        "ttl": 1,
        "comment": "Managed by webhookr"
    });
    let request = match existing.first() {
        Some(record) => client.put(format!("{API_BASE}/zones/{zone}/dns_records/{}", record.id)),
        None => client.post(format!("{API_BASE}/zones/{zone}/dns_records")),
    };
    let _: serde_json::Value = api(request.bearer_auth(token).json(&body))?;
    Ok(())
}

fn get_tunnel_token(client: &Client, token: &str, account: &str, tunnel: &str) -> Result<String> {
    api(client
        .get(format!(
            "{API_BASE}/accounts/{account}/cfd_tunnel/{tunnel}/token"
        ))
        .bearer_auth(token))
}

fn api<T: DeserializeOwned>(request: RequestBuilder) -> Result<T> {
    let response = request.send().context("Cloudflare API request failed")?;
    decode(response)
}

fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    let body = response
        .text()
        .context("failed to read Cloudflare API response")?;
    let envelope: ApiEnvelope<T> = serde_json::from_str(&body)
        .with_context(|| format!("Cloudflare returned an invalid response (HTTP {status})"))?;
    if !status.is_success() || !envelope.success {
        let message = envelope
            .errors
            .iter()
            .map(|error| match error.code {
                Some(code) => format!("{} ({code})", error.message),
                None => error.message.clone(),
            })
            .collect::<Vec<_>>()
            .join("; ");
        bail!(
            "Cloudflare API rejected the request (HTTP {status}): {}",
            if message.is_empty() {
                "unknown error"
            } else {
                &message
            }
        );
    }
    envelope
        .result
        .context("Cloudflare API response had no result")
}

pub fn save(provisioned: ProvisionedTunnel, app: &mut AppConfig) -> Result<()> {
    config::save_cloudflare_credentials(&provisioned.credentials)?;
    app.cloudflare = Some(provisioned.config);
    config::save_config(app)
}

/// Start `cloudflared`, preferring a native binary and falling back to Docker.
pub fn spawn_connector(app: &AppConfig) -> Result<Option<Child>> {
    if app.cloudflare.is_none() {
        return Ok(None);
    }
    let credentials = config::load_cloudflare_credentials()?;
    let mut command = if command_available("cloudflared") {
        let mut command = Command::new("cloudflared");
        command.args(["tunnel", "--no-autoupdate", "run"]);
        command
    } else if command_available("docker") {
        let mut command = Command::new("docker");
        command.args([
            "run",
            "--rm",
            "--network",
            "host",
            "-e",
            "TUNNEL_TOKEN",
            "cloudflare/cloudflared:latest",
            "tunnel",
            "--no-autoupdate",
            "run",
        ]);
        command
    } else {
        bail!("Cloudflare is configured, but neither cloudflared nor docker is installed");
    };
    command
        .env("TUNNEL_TOKEN", credentials.tunnel_token)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command
        .spawn()
        .map(Some)
        .context("failed to start Cloudflare Tunnel connector")
}

pub fn connector_label() -> &'static str {
    if command_available("cloudflared") {
        "cloudflared"
    } else if command_available("docker") {
        "docker/cloudflared"
    } else {
        "unavailable"
    }
}

fn command_available(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn listen_port(address: &str) -> u16 {
    address
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(9000)
}

fn normalize_hostname(value: &str) -> Result<String> {
    let hostname = value.trim().trim_end_matches('.').to_ascii_lowercase();
    if hostname.len() > 253
        || !hostname.contains('.')
        || hostname.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        })
    {
        bail!("enter a hostname such as hooks.example.com (without https:// or a path)");
    }
    Ok(hostname)
}

fn tunnel_name(hostname: &str) -> String {
    format!("webhookr-{}", hostname.replace('.', "-"))
}

#[cfg(test)]
mod tests {
    use super::{listen_port, normalize_hostname, tunnel_name};

    #[test]
    fn hostnames_are_normalized_and_validated() {
        assert_eq!(
            normalize_hostname("Hooks.Example.com.").unwrap(),
            "hooks.example.com"
        );
        assert!(normalize_hostname("https://hooks.example.com").is_err());
        assert!(normalize_hostname("localhost").is_err());
        assert!(normalize_hostname("-hooks.example.com").is_err());
    }

    #[test]
    fn derives_runtime_values() {
        assert_eq!(listen_port("0.0.0.0:9911"), 9911);
        assert_eq!(
            tunnel_name("hooks.example.com"),
            "webhookr-hooks-example-com"
        );
    }
}
