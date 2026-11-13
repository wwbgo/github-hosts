//! Probe engine.
//!
//! Two stages, because validating content for every (host, IP) pair is far too
//! many requests:
//!
//! * stage 1 screens a candidate pool per *family* against one representative
//!   hostname and ranks the survivors by latency;
//! * stage 2 runs the full content validation for each real hostname against
//!   its own shortlist, so every host ends up with its own best edge IP.
//!
//! Each hostname therefore keeps its own CDN locality instead of collapsing
//! everything onto whichever node won the race.

use crate::config::Config;
use crate::net::{fetch, pinned_client, resolve_v4};
use crate::validators::*;
use chrono::{DateTime, Local};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Instant;

pub const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                              (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
pub const GIT_UA: &str = "git/2.53.0.windows.3";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    Web,
    Api,
    Raw,
    Assets,
    Pages,
}

impl Family {
    pub const ALL: [Family; 5] = [
        Family::Web,
        Family::Api,
        Family::Raw,
        Family::Assets,
        Family::Pages,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Family::Web => "web",
            Family::Api => "api",
            Family::Raw => "raw",
            Family::Assets => "assets",
            Family::Pages => "pages",
        }
    }

    /// Representative hostname used for the cheap latency screen.
    pub fn screen_host(self) -> &'static str {
        match self {
            Family::Web => "github.com",
            Family::Api => "api.github.com",
            Family::Raw => "raw.githubusercontent.com",
            Family::Assets => "github.githubassets.com",
            Family::Pages => "github.io",
        }
    }

    /// Fastly anycast shards, used only when DNS yields nothing usable.
    pub fn fallback(self) -> &'static [&'static str] {
        match self {
            Family::Web | Family::Api => &[],
            Family::Raw => &[
                "185.199.108.133",
                "185.199.109.133",
                "185.199.110.133",
                "185.199.111.133",
            ],
            Family::Assets => &[
                "185.199.108.215",
                "185.199.109.215",
                "185.199.110.215",
                "185.199.111.215",
            ],
            Family::Pages => &[
                "185.199.108.153",
                "185.199.109.153",
                "185.199.110.153",
                "185.199.111.153",
            ],
        }
    }

    /// Which section of `api.github.com/meta` supplies this family's edges.
    pub fn meta_key(self) -> Option<&'static str> {
        match self {
            Family::Web => Some("web"),
            Family::Api => Some("api"),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct Target {
    pub host: &'static str,
    pub family: Family,
    pub path: &'static str,
    pub validate: Validator,
    pub note: &'static str,
    pub required: bool,
    pub max_body: usize,
    pub ua: &'static str,
}

fn t(
    host: &'static str,
    family: Family,
    path: &'static str,
    validate: Validator,
    note: &'static str,
    required: bool,
    max_body: usize,
) -> Target {
    Target {
        host,
        family,
        path,
        validate,
        note,
        required,
        max_body,
        ua: BROWSER_UA,
    }
}

/// Every endpoint the browser and `git` actually touch.
pub fn targets() -> Vec<Target> {
    vec![
        // ---- page itself ----
        t(
            "github.com",
            Family::Web,
            "/",
            v_html,
            "site html",
            true,
            200_000,
        ),
        // ---- CSS / JS / fonts; without this the page loads unstyled ----
        t(
            "github.githubassets.com",
            Family::Assets,
            "/favicon.ico",
            v_asset,
            "assets",
            true,
            64_000,
        ),
        // ---- REST API ----
        t(
            "api.github.com",
            Family::Api,
            "/zen",
            v_zen,
            "rest api",
            true,
            4_096,
        ),
        // ---- raw blobs, the path behind "download file" links ----
        t(
            "raw.githubusercontent.com",
            Family::Raw,
            "/octocat/Hello-World/master/README",
            v_raw,
            "raw blob",
            true,
            8_192,
        ),
        // ---- avatars: degraded images are cosmetic, never block the block ----
        t(
            "avatars.githubusercontent.com",
            Family::Raw,
            "/u/9919?s=64",
            v_image,
            "avatar",
            false,
            65_536,
        ),
        // ---- source archives, the path behind "Download ZIP" ----
        t(
            "codeload.github.com",
            Family::Web,
            "/octocat/Hello-World/tar.gz/refs/heads/master",
            v_codeload,
            "source archive",
            true,
            8 * 1024 * 1024,
        ),
        // ---- optional hosts: improve fidelity, never block publication ----
        t(
            "www.github.com",
            Family::Web,
            "/",
            v_html_or_redirect,
            "www alias",
            false,
            200_000,
        ),
        t(
            "gist.github.com",
            Family::Web,
            "/",
            v_html_or_redirect,
            "gist",
            false,
            200_000,
        ),
        t(
            "camo.githubusercontent.com",
            Family::Raw,
            "/",
            v_reachable,
            "readme images",
            false,
            1_024,
        ),
        t(
            "user-images.githubusercontent.com",
            Family::Raw,
            "/",
            v_reachable,
            "issue images",
            false,
            1_024,
        ),
        t(
            "objects.githubusercontent.com",
            Family::Raw,
            "/",
            v_reachable,
            "release objects",
            false,
            1_024,
        ),
        t(
            "release-assets.githubusercontent.com",
            Family::Raw,
            "/",
            v_reachable,
            "release assets",
            false,
            1_024,
        ),
        t(
            "media.githubusercontent.com",
            Family::Raw,
            "/",
            v_reachable,
            "lfs media",
            false,
            1_024,
        ),
        t(
            "github.io",
            Family::Pages,
            "/",
            v_reachable,
            "pages",
            false,
            1_024,
        ),
    ]
}

pub const GIT_PATH: &str = "/octocat/Hello-World.git/info/refs?service=git-upload-pack";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub ip: String,
    pub latency_ms: f64,
    pub verified: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub generated_at: DateTime<Local>,
    pub cycle_id: u64,
    pub verified: usize,
    pub expected: usize,
    pub required_ok: bool,
    pub git_ok: bool,
    pub git_detail: String,
    pub elapsed_ms: u64,
    pub endpoints: Vec<Endpoint>,
    pub unverified: Vec<String>,
    /// What the system resolver returned for each hostname during this cycle.
    /// Exposed so `mode=auto` can skip pinning what DNS already gets right, and
    /// so operators can see DNS poisoning at a glance.
    pub dns: BTreeMap<String, Vec<String>>,
}

