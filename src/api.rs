//! HTTP surface.
//!
//! `/hosts` is served from the files written by the probe (`hosts.txt` and
//! `snapshot.json`), so a client always receives exactly what is on disk. The
//! in-memory snapshot is only a fallback for the window where persistence is
//! failing, and the source for derived views (custom mode or family filter),
//! which are rendered from the persisted JSON.

use crate::config::Config;
use crate::page;
use crate::probe::Snapshot;
use crate::render::content_hash;
use crate::render::render_hosts;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/hosts", get(hosts))
        .route("/v1/hosts", get(hosts))
        .route("/v1/snapshot", get(snapshot))
        .route("/v1/refresh", post(refresh))
        .route("/healthz", get(healthz))
        .route("/livez", get(livez))
        .with_state(state)
}

#[derive(Deserialize)]
struct HostsQuery {
    #[serde(default)]
    mode: Option<String>,
    /// Restrict output to one or more families, e.g. `?families=web,raw`.
    #[serde(default)]
    families: Option<String>,
}

fn family_of(host: &str) -> &'static str {
    match host {
        "github.com" | "www.github.com" | "gist.github.com" | "codeload.github.com" => "web",
        "api.github.com" => "api",
        "github.githubassets.com" => "assets",
        "github.io" => "pages",
        _ => "raw",
    }
}

/// Load the snapshot the way `/hosts` needs it: prefer the file on disk, fall
/// back to memory only when the file cannot be read.
async fn load_snapshot(st: &AppState) -> Option<Snapshot> {
    match st.store.read_snapshot() {
        Ok(s) => Some(s),
        Err(e) => {
            let mem = st.snapshot().await;
            if mem.is_some() {
                tracing::warn!("state file unreadable, serving memory copy: {e:#}");
            }
            mem.as_deref().cloned()
        }
    }
}

/// The `mode=` value baked into a rendered block's header line, if present.
///
/// Needed because the file on disk was rendered under whatever mode was
/// configured *then*. Comparing the request against the current config alone is
/// wrong: switch `HOSTS_MODE` from `pin` to `auto`, restart, and a `?mode=auto`
/// request would match the config and be served the stale `pin` block.
fn block_mode(block: &str) -> Option<&str> {
    let line = block.lines().next()?;
    let start = line.find("mode=")? + "mode=".len();
    let rest = &line[start..];
    let end = rest.find([' ', ':']).unwrap_or(rest.len());
    Some(&rest[..end])
}

async fn hosts(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    q: Query<HostsQuery>,
) -> Response {
    let mode = q.mode.clone().unwrap_or_else(|| st.cfg.mode.clone());

    // Fast path: the default view is exactly the file the probe wrote, so it is
    // served verbatim without parsing or re-rendering -- but only when the file
    // was actually rendered in the mode being asked for.
    let mut from_file: Option<String> = None;
    if q.families.is_none() {
        if let Ok(b) = st.store.read_hosts() {
            if !b.trim().is_empty() && block_mode(&b) == Some(mode.as_str()) {
                from_file = Some(b);
            } else if !b.trim().is_empty() {
                tracing::debug!(
                    "hosts.txt was rendered as {:?}, request wants {mode:?}; re-rendering",
                    block_mode(&b)
                );
            }
        }
    }

    let mut body = match from_file {
        Some(b) => b,
        None => {
            let Some(snap) = load_snapshot(&st).await else {
                // Nothing verified yet: refuse to emit an empty block rather
                // than let a consumer install a hosts file with no entries.
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    format!(
                        "{}\n# probe cycle has not completed yet; retry shortly\n# <<< github-hosts <<<\n",
                        crate::render::BLOCK_BEGIN
                    ),
                )
                    .into_response();
            };
            if let Some(fams) = &q.families {
                let want: Vec<&str> = fams.split(',').map(|s| s.trim()).collect();
                let mut filtered = snap.clone();
                filtered
                    .endpoints
                    .retain(|e| want.contains(&family_of(&e.host)));
                filtered.unverified.retain(|h| want.contains(&family_of(h)));
                filtered.verified = filtered.endpoints.len();
                filtered.expected = filtered.endpoints.len() + filtered.unverified.len();
                render_hosts(&filtered, &mode)
            } else {
                render_hosts(&snap, &mode)
            }
        }
    };
    if !body.ends_with('\n') {
        body.push('\n');
    }

    let etag = content_hash(&body);
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if inm.trim() == etag || inm.trim().trim_start_matches("W/") == etag {
            return (
                StatusCode::NOT_MODIFIED,
                [
                    (header::ETAG, etag.as_str()),
                    (header::CACHE_CONTROL, "public, max-age=60"),
                ],
            )
                .into_response();
        }
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::ETAG, etag.as_str()),
            (header::CACHE_CONTROL, "public, max-age=60"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response()
}

