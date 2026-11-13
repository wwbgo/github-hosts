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
use std::collections::{BTreeMap, BTreeSet};
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
    /// Newest published cycle, always the last one that completed.
    pub current: RwLock<Option<Arc<Published>>>,
    /// Consecutive cycles each host has failed to verify. Nothing stale is kept
    /// for a host that failed, so this counter is the only place where "it has
    /// been failing for an hour" stays visible.
    pub failures: RwLock<BTreeMap<String, u32>>,
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
            failures: RwLock::new(BTreeMap::new()),
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

    /// Record which hosts this cycle failed to verify, so a persistent failure
    /// can be told apart from a single bad window.
    pub async fn note_cycle(&self, snap: &Snapshot) {
        // A host whose family is turned off was never probed. Counting it would
        // report a configuration choice as an outage, so it is skipped and any
        // streak it had is dropped.
        let failing: BTreeSet<&str> = snap
            .unverified
            .iter()
            .map(|h| h.as_str())
            .filter(|h| !snap.suppressed.iter().any(|s| s == h))
            .collect();
        let mut f = self.failures.write().await;
        f.retain(|host, _| failing.contains(host.as_str()));
        for host in failing {
            *f.entry(host.to_string()).or_insert(0) += 1;
        }
    }

    pub async fn failure_streaks(&self) -> BTreeMap<String, u32> {
        self.failures.read().await.clone()
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
    /// Every completed cycle is installed: the newest probe result is the truth,
    /// and keeping an older snapshot would leave pins on disk for hosts this
    /// cycle just failed to reach.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::test_snapshot;

    #[tokio::test]
    async fn failure_streaks_accumulate_and_reset() {
        let st = AppState::new(Config::default());
        for _ in 0..3 {
            st.note_cycle(&test_snapshot(&["github.com"], &["gist.github.com"], true))
                .await;
        }
        assert_eq!(st.failure_streaks().await.get("gist.github.com"), Some(&3));

        st.note_cycle(&test_snapshot(
            &["github.com", "gist.github.com"],
            &[],
            true,
        ))
        .await;
        assert!(
            st.failure_streaks().await.is_empty(),
            "a host that verified again must lose its streak"
        );
    }

    #[tokio::test]
    async fn a_family_turned_off_is_not_a_failure_streak() {
        // FAMILIES=api is a valid configuration; the hosts it excludes were
        // never probed, so they must not appear as "failing for N cycles".
        let st = AppState::new(Config::default());
        let mut snap = test_snapshot(&["api.github.com"], &["github.com"], true);
        snap.suppressed = vec!["github.com".to_string()];
        st.note_cycle(&snap).await;
        assert!(
            st.failure_streaks().await.is_empty(),
            "a suppressed host is not a failure"
        );
    }
}
