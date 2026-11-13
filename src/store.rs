//! Durable storage for the last verified result.
//!
//! Three files, with one of them acting as the commit point:
//!
//! * `snapshot.json` — the structured result
//! * `hosts.txt`     — the rendered block, byte-for-byte what a client consumes
//! * `commit.json`   — tiny record naming the cycle that the two above belong to
//!
//! ## Why a commit file
//!
//! Two files cannot be replaced atomically together. Writing them one after the
//! other leaves a window where `hosts.txt` is the new cycle and `snapshot.json`
//! is still the old one (or the reverse), and a reader hitting `/hosts` and
//! `/v1/snapshot` across that window gets two different answers. Worse, if the
//! second write fails the disagreement is permanent until the next successful
//! cycle.
//!
//! So the pair is written first and `commit.json` last. `commit.json` records the
//! cycle number, the render mode and a hash of `hosts.txt`; readers treat it as
//! the single source of truth and ignore any data file that does not match it.
//! One small atomic rename is therefore the commit, and readers cannot observe a
//! half-published pair.
//!
//! ## Why a write failure is safe
//!
//! Because `commit.json` is the commit point, a failed write leaves the previous
//! commit in place -- while the caller still holds the newer result in memory.
//! The HTTP layer falls back to memory exactly when the on-disk triple does not
//! match the in-memory version, so a read-only volume costs durability without
//! ever serving a stale block.

use crate::probe::Snapshot;
use crate::render::content_hash;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const HOSTS_FILE: &str = "hosts.txt";
pub const SNAPSHOT_FILE: &str = "snapshot.json";
pub const COMMIT_FILE: &str = "commit.json";

/// The commit record. Deliberately tiny: it is read on request paths.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Commit {
    pub cycle: u64,
    pub mode: String,
    /// `content_hash()` of the block, i.e. the ETag without ambiguity.
    pub hosts_sha: String,
    pub generated_at: String,
}

/// A fully consistent on-disk triple.
pub struct Committed {
    pub commit: Commit,
    pub hosts: String,
    pub snapshot: Snapshot,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new<P: AsRef<Path>>(dir: P) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    pub fn hosts_path(&self) -> PathBuf {
        self.dir.join(HOSTS_FILE)
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.dir.join(SNAPSHOT_FILE)
    }

    pub fn commit_path(&self) -> PathBuf {
        self.dir.join(COMMIT_FILE)
    }

