//! Minimal pinned-IP HTTP client.
//!
//! `reqwest` would normally resolve the hostname itself. `resolve()` overrides
//! that for one domain, so the connection goes to a chosen edge IP while TLS
//! SNI, certificate hostname verification and the HTTP `Host` header all keep
//! the real name. That is what makes "is this IP good for github.com" a
//! meaningful question instead of "does this IP accept a TCP connection".

use anyhow::{anyhow, Result};
use reqwest::redirect::Policy;
use reqwest::Client;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

pub struct Fetch {
    pub status: u16,
    pub content_type: String,
    /// `Location` header, needed to judge redirect-only alias hostnames.
    pub location: Option<String>,
    pub body: Vec<u8>,
    pub total_ms: f64,
}

const MAX_BODY_HARD_CAP: usize = 16 * 1024 * 1024;

pub fn pinned_client(host: &str, ip: IpAddr, timeout: Duration) -> Result<Client> {
    Client::builder()
        .resolve(host, SocketAddr::new(ip, 443))
        .no_proxy()
        .http1_only()
        .timeout(timeout)
        .connect_timeout(timeout)
        .pool_max_idle_per_host(0)
        .redirect(Policy::none())
        .build()
        .map_err(|e| anyhow!("client build: {e}"))
}

pub async fn fetch(c: &Client, url: &str, max_body: usize, ua: &str) -> Result<Fetch> {
    use futures::StreamExt as _;

    let t0 = Instant::now();
    let resp = c
        .get(url)
        .header(reqwest::header::USER_AGENT, ua)
        .header(reqwest::header::ACCEPT, "*/*")
        // identity: no compression means no transparent decode, so tar.gz and
        // image magic bytes survive to the validator intact.
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await?;

    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if let Some(len) = resp.content_length() {
        if len as usize > MAX_BODY_HARD_CAP {
            return Err(anyhow!("body too large: {len}"));
        }
    }

    // Stream and stop as soon as we have enough. Reading the whole body would
    // make every check hostage to a mid-transfer stall, which is exactly the
    // failure mode that appears under interference on this kind of link.
    let mut body: Vec<u8> = Vec::with_capacity(max_body.min(256 * 1024));
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        body.extend_from_slice(&chunk);
        if body.len() >= max_body {
            break;
        }
    }
    let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if body.len() > max_body {
        body.truncate(max_body);
    }
    Ok(Fetch {
        status,
        content_type,
        location,
        body,
        total_ms,
    })
}

/// Resolve a hostname through the container's resolver, IPv4 only.
pub async fn resolve_v4(host: &str) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    if let Ok(it) = tokio::net::lookup_host((host, 443u16)).await {
        for sa in it {
            let ip = sa.ip();
            if ip.is_ipv4() && !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    if out.is_empty() {
        // A poisoned resolver can return nothing usable; fall back to the
        // system resolver result via the OS route used by `getaddrinfo`.
        if let Ok(it) = tokio::net::lookup_host(format!("{host}:443")).await {
            for sa in it {
                let ip = sa.ip();
                if ip.is_ipv4() && !out.contains(&ip) {
                    out.push(ip);
                }
            }
        }
    }
    out
}