impl Snapshot {
    /// Hosts that a consumer cannot function without (see `targets()`).
    pub fn required_hits(&self) -> usize {
        self.endpoints
            .iter()
            .filter(|e| e.verified && host_is_required(&e.host))
            .count()
    }
}

/// Whether any target for `host` is marked required, i.e. the block is
/// incomplete without it.
pub fn host_is_required(host: &str) -> bool {
    targets().iter().any(|t| t.host == host && t.required)
}

fn enabled(f: Family) -> bool {
    match std::env::var("FAMILIES") {
        Ok(v) => v
            .split(',')
            .map(|s| s.trim())
            .any(|s| s.eq_ignore_ascii_case(f.key())),
        Err(_) => true,
    }
}

fn slash32(meta: &serde_json::Value, key: &str) -> Vec<IpAddr> {
    meta.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .filter_map(|s| {
                    let (base, len) = s.split_once('/')?;
                    if len != "32" || base.contains(':') {
                        return None;
                    }
                    base.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Official edge ranges. Fetched over a pinned socket so a poisoned resolver
/// cannot feed the candidate pool garbage.
async fn fetch_meta(cfg: &Config) -> Option<serde_json::Value> {
    for ip in resolve_v4("api.github.com").await {
        let Ok(c) = pinned_client("api.github.com", ip, cfg.timeout) else {
            continue;
        };
        let Ok(f) = fetch(
            &c,
            "https://api.github.com/meta",
            2 * 1024 * 1024,
            BROWSER_UA,
        )
        .await
        else {
            continue;
        };
        if f.status == 200 {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&f.body) {
                return Some(v);
            }
        }
    }
    tracing::warn!("api.github.com/meta unavailable, relying on DNS only");
    None
}

/// A candidate that answered the stage-1 screen.
#[derive(Clone, Copy, Debug)]
pub struct Alive {
    pub ip: IpAddr,
    /// HTTP status for the screen host. 200 is a strong signal: the edge
    /// actually serves this hostname. A 400/404 means it answers but does not
    /// host this name (e.g. an api-only node answering for github.com), so it
    /// must rank below any 200 even if it is faster.
    pub status: u16,
    pub ms: f64,
}

impl Alive {
    fn rank(&self) -> (u8, u64) {
        let tier = if self.status == 200 { 0 } else { 1 };
        (tier, (self.ms * 1000.0) as u64)
    }
}

/// Stage 1: is this IP serving this hostname at all, and how fast?
///
/// Deliberately a single attempt per IP: a connect timeout here means a
/// blackholed route, and repeating it just multiplies the delay by the number
/// of dead candidates. Latency is re-measured during stage 2 anyway.
///
/// All families are screened in one flat stream so `concurrency` is a true
/// global bound. Screening them one after another made the slowest part of a
/// cycle additive: five families times ~24 s each is where the observed 80 s
/// cycles came from.
async fn screen_all(cfg: &Config, probes: Vec<(Family, IpAddr)>) -> BTreeMap<Family, Vec<Alive>> {
    let timeout = cfg.timeout;

    let results: Vec<Option<(Family, Alive)>> = stream::iter(probes)
        .map(|(fam, ip)| async move {
            let host = fam.screen_host();
            let url = format!("https://{host}/");
            let c = pinned_client(host, ip, timeout).ok()?;
            let f = fetch(&c, &url, 4_096, BROWSER_UA).await.ok()?;
            Some((
                fam,
                Alive {
                    ip,
                    status: f.status,
                    ms: f.total_ms,
                },
            ))
        })
        .buffer_unordered(cfg.concurrency)
        .collect()
        .await;

    let mut ranked: BTreeMap<Family, Vec<Alive>> = BTreeMap::new();
    for (fam, alive) in results.into_iter().flatten() {
        ranked.entry(fam).or_default().push(alive);
    }
    for v in ranked.values_mut() {
        v.sort_by_key(|a| a.rank());
    }
    ranked
}

/// Stage 2: every target for this hostname must pass on this IP.
///
/// Transport errors are retried (`repeats`), but a *validator* rejection is
/// final: a 301 in place of HTML or a wrong content-type will not change on a
/// second attempt, and retrying only burns the cycle budget.
async fn validate_host(
    cfg: &Config,
    host: &str,
    targets: &[Target],
    ip: IpAddr,
) -> Option<Endpoint> {
    let client = pinned_client(host, ip, cfg.timeout).ok()?;
    let mut details = Vec::with_capacity(targets.len());
    let mut latency = f64::MAX;

    for t in targets {
        let url = format!("https://{host}{}", t.path);
        let mut passed: Option<String> = None;
        let mut last_err = String::new();
        for attempt in 0..cfg.repeats {
            match fetch(&client, &url, t.max_body, t.ua).await {
                Ok(f) => match (t.validate)(&f) {
                    Ok(d) => {
                        latency = latency.min(f.total_ms);
                        passed = Some(d);
                        break;
                    }
                    Err(e) => {
                        last_err = e;
                        break; // deterministic rejection, do not retry
                    }
                },
                Err(e) => {
                    last_err = format!("{e:#}");
                    if attempt + 1 < cfg.repeats {
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    }
                }
            }
        }
        let Some(d) = passed else {
            tracing::debug!("{host} @{ip} rejected on {}: {last_err}", t.note);
            return None;
        };
        details.push(format!("{}={}", t.note, d));
    }

    Some(Endpoint {
        host: host.to_string(),
        ip: ip.to_string(),
        latency_ms: if latency.is_finite() { latency } else { 0.0 },
        verified: true,
        detail: details.join("; "),
    })
}

struct Job {
    host: &'static str,
    family: Family,
    targets: Vec<Target>,
    required: bool,
}

/// Run one full probe cycle.
///
/// `prev` enables stickiness: a host whose previous IP is still reachable keeps
/// that IP even if a slightly faster one exists. Churning addresses every cycle
/// would break in-flight connections and defeat the point of a stable hosts
/// file, so continuity is preferred over shaving off milliseconds.
pub async fn run_cycle(cfg: &Config, cycle_id: u64, prev: Option<&Snapshot>) -> Snapshot {
    let t0 = Instant::now();
    let all = targets();

    // ---- DNS for every hostname ----
    let mut hosts: Vec<&'static str> = all.iter().map(|t| t.host).collect();
    hosts.sort_unstable();
    hosts.dedup();
    let mut dns: BTreeMap<&'static str, Vec<IpAddr>> = BTreeMap::new();
    for h in &hosts {
        dns.insert(*h, resolve_v4(h).await);
    }

    // ---- candidate pools per family ----
    let meta = fetch_meta(cfg).await;
    let mut pools: BTreeMap<Family, Vec<IpAddr>> = BTreeMap::new();
    for fam in Family::ALL {
        if !enabled(fam) {
            continue;
        }
        let mut pool: Vec<IpAddr> = Vec::new();
        // DNS answers first so a max_scan cap never drops them
        for t in all.iter().filter(|t| t.family == fam) {
            for ip in dns.get(t.host).cloned().unwrap_or_default() {
                if !pool.contains(&ip) {
                    pool.push(ip);
                }
            }
        }
        for s in fam.fallback() {
            if let Ok(ip) = s.parse::<IpAddr>() {
                if !pool.contains(&ip) {
                    pool.push(ip);
                }
            }
        }
        if let (Some(m), Some(k)) = (&meta, fam.meta_key()) {
            for ip in slash32(m, k) {
                if !pool.contains(&ip) {
                    pool.push(ip);
                }
            }
        }
        pool.truncate(cfg.max_scan.max(1));
        tracing::info!(
            "family {:<6} hosts={} candidates={}",
            fam.key(),
            all.iter().filter(|t| t.family == fam).count(),
            pool.len()
        );
        pools.insert(fam, pool);
    }

    // ---- stage 1 ----
    // Flatten every (family, candidate) pair into one stream: the per-family
    // loop it replaces made the families' latencies additive.
    let probes: Vec<(Family, IpAddr)> = pools
        .iter()
        .flat_map(|(fam, ips)| ips.iter().map(move |ip| (*fam, *ip)))
        .collect();
    let screened = screen_all(cfg, probes).await;
    let mut ranked: BTreeMap<Family, Vec<Alive>> = BTreeMap::new();
    for fam in pools.keys() {
        let alive = screened.get(fam).cloned().unwrap_or_default();
        let ok200 = alive.iter().filter(|a| a.status == 200).count();
        tracing::info!(
            "family {:<6} reachable={} (200={})",
            fam.key(),
            alive.len(),
            ok200
        );
        ranked.insert(*fam, alive);
    }

    // ---- stage 2 ----
    let mut jobs: Vec<Job> = Vec::new();
    let mut by_host: BTreeMap<&'static str, Vec<Target>> = BTreeMap::new();
    for t in all.iter().cloned() {
        by_host.entry(t.host).or_default().push(t);
    }
    // Hosts whose family was suppressed by `FAMILIES` are never probed, so they
    // must not be counted as verified -- and a suppressed *required* host must
    // fail the cycle, otherwise `FAMILIES=raw` would report required_ok while
    // github.com was never touched.
    let mut suppressed: Vec<(&'static str, bool)> = Vec::new();
    for (host, targets) in by_host {
        let family = targets[0].family;
        let required = targets.iter().any(|t| t.required);
        if !ranked.contains_key(&family) {
            suppressed.push((host, required));
            continue;
        }
        jobs.push(Job {
            host,
            family,
            required,
            targets,
        });
    }

    let timeout = cfg.timeout;
    let repeats = cfg.repeats;
    let concurrency = cfg.concurrency;
    let top_k = cfg.top_k;
    let expected_hosts = jobs.len();
    let cfg2 = cfg.clone();
    let ranked_ref = &ranked;
    let dns_ref = &dns;

    // Candidates are validated in small concurrent waves, ordered 200-first
    // then fastest-first, until one passes or the host's time budget runs out.
    // The budget matters: during a full blockade every candidate costs a
    // connect timeout, and without a cap one host could stall the whole cycle.
    const WAVE: usize = 3;
    const HOST_BUDGET: std::time::Duration = std::time::Duration::from_secs(25);

    // A job runs up to WAVE validations at once, so admitting `concurrency`
    // jobs would put WAVE times that many requests on the wire. Divide instead:
    // CONCURRENCY is documented as the request-level bound, and it should stay
    // one, otherwise a 12 setting silently becomes 36 connects against GitHub.
    let job_parallelism = (concurrency / WAVE).max(1);

    let outcomes: Vec<(&'static str, Option<Endpoint>, bool)> = stream::iter(jobs)
        .map(|job| {
            let sub = cfg2.clone();
            async move {
                // Order matters. A hostname's own resolver answers go first:
                // edges like codeload (20.205.243.165), assets and media have
                // their own dedicated addresses that are absent from the
                // official /32 list and score badly against the family screen
                // host, so a family-only shortlist would never validate them.
                let mut shortlist: Vec<IpAddr> = Vec::new();
                if let Some(own) = dns_ref.get(job.host) {
                    for ip in own {
                        if !shortlist.contains(ip) {
                            shortlist.push(*ip);
                        }
                    }
                }
                let limit = top_k.max(1);
                for a in ranked_ref.get(&job.family).into_iter().flatten() {
                    if shortlist.len() >= limit {
                        break;
                    }
                    if !shortlist.contains(&a.ip) {
                        shortlist.push(a.ip);
                    }
                }
                // Stickiness: if this host's previous IP survived the screen,
                // try it first and keep it.
                if let Some(p) = prev {
                    if let Some(pe) = p
                        .endpoints
                        .iter()
                        .find(|e| e.host == job.host && e.verified)
                    {
                        if let Ok(pip) = pe.ip.parse::<IpAddr>() {
                            if let Some(pos) = shortlist.iter().position(|x| *x == pip) {
                                let ip = shortlist.remove(pos);
                                shortlist.insert(0, ip);
                            }
                        }
                    }
                }
                let deadline = Instant::now() + HOST_BUDGET;
                let mut start = 0usize;
                while start < shortlist.len() && Instant::now() < deadline {
                    let end = (start + WAVE).min(shortlist.len());
                    let batch: Vec<IpAddr> = shortlist[start..end].to_vec();
                    let results: Vec<Option<Endpoint>> = stream::iter(batch)
                        .map(|ip| validate_host(&sub, job.host, &job.targets, ip))
                        .buffer_unordered(WAVE)
                        .collect()
                        .await;
                    let mut ok: Vec<Endpoint> = results.into_iter().flatten().collect();
                    if !ok.is_empty() {
                        ok.sort_by(|a, b| {
                            a.latency_ms
                                .partial_cmp(&b.latency_ms)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                        return (job.host, Some(ok.remove(0)), job.required);
                    }
                    start = end;
                }
                if Instant::now() >= deadline {
                    tracing::debug!("{} hit the {HOST_BUDGET:?} budget", job.host);
                }
                (job.host, None, job.required)
            }
        })
        .buffer_unordered(job_parallelism)
        .collect()
        .await;

    // ---- assemble ----
    let mut endpoints = Vec::new();
    let mut unverified = Vec::new();
    let mut required_ok = true;
    for (host, required) in &suppressed {
        if *required {
            required_ok = false;
        }
        unverified.push(host.to_string());
        tracing::warn!("{host} not probed: family disabled by FAMILIES");
    }
    for (host, ep, required) in outcomes {
        match ep {
            Some(e) => endpoints.push(e),
            None => {
                if required {
                    required_ok = false;
                }
                unverified.push(host.to_string());
                tracing::warn!("{host} could not be verified");
            }
        }
    }
    // stable presentation order
    let order: Vec<&str> = all.iter().map(|t| t.host).collect();
    endpoints.sort_by_key(|e| {
        order
            .iter()
            .position(|h| *h == e.host.as_str())
            .unwrap_or(usize::MAX)
    });
    endpoints.dedup_by(|a, b| a.host == b.host);

    // ---- git over the winning github.com edge ----
    //
    // The upload-pack advertisement for a normal repository is a few hundred
    // KB, so the generic 6 s probe timeout is too tight for it; a slow but
    // healthy edge would otherwise be reported as a git failure.
    let git_timeout = (timeout * 3).max(std::time::Duration::from_secs(15));
    let (git_ok, git_detail) = match endpoints.iter().find(|e| e.host == "github.com") {
        Some(gh) => match gh.ip.parse::<IpAddr>() {
            Ok(ip) => match pinned_client("github.com", ip, git_timeout) {
                Ok(c) => {
                    let url = format!("https://github.com{GIT_PATH}");
                    let mut ok = false;
                    let mut d = String::from("not attempted");
                    for _ in 0..repeats {
                        match fetch(&c, &url, 16_384, GIT_UA).await {
                            Ok(f) => match v_git(&f) {
                                Ok(x) => {
                                    ok = true;
                                    d = x;
                                    break;
                                }
                                Err(e) => d = e,
                            },
                            Err(e) => d = format!("{e:#}"),
                        }
                    }
                    (ok, d)
                }
                Err(e) => (false, format!("{e:#}")),
            },
            Err(e) => (false, format!("bad ip: {e}")),
        },
        None => (false, "no github.com edge selected".to_string()),
    };

    let dns_map: BTreeMap<String, Vec<String>> = dns
        .iter()
        .map(|(h, ips)| (h.to_string(), ips.iter().map(|i| i.to_string()).collect()))
        .collect();

    Snapshot {
        generated_at: Local::now(),
        cycle_id,
        verified: endpoints.len(),
        expected: expected_hosts,
        required_ok,
        git_ok,
        git_detail,
        elapsed_ms: t0.elapsed().as_millis() as u64,
        endpoints,
        unverified,
        dns: dns_map,
    }
}

/// Publish order. Coverage of *required* hosts outranks total coverage: a
/// cycle that verifies 12 optional hosts but loses `github.com` is worse than
/// one that verifies 6, and a plain endpoint count cannot tell them apart.
/// git is the last tiebreak because a block without a working `git` cannot be
/// fetched from at all.
fn rank(s: &Snapshot) -> (usize, usize, bool) {
    (s.required_hits(), s.endpoints.len(), s.git_ok)
}

/// Guard against replacing a good snapshot with a degraded one.
pub fn is_at_least_as_good(old: &Snapshot, new: &Snapshot) -> bool {
    rank(new) >= rank(old)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn snap(hits: &[(&str, bool)], git: bool) -> Snapshot {
        Snapshot {
            generated_at: Local::now(),
            cycle_id: 1,
            verified: hits.len(),
            expected: hits.len(),
            required_ok: true,
            git_ok: git,
            git_detail: String::new(),
            elapsed_ms: 0,
            endpoints: hits
                .iter()
                .map(|(h, _)| Endpoint {
                    host: (*h).to_string(),
                    ip: "1.1.1.1".into(),
                    latency_ms: 1.0,
                    verified: true,
                    detail: String::new(),
                })
                .collect(),
            unverified: Vec::new(),
            dns: BTreeMap::new(),
        }
    }

    #[test]
    fn required_hosts_are_identified_from_targets() {
        assert!(host_is_required("github.com"));
        assert!(host_is_required("codeload.github.com"));
        // Optional by design: a broken avatar must never block publication.
        assert!(!host_is_required("avatars.githubusercontent.com"));
        assert!(!host_is_required("gist.github.com"));
    }

    #[test]
    fn losing_a_required_host_is_worse_than_gaining_optional_ones() {
        // The regression this guards: 13 -> 33 verified but no github.com used
        // to be accepted because only the raw count was compared.
        let good = snap(&[("github.com", true), ("api.github.com", true)], true);
        let wider = snap(
            &[
                ("api.github.com", true),
                ("avatars.githubusercontent.com", true),
                ("camo.githubusercontent.com", true),
                ("media.githubusercontent.com", true),
            ],
            true,
        );
        assert!(
            !is_at_least_as_good(&good, &wider),
            "a block without github.com must not replace one that has it"
        );
    }

    #[test]
    fn more_required_coverage_is_accepted() {
        let few = snap(&[("github.com", true)], true);
        let more = snap(&[("github.com", true), ("api.github.com", true)], true);
        assert!(is_at_least_as_good(&few, &more));
        assert!(!is_at_least_as_good(&more, &few));
    }

    #[test]
    fn equal_coverage_prefers_a_git_working_result() {
        let git_broken = snap(&[("github.com", true)], false);
        let git_ok = snap(&[("github.com", true)], true);
        assert!(is_at_least_as_good(&git_broken, &git_ok));
        assert!(!is_at_least_as_good(&git_ok, &git_broken));
    }

    #[test]
    fn identical_cycles_are_idempotent() {
        let a = snap(&[("github.com", true)], true);
        let b = snap(&[("github.com", true)], true);
        assert!(is_at_least_as_good(&a, &b));
        assert!(is_at_least_as_good(&b, &a));
    }
}