    pub fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating state dir {}", self.dir.display()))
    }

    /// Make a rename in this directory durable.
    ///
    /// `File::sync_all` flushes the file's own data, but on Unix the directory
    /// entry created by the rename lives in the parent directory, which needs its
    /// own fsync. Without it a power loss can undo a rename the process already
    /// reported as successful. Windows has no equivalent operation.
    #[cfg(unix)]
    fn sync_dir(&self) -> Result<()> {
        fs::File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("syncing dir {}", self.dir.display()))
    }

    #[cfg(not(unix))]
    fn sync_dir(&self) -> Result<()> {
        Ok(())
    }

    /// Replace `name` with `bytes` without a window where the file is partial.
    fn atomic_write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.ensure_dir()?;
        let target = self.dir.join(name);
        // Per-process temp name: a fixed `.name.tmp` lets two instances sharing
        // a state dir overwrite each other's partial write and then rename it
        // into place as if it were their own.
        let tmp = self.dir.join(format!(".{name}.{}.tmp", std::process::id()));

        {
            let mut f =
                fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("writing {}", tmp.display()))?;
            // Flush to the device before the rename: without this a crash can
            // leave the renamed file present but empty.
            f.sync_all()
                .with_context(|| format!("syncing {}", tmp.display()))?;
        }

        fs::rename(&tmp, &target)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), target.display()))?;
        Ok(())
    }

    /// Persist one successful cycle.
    ///
    /// Order is load-bearing: data files first, `commit.json` last. Readers only
    /// trust the data once the commit names it.
    pub fn save(&self, block: &str, snap: &Snapshot, mode: &str) -> Result<()> {
        let json = serde_json::to_vec_pretty(snap).context("serialising snapshot")?;
        self.atomic_write(SNAPSHOT_FILE, &json)?;
        self.atomic_write(HOSTS_FILE, block.as_bytes())?;

        let commit = Commit {
            cycle: snap.cycle_id,
            mode: mode.to_string(),
            hosts_sha: content_hash(block),
            generated_at: snap.generated_at.to_rfc3339(),
        };
        let body = serde_json::to_vec(&commit).context("serialising commit")?;
        self.atomic_write(COMMIT_FILE, &body)?;
        self.sync_dir()?;
        Ok(())
    }

    pub fn read_commit(&self) -> Result<Commit> {
        let raw = fs::read(self.commit_path())
            .with_context(|| format!("reading {}", self.commit_path().display()))?;
        serde_json::from_slice(&raw).context("parsing commit.json")
    }

    pub fn read_hosts_raw(&self) -> Result<String> {
        fs::read_to_string(self.hosts_path())
            .with_context(|| format!("reading {}", self.hosts_path().display()))
    }

    pub fn read_snapshot_raw(&self) -> Result<Snapshot> {
        let raw = fs::read(self.snapshot_path())
            .with_context(|| format!("reading {}", self.snapshot_path().display()))?;
        serde_json::from_slice(&raw).context("parsing snapshot.json")
    }

    /// Read the on-disk triple, rejecting anything the commit does not vouch for.
    ///
    /// `Ok(None)` means "the disk does not hold a consistent publish" -- nothing
    /// published yet, a write was interrupted, or another instance wrote
    /// something this commit does not describe. Callers fall back to their
    /// in-memory copy rather than serve a mismatch.
    pub fn read_committed(&self) -> Result<Option<Committed>> {
        let commit = match self.read_commit() {
            Ok(c) => c,
            Err(e) => {
                if self.commit_path().exists() {
                    tracing::warn!("ignoring unreadable commit.json: {e:#}");
                }
                return Ok(None);
            }
        };

        let hosts = self.read_hosts_raw()?;
        if content_hash(&hosts) != commit.hosts_sha {
            tracing::warn!(
                "hosts.txt does not match commit.json (cycle {}); treating disk state as stale",
                commit.cycle
            );
            return Ok(None);
        }

        let snapshot = self.read_snapshot_raw()?;
        if snapshot.cycle_id != commit.cycle {
            tracing::warn!(
                "snapshot.json is cycle {} but commit says {}; treating disk state as stale",
                snapshot.cycle_id,
                commit.cycle
            );
            return Ok(None);
        }

        Ok(Some(Committed {
            commit,
            hosts,
            snapshot,
        }))
    }

    /// Warm-start state: `(snapshot, block, mode)`.
    pub fn load_optional(&self) -> Option<(Snapshot, String, String)> {
        match self.read_committed() {
            Ok(Some(c)) => Some((c.snapshot, c.hosts, c.commit.mode)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("ignoring unreadable state: {e:#}");
                None
            }
        }
    }

    /// Human-readable state of the persisted files, for the status page.
    pub fn describe(&self) -> String {
        let hosts = fs::metadata(self.hosts_path())
            .map(|m| format!("{} B", m.len()))
            .unwrap_or_else(|_| "missing".to_string());
        let cycle = self
            .read_commit()
            .map(|c| format!("#{}", c.cycle))
            .unwrap_or_else(|_| "uncommitted".to_string());
        format!(
            "{}/{} (cycle {}, hosts {hosts})",
            self.dir.display(),
            COMMIT_FILE,
            cycle
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::Endpoint;
    use chrono::Local;
    use std::collections::BTreeMap;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "gh-hosts-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        p
    }

    fn sample(cycle: u64) -> Snapshot {
        Snapshot {
            generated_at: Local::now(),
            cycle_id: cycle,
            verified: 1,
            expected: 2,
            required_ok: true,
            git_ok: true,
            git_detail: "ok".into(),
            elapsed_ms: 1234,
            endpoints: vec![Endpoint {
                host: "github.com".into(),
                ip: "140.82.121.3".into(),
                latency_ms: 210.0,
                verified: true,
                detail: "site html=html 5000B".into(),
            }],
            unverified: vec!["gist.github.com".into()],
            dns: BTreeMap::new(),
        }
    }

    fn block(ip: &str) -> String {
        format!("# >>> github-hosts :: t :: mode=pin :: v1.1.0 >>>\n{ip}  github.com\n")
    }

    #[test]
    fn save_then_read_round_trips_the_whole_triple() {
        let dir = temp_dir("roundtrip");
        let store = Store::new(&dir);
        let b = block("140.82.121.3");

        store.save(&b, &sample(7), "pin").expect("save");

        let c = store.read_committed().unwrap().expect("committed");
        assert_eq!(c.commit.cycle, 7);
        assert_eq!(c.commit.mode, "pin");
        // The block must come back byte-for-byte: clients paste it verbatim.
        assert_eq!(c.hosts, b);
        assert_eq!(c.snapshot.cycle_id, 7);
        assert_eq!(c.snapshot.endpoints[0].ip, "140.82.121.3");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_temp_file_survives_a_save() {
        let dir = temp_dir("notmp");
        let store = Store::new(&dir);
        store.save(&block("1.1.1.1"), &sample(1), "pin").unwrap();
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_overwrites_in_place() {
        let dir = temp_dir("overwrite");
        let store = Store::new(&dir);
        store.save(&block("1.1.1.1"), &sample(1), "pin").unwrap();
        store.save(&block("2.2.2.2"), &sample(2), "pin").unwrap();
        let c = store.read_committed().unwrap().unwrap();
        assert_eq!(c.commit.cycle, 2);
        assert!(c.hosts.contains("2.2.2.2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_state_is_not_an_error_on_boot() {
        let dir = temp_dir("missing");
        let store = Store::new(&dir);
        assert!(store.load_optional().is_none());
    }

    #[test]
    fn corrupt_state_is_ignored_rather_than_fatal() {
        let dir = temp_dir("corrupt");
        let store = Store::new(&dir);
        store.ensure_dir().unwrap();
        fs::write(store.snapshot_path(), b"{not json").unwrap();
        assert!(store.load_optional().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_half_written_pair_is_not_served() {
        // The exact failure this design exists for: hosts.txt replaced, then the
        // process dies before snapshot.json. commit.json still names the old
        // cycle, so the pair must be rejected as inconsistent.
        let dir = temp_dir("halfwrite");
        let store = Store::new(&dir);
        store.save(&block("1.1.1.1"), &sample(1), "pin").unwrap();

        fs::write(store.hosts_path(), block("9.9.9.9")).unwrap(); // new block only

        assert!(
            store.read_committed().unwrap().is_none(),
            "an uncommitted hosts.txt must not be served as truth"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_snapshot_from_another_cycle_is_not_served() {
        let dir = temp_dir("mismatch");
        let store = Store::new(&dir);
        store.save(&block("1.1.1.1"), &sample(1), "pin").unwrap();

        // Replace the JSON with a different cycle, leaving commit.json alone.
        let other = serde_json::to_vec(&sample(99)).unwrap();
        fs::write(store.snapshot_path(), other).unwrap();

        assert!(store.read_committed().unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_commit_records_the_render_mode() {
        // /hosts uses this to decide whether the file answers the request.
        let dir = temp_dir("mode");
        let store = Store::new(&dir);
        store.save(&block("1.1.1.1"), &sample(3), "auto").unwrap();
        assert_eq!(store.read_commit().unwrap().mode, "auto");
        std::fs::remove_dir_all(&dir).ok();
    }
}
