//! Shared state: the last published snapshot, on-disk persistence, and the
//! manual refresh trigger.
//!
//! The snapshot is held in memory *and* mirrored to `hosts.txt` / `snapshot.json`
//! by `Store`. The HTTP layer reads the files, so what a client receives is
//! always exactly what the last successful cycle wrote; the in-memory copy
//! exists for restart continuity, the sticky-IP preference and the status page.

use crate::config::Config;
use crate::probe::Snapshot;
use crate::store::Store;
use chrono::{DateTime, Local};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Notify, RwLock};

pub struct AppState {
    pub cfg: Config,
    pub store: Store,
    /// Last published snapshot. Only replaced when the new cycle is at least
    /// as good as the old one, so consumers never see a degraded file.
    pub current: RwLock<Option<Arc<Snapshot>>>,
    /// Wakes the scheduler early (POST /v1/refresh).
    pub refresh: Notify,
    pub cycle: AtomicU64,
    /// When the next probe is due. Published by the scheduler so the status
    /// page can say "next at ..." without guessing.
    pub next_probe_at: RwLock<Option<DateTime<Local>>>,
    /// When the current/last cycle started. `POST /v1/refresh` consults this so
    /// a caller cannot drive back-to-back cycles and defeat the interval floor.
    pub last_cycle_started_at: RwLock<Option<DateTime<Local>>>,
    /// Set while a cycle is running, so the page can distinguish "idle" from
    /// "probing right now".
    pub probing: AtomicBool,
    /// Outcome of the most recent persist attempt, surfaced on the status page.
    pub last_save_error: RwLock<Option<String>>,
}

impl AppState {
    pub fn new(cfg: Config) -> Arc<Self> {
        let store = Store::new(&cfg.state_dir);
        Arc::new(Self {
            cfg,
            store,
            current: RwLock::new(None),
            refresh: Notify::new(),
            cycle: AtomicU64::new(0),
            next_probe_at: RwLock::new(None),
            last_cycle_started_at: RwLock::new(None),
            probing: AtomicBool::new(false),
            last_save_error: RwLock::new(None),
        })
    }

    pub async fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.current.read().await.clone()
    }

    pub fn next_cycle_id(&self) -> u64 {
        self.cycle.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn is_probing(&self) -> bool {
        self.probing.load(Ordering::Relaxed)
    }

    pub fn set_probing(&self, v: bool) {
        self.probing.store(v, Ordering::Relaxed);
    }

    /// Publish a snapshot and mirror it to disk.
    ///
    /// The in-memory copy is installed even when the write fails: a read-only
    /// volume should cost durability, not take the running service down.
    pub async fn publish(&self, snap: Snapshot, block: &str) {
        let saved = self.store.save(block, &snap);
        {
            let mut err = self.last_save_error.write().await;
            match saved {
                Ok(()) => {
                    if err.is_some() {
                        tracing::info!("state persistence recovered");
                    }
                    *err = None;
                }
                Err(e) => {
                    tracing::error!("could not persist state: {e:#}");
                    *err = Some(format!("{e:#}"));
                }
            }
        }
        *self.current.write().await = Some(Arc::new(snap));
    }

    /// Warm start: adopt whatever the previous run persisted.
    pub async fn restore(&self) -> bool {
        let Some(snap) = self.store.load_optional() else {
            return false;
        };
        // Keep cycle numbering monotonic across restarts so a consumer polling
        // the cycle number never sees it go backwards.
        self.cycle.store(snap.cycle_id, Ordering::Relaxed);
        tracing::info!(
            "restored state from disk: cycle {}, {}/{} verified, git={}, {}",
            snap.cycle_id,
            snap.verified,
            snap.expected,
            snap.git_ok,
            self.store.describe()
        );
        *self.current.write().await = Some(Arc::new(snap));
        true
    }
}
