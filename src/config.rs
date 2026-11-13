//! Runtime configuration, entirely environment driven so the same image can
//! be re-tuned without a rebuild.

use std::path::PathBuf;
use std::time::Duration;

/// Floor for the probe interval. Each cycle issues a few hundred requests
/// against GitHub's edge network; anything much below ten minutes risks
/// looking like abuse and getting the source address throttled, which would
/// then corrupt the very measurements the tool exists to produce.
pub const MIN_INTERVAL_SECS: u64 = 600;

/// Default cadence. Fifteen minutes keeps the cached block fresh enough (edge
/// blocking rotates on a minutes timescale) while staying far from anything
/// that could read as a request flood.
pub const DEFAULT_INTERVAL_SECS: u64 = 900;

/// Compile-time guard. Lowering the floor is a deliberate policy change, so it
/// should break the build rather than pass silently in review.
const _: () = assert!(MIN_INTERVAL_SECS >= 600);
const _: () = assert!(DEFAULT_INTERVAL_SECS >= MIN_INTERVAL_SECS);

#[derive(Clone, Debug)]
pub struct Config {
    /// HTTP listen port for the API.
    pub port: u16,
    /// Delay between two probe cycles.
    pub interval: Duration,
    /// Per-socket timeout. Kept short so a blackholed IP costs little.
    pub timeout: Duration,
    /// Number of probe requests in flight at once.
    pub concurrency: usize,
    /// Upper bound on candidate IPs screened per family.
    pub max_scan: usize,
    /// Shortlist size handed to the content validator.
    pub top_k: usize,
    /// Attempts per check before an IP is declared bad.
    pub repeats: usize,
    /// `pin` emits every verified host, `auto` only those where DNS is unusable.
    pub mode: String,
    /// Directory holding the persisted `hosts.txt` and `snapshot.json`.
    pub state_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 8080,
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            timeout: Duration::from_secs(6),
            concurrency: 12,
            max_scan: 40,
            top_k: 12,
            repeats: 2,
            mode: "pin".to_string(),
            state_dir: PathBuf::from("data"),
        }
    }
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<T>().ok())
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        let d = Self::default();

        // Clamped rather than rejected: a small value should not stop the
        // service, but it also should not turn it into a request flood.
        let requested = env_num("INTERVAL_SECS", d.interval.as_secs());
        let interval_secs = requested.max(MIN_INTERVAL_SECS);

        Self {
            port: env_num("PORT", d.port),
            interval: Duration::from_secs(interval_secs),
            timeout: Duration::from_secs_f64(env_num(
                "PROBE_TIMEOUT_SECS",
                d.timeout.as_secs_f64(),
            )),
            concurrency: env_num("CONCURRENCY", d.concurrency).max(1),
            max_scan: env_num("MAX_SCAN", d.max_scan).max(1),
            top_k: env_num("TOP_K", d.top_k).max(1),
            repeats: env_num("REPEATS", d.repeats).max(1),
            mode: std::env::var("HOSTS_MODE").unwrap_or(d.mode),
            state_dir: std::env::var("STATE_DIR")
                .map(PathBuf::from)
                .unwrap_or(d.state_dir),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_interval_is_above_the_floor() {
        let c = Config::default();
        assert!(c.interval.as_secs() >= MIN_INTERVAL_SECS);
        assert_eq!(c.interval.as_secs(), DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn interval_from_env_is_clamped_up_to_the_floor() {
        // A caller passing a tiny value gets the floor, not a flood.
        std::env::set_var("INTERVAL_SECS", "5");
        let c = Config::from_env();
        assert_eq!(c.interval.as_secs(), MIN_INTERVAL_SECS);
        std::env::remove_var("INTERVAL_SECS");
    }

    #[test]
    fn interval_from_env_above_the_floor_is_honoured() {
        std::env::set_var("INTERVAL_SECS", "3600");
        let c = Config::from_env();
        assert_eq!(c.interval.as_secs(), 3600);
        std::env::remove_var("INTERVAL_SECS");
    }
}