async fn snapshot(State(st): State<Arc<AppState>>) -> Response {
    match load_snapshot(&st).await {
        Some(s) => Json(s).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no snapshot yet"})),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
struct RefreshAck {
    accepted: bool,
    current_cycle: u64,
    /// Set when the request was declined; explains why.
    #[serde(skip_serializing_if = "Option::is_none")]
    rejected: Option<String>,
    /// Seconds until a refresh will be accepted, when the request was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_secs: Option<i64>,
}

/// Minimum spacing between two cycle starts, whatever triggered them.
///
/// `Notify` coalesces into a permit, so without this an unauthenticated caller
/// could keep the prober running back to back forever, defeating the whole
/// point of the interval floor. One probe cycle per minute is still far more
/// than the measurements need; the configured interval governs the normal path.
fn refresh_floor(cfg: &Config) -> std::time::Duration {
    std::time::Duration::from_secs(60).min(cfg.interval)
}

async fn refresh(State(st): State<Arc<AppState>>) -> Response {
    let cycle = st.cycle.load(std::sync::atomic::Ordering::Relaxed);

    if st.is_probing() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(RefreshAck {
                accepted: false,
                current_cycle: cycle,
                rejected: Some("a cycle is already running".into()),
                retry_after_secs: Some(30),
            }),
        )
            .into_response();
    }

    if let Some(started) = *st.last_cycle_started_at.read().await {
        let elapsed = chrono::Local::now() - started;
        let floor = refresh_floor(&st.cfg);
        if let Ok(elapsed) = elapsed.to_std() {
            if elapsed < floor {
                let wait = (floor - elapsed).as_secs() as i64;
                tracing::info!("refresh refused: {wait}s of the floor remaining");
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(RefreshAck {
                        accepted: false,
                        current_cycle: cycle,
                        rejected: Some("minimum interval between probe cycles not reached".into()),
                        retry_after_secs: Some(wait),
                    }),
                )
                    .into_response();
            }
        }
    }

    st.refresh.notify_one();
    Json(RefreshAck {
        accepted: true,
        current_cycle: cycle,
        rejected: None,
        retry_after_secs: None,
    })
    .into_response()
}

/// Liveness only: is this process serving HTTP at all?
///
/// Deliberately separate from `/healthz`. `/healthz` reports whether the last
/// probe produced a fully verified block, and during a blockade it answers 503
/// *because the tool is working correctly*. Pointing a container HEALTHCHECK at
/// that makes the container look broken exactly when it is doing its job, and a
/// restarter would loop it forever. Containers should probe this endpoint;
/// humans and dashboards should read `/healthz`.
async fn livez() -> Response {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "status": "alive" })),
    )
        .into_response()
}

async fn healthz(State(st): State<Arc<AppState>>) -> Response {
    match load_snapshot(&st).await {
        Some(s) => {
            let code = if s.required_ok && s.git_ok {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            let save_error = st.last_save_error.read().await.clone();
            (
                code,
                Json(serde_json::json!({
                    "status": if code == StatusCode::OK { "ok" } else { "degraded" },
                    "cycle": s.cycle_id,
                    "generated_at": s.generated_at.to_rfc3339(),
                    "verified": s.verified,
                    "required_ok": s.required_ok,
                    "git_ok": s.git_ok,
                    "elapsed_ms": s.elapsed_ms,
                    "interval_secs": st.cfg.interval.as_secs(),
                    "state_dir": st.cfg.state_dir.display().to_string(),
                    "persisted": save_error.is_none(),
                    "persist_error": save_error,
                })),
            )
                .into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"status": "starting"})),
        )
            .into_response(),
    }
}

async fn index(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let snap = load_snapshot(&st).await;
    let probing = st.is_probing();
    let next = *st.next_probe_at.read().await;
    let interval = st.cfg.interval.as_secs();
    let mode = st.cfg.mode.clone();
    let state_desc = st.store.describe();
    let save_error = st.last_save_error.read().await.clone();

    // Content negotiation: a browser gets the status page, a shell gets text.
    let wants_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/html"))
        .unwrap_or(false);

    if wants_html {
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            page::status_html(
                snap.as_ref(),
                probing,
                next,
                interval,
                &mode,
                &state_desc,
                save_error.as_deref(),
            ),
        )
            .into_response()
    } else {
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            page::status_text(
                snap.as_ref(),
                probing,
                next,
                interval,
                &mode,
                &state_desc,
                save_error.as_deref(),
            ),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_mode_reads_the_rendered_mode() {
        let pin = "# >>> github-hosts :: 2026-11-13T12:00:00+08:00 :: mode=pin :: v1.1.0 >>>\n140.82.121.3 github.com\n";
        assert_eq!(block_mode(pin), Some("pin"));

        let auto = "# >>> github-hosts :: 2026-11-13T12:00:00+08:00 :: mode=auto :: v1.1.0 >>>\n";
        assert_eq!(block_mode(auto), Some("auto"));
    }

    #[test]
    fn block_mode_distinguishes_modes_so_a_stale_file_is_not_served() {
        // The regression this guards: switching HOSTS_MODE from pin to auto and
        // asking for ?mode=auto used to compare against the *config* and serve
        // the stale pin file verbatim.
        let pin_file = "# >>> github-hosts :: t :: mode=pin :: v1.1.0 >>>\n";
        assert_ne!(block_mode(pin_file), Some("auto"));
    }

    #[test]
    fn block_mode_tolerates_junk() {
        assert_eq!(block_mode(""), None);
        assert_eq!(block_mode("no header here"), None);
        // A truncated header must not panic.
        assert_eq!(block_mode("# >>> github-hosts :: mode="), Some(""));
    }
}
