mod api;
mod config;
mod net;
mod page;
mod probe;
mod render;
mod state;
mod store;
mod validators;

use anyhow::Result;
use config::Config;
use render::{render_hosts, VERSION};
use state::AppState;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr so `--once > hosts.txt` captures only the block.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cfg = Config::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Diagnostic / one-shot mode: probe once, persist, print, exit. Useful in
    // CI or a scheduled task without keeping the server up.
    if args.iter().any(|a| a == "--once") {
        let store = store::Store::new(&cfg.state_dir);
        let existing = store.load_optional();
        let prev = existing.as_ref().map(|(s, _, _)| s);
        let cycle_id = prev.map(|s| s.cycle_id + 1).unwrap_or(1);
        let snap = probe::run_cycle(&cfg, cycle_id, prev).await;

        // An explicit --once writes regardless of how this cycle compares: it is
        // a request to record what was just measured. Say what got lost, so a
        // shutdown of a host is visible without diffing the file.
        if let Some(old) = prev {
            let lost = probe::lost_hosts(old, &snap);
            if !lost.is_empty() {
                tracing::warn!(
                    "this cycle lost {} host(s) the stored snapshot had: {}",
                    lost.len(),
                    lost.join(", ")
                );
            }
        }

        let block = render_hosts(&snap, &cfg.mode);
        match store.save(&block, &snap, &cfg.mode) {
            Ok(()) => tracing::info!("state written to {}", store.describe()),
            Err(e) => tracing::error!("could not persist state: {e:#}"),
        }
        print!("{block}");
        return Ok(());
    }
    if args.iter().any(|a| a == "--version") {
        println!("github-hosts {VERSION}");
        return Ok(());
    }

    let state = AppState::new(cfg.clone());
    tracing::info!(
        "github-hosts {VERSION}: interval={}s timeout={:.1}s concurrency={} max_scan={} top_k={} mode={} state={}",
        cfg.interval.as_secs(),
        cfg.timeout.as_secs_f64(),
        cfg.concurrency,
        cfg.max_scan,
        cfg.top_k,
        cfg.mode,
        cfg.state_dir.display()
    );
    if let Err(e) = state.store.ensure_dir() {
        tracing::error!("state dir unavailable, results will not persist: {e:#}");
    }
    // Warm start: a restart should not blank the API until the first cycle ends.
    state.restore().await;

    // Scheduler: run immediately, then on the interval or on demand, never two
    // cycles at once.
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                let cycle_id = state.next_cycle_id();
                tracing::info!("probe cycle {cycle_id} start");
                state.set_probing(true);
                *state.last_cycle_started_at.write().await = Some(chrono::Local::now());
                *state.next_probe_at.write().await = None;

                // Snapshot the previous result first so the new cycle can prefer
                // the addresses already in use.
                let prev = state.published().await;
                let prev_snap = prev.as_ref().map(|p| p.snapshot.clone());
                let snap = probe::run_cycle(&state.cfg, cycle_id, prev_snap.as_ref()).await;
                tracing::info!(
                    "probe cycle {} done: {}/{} verified, git={}, {}ms",
                    cycle_id,
                    snap.verified,
                    snap.expected,
                    snap.git_ok,
                    snap.elapsed_ms
                );

                // Count the failures before the nothing-verified guard: a
                // cycle where the whole network is gone must still raise the
                // per-host streaks, otherwise the one case where "it has been
                // failing for an hour" matters most is the one that stays
                // invisible.
                state.note_cycle(&snap).await;

                // Do not publish a cycle that verified nothing: with FAMILIES=""
                // or all names misspelled, `jobs` is empty, which used to produce
                // verified=0/expected=0/required_ok=true and satisfy the
                // cold-start condition, publishing a header-only block.
                if !probe::publishable(&snap) {
                    tracing::error!(
                        "cycle {cycle_id} verified nothing (check FAMILIES={:?}); not publishing",
                        std::env::var("FAMILIES").unwrap_or_default()
                    );
                    state.set_probing(false);
                    let wait = jitter_delay(state.cfg.interval);
                    *state.next_probe_at.write().await = Some(
                        chrono::Local::now() + chrono::Duration::from_std(wait).unwrap_or_default(),
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = state.refresh.notified() => {}
                    }
                    continue;
                }

                // The newest cycle always replaces the previous one. The old
                // rule -- publish only when the new cycle ranked at least as
                // high -- froze the file as soon as an optional host stopped
                // verifying: the pin for that host stayed on disk long after a
                // cycle had proved it dead, and every other host's freshly
                // verified address was thrown away with it. A stale answer is
                // worse than a smaller honest one.
                if let Some(old) = prev_snap.as_ref() {
                    let lost = probe::lost_hosts(old, &snap);
                    if !lost.is_empty() {
                        tracing::warn!(
                            "cycle {cycle_id} lost {} host(s) the previous cycle had: {}",
                            lost.len(),
                            lost.join(", ")
                        );
                    }
                }
                let required = snap.required_hits();
                let block = render_hosts(&snap, &state.cfg.mode);
                state.publish(snap, block).await;
                tracing::info!(
                    "snapshot published ({required} required host(s)): {}",
                    state.store.describe()
                );

                state.set_probing(false);
                // Jitter so a fleet of instances does not synchronise its
                // requests on the same second.
                let jitter = jitter_delay(state.cfg.interval);
                *state.next_probe_at.write().await = Some(
                    chrono::Local::now() + chrono::Duration::from_std(jitter).unwrap_or_default(),
                );

                tokio::select! {
                    _ = tokio::time::sleep(jitter) => {}
                    _ = state.refresh.notified() => {
                        tracing::info!("refresh requested");
                    }
                }
            }
        });
    }

    let app = api::router(state.clone());
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Spread the schedule by up to ±10% so several instances started together do
/// not keep firing in lockstep and multiplying the load they place on GitHub.
fn jitter_delay(base: Duration) -> Duration {
    let secs = base.as_secs_f64();
    // Cheap deterministic-ish entropy: the nanosecond field of the clock.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as f64)
        .unwrap_or(0.0);
    let unit = nanos / 1_000_000_000.0; // 0.0 .. 1.0
    let factor = 0.9 + 0.2 * unit; // 0.9 .. 1.1
    Duration::from_secs_f64(secs * factor)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutting down");
    tokio::time::sleep(Duration::from_millis(50)).await;
}
