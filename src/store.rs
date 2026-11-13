//! Durable storage for the last verified result.
//!
//! The probe writes two files, the API reads them back:
//!
//! * `hosts.txt`     — the rendered block, exactly what a client consumes
//! * `snapshot.json` — the structured result, so any mode/filter can be
//!   rendered on demand and the status page survives a restart
//!
//! Two properties matter here:
//!
//! 1. **Atomicity.** Every write goes to a temporary file in the same
//!    directory and is then renamed over the target. A reader therefore sees
//!    either the previous complete file or the new complete file, never a
//!    half-written one -- including when the process is killed mid-write.
//! 2. **Restart continuity.** On boot the JSON is loaded back, so `/hosts`
//!    answers immediately instead of returning 503 until the first cycle ends.

use crate::probe::Snapshot;
use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const HOSTS_FILE: &str = "hosts.txt";
pub const SNAPSHOT_FILE: &str = "snapshot.json";

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

    pub fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating state dir {}", self.dir.display()))
    }

    /// Replace `name` with `bytes` without a window where the file is partial.
    fn atomic_write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.ensure_dir()?;
        let target = self.dir.join(name);
        let tmp = self.dir.join(format!(".{name}.tmp"));

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

    /// Persist one successful cycle: the rendered block plus its JSON source.
    pub fn save(&self, block: &str, snap: &Snapshot) -> Result<()> {
        let json = serde_json::to_vec_pretty(snap).context("serialising snapshot")?;
        // JSON first: if the block was written and the JSON failed, a restart
        // would serve a block whose detail could not be reloaded.
        self.atomic_write(SNAPSHOT_FILE, &json)?;
        self.atomic_write(HOSTS_FILE, block.as_bytes())?;
        Ok(())
    }

    pub fn read_hosts(&self) -> Result<String> {
        fs::read_to_string(self.hosts_path())
            .with_context(|| format!("reading {}", self.hosts_path().display()))
    }

    pub fn read_snapshot(&self) -> Result<Snapshot> {
        let raw = fs::read(self.snapshot_path())
            .with_context(|| format!("reading {}", self.snapshot_path().display()))?;
        serde_json::from_slice(&raw).context("parsing snapshot.json")
    }

    /// Snapshot for boot-time warm start; `None` when there is nothing usable.
    pub fn load_optional(&self) -> Option<Snapshot> {
        match self.read_snapshot() {
            Ok(s) => Some(s),
            Err(e) => {
                if self.snapshot_path().exists() {
                    tracing::warn!("ignoring unreadable state: {e:#}");
                }
                None
            }
        }
    }

    /// Human-readable state of the persisted files, for the status page.
    pub fn describe(&self) -> String {
        let hosts = fs::metadata(self.hosts_path())
            .map(|m| format!("{} B", m.len()))
            .unwrap_or_else(|_| "missing".to_string());
        format!("{}/{} (hosts {})", self.dir.display(), SNAPSHOT_FILE, hosts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Endpoint, Snapshot};
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

    fn sample() -> Snapshot {
        Snapshot {
            generated_at: Local::now(),
            cycle_id: 7,
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

    #[test]
    fn save_then_read_round_trips_both_files() {
        let dir = temp_dir("roundtrip");
        let store = Store::new(&dir);
        let snap = sample();
        let block = "# >>> github-hosts\n140.82.121.3  github.com\n";

        store.save(block, &snap).expect("save");

        // The block must come back byte-for-byte: clients paste it verbatim.
        assert_eq!(store.read_hosts().unwrap(), block);
        let back = store.read_snapshot().unwrap();
        assert_eq!(back.cycle_id, 7);
        assert_eq!(back.endpoints[0].ip, "140.82.121.3");
        assert!(back.git_ok);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_temp_file_survives_a_save() {
        // A leftover .tmp would be harmless, but its presence means the rename
        // did not happen and a reader could be looking at a stale file.
        let dir = temp_dir("notmp");
        let store = Store::new(&dir);
        store.save("block\n", &sample()).expect("save");
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
        store.save("first\n", &sample()).unwrap();
        store.save("second\n", &sample()).unwrap();
        assert_eq!(store.read_hosts().unwrap(), "second\n");
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
        // A truncated JSON must not stop the service: the probe will simply
        // run again and overwrite it.
        let dir = temp_dir("corrupt");
        let store = Store::new(&dir);
        store.ensure_dir().unwrap();
        fs::write(store.snapshot_path(), b"{not json").unwrap();
        assert!(store.load_optional().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
