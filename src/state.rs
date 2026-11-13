//! Shared state: what has been published, the manual refresh trigger, and the
//! bookkeeping behind the status page.
//!
//! ## One authoritative version
//!
//! `Published` carries both the structured result and the rendered block, plus a
//! `persisted` flag. That flag is what makes a failed write safe: the in-memory
//! record always describes the newest *good* cycle, while `store` may still hold
//! an older committed one. The HTTP layer compares the two (by cycle and by the
//! block hash recorded in `commit.json`) and serves memory whenever they
//! disagree, so a read-only volume cannot cause a stale block to be served.

use crate::config::Config;
use crate::probe::Snapshot;
use crate::store::Store;
use chrono::{DateTime, Local};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Notify, RwLock};

/// One successful cycle, as the service presents it.
pub struct Published {
    pub snapshot: Snapshot,
    /// Block rendered in the *default* mode, i.e. what `/hosts` serves.
    pub block: String,
    /// False when the disk write failed; the block is then memory-only.
    pub persisted: bool,
}

pub struct AppState {
    pub cfg: Config,
    pub store: Store,
    /// Newest good cycle. Replaced only when the new one ranks at least as high.
    pub current: RwLock<Option<Arc<Published>>>,
    /// Wakes the scheduler early (POST /v1/refresh).
    pub refresh: Notify,
    pub cycle: AtomicU64,
    /// When the next probe is due, for the status-page countdown.
    pub next_probe_at: RwLock<Option<DateTime<Local>>>,
    /// When the current/last cycle started. `POST /v1/refresh` consults this so
    /// a caller cannot drive back-to-back cycles and defeat the interval floor.
    pub last_cycle_started_at: RwLock<Option<DateTime<Local>>>,
    /// Set while a cycle is running.
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

    pub async fn published(&self) -> Option<Arc<Published>> {
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

    /// Install a new cycle and mirror it to disk.
    ///
    /// The in-memory record is installed regardless of the write result, because
    /// it is the newer truth; `persisted` records whether the disk agrees.
    pub async fn publish(&self, snap: Snapshot, block: String) {
        let saved = self.store.save(&block, &snap, &self.cfg.mode);
        let persisted = saved.is_ok();
        {
            let mut err = self.last_save_error.write().await;
            match &saved {
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
        *self.current.write().await = Some(Arc::new(Published {
            snapshot: snap,
            block,
            persisted,
        }));
    }

    /// Warm start from the last committed cycle.
    pub async fn restore(&self) -> bool {
        let Some((snapshot, block, mode)) = self.store.load_optional() else {
            return false;
        };
        // Keep cycle numbering monotonic across restarts so a poller watching
        // the cycle number never sees it go backwards.
        self.cycle.store(snapshot.cycle_id, Ordering::Relaxed);
        tracing::info!(
            "restored state from disk: cycle {}, {}/{} verified, git={}, mode={mode}, {}",
            snapshot.cycle_id,
            snapshot.verified,
            snapshot.expected,
            snapshot.git_ok,
            self.store.describe()
        );
        *self.current.write().await = Some(Arc::new(Published {
            snapshot,
            block,
            persisted: true,
        }));
        true
    }
}
