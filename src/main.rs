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
        let cycle_id = existing.as_ref().map(|s| s.cycle_id + 1).unwrap_or(1);
        let snap = probe::run_cycle(&cfg, cycle_id, existing.as_ref()).await;

        // Warn before clobbering a better result: running --once during a bad
        // window while the daemon holds a good snapshot would otherwise publish
        // the worse one to disk. The write still happens -- --once is an
        // explicit request -- but never silently.
        if let Some(old) = &existing {
            if !probe::is_at_least_as_good(old, &snap) {
                tracing::warn!(
                    "this cycle is worse than the stored one ({} required / {} total, vs {} / {}); \
                     overwriting {} anyway because --once was requested",
                    snap.required_hits(),
                    snap.verified,
                    old.required_hits(),
                    old.verified,
                    store.hosts_path().display()
                );
            }
        }

        let block = render_hosts(&snap, &cfg.mode);
        match store.save(&block, &snap) {
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
        "github-hosts {VERSION}: interval={}s timeout={}s concurrency={} max_scan={} top_k={} mode={} state={}",
        cfg.interval.as_secs(),
        cfg.timeout.as_secs(),
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
                *state.next_probe_at.write().await = None;

                // Snapshot the previous result first so the new cycle can prefer
                // the addresses already in use.
                let prev = state.current.read().await.clone();
                let snap = probe::run_cycle(&state.cfg, cycle_id, prev.as_deref()).await;
                tracing::info!(
                    "probe cycle {} done: {}/{} verified, git={}, {}ms",
                    cycle_id,
                    snap.verified,
                    snap.expected,
                    snap.git_ok,
                    snap.elapsed_ms
                );

                let publish = match prev.as_deref() {
                    None => snap.required_ok || snap.verified > 0,
                    Some(old) => probe::is_at_least_as_good(old, &snap),
                };
                if publish {
                    let block = render_hosts(&snap, &state.cfg.mode);
                    state.publish(snap, &block).await;
                    tracing::info!("snapshot published to {}", state.store.describe());
                } else {
                    // The persisted file keeps the previous good result, which
                    // is the whole point of not overwriting on a worse cycle.
                    tracing::warn!("cycle worse than current snapshot; keeping previous");
                }

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
