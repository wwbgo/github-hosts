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

/// Anything that can supply a configuration value by name. `Config::from_env`
/// uses the process environment; tests supply a plain map instead, so they do
/// not mutate global state that other tests read concurrently.
pub trait Lookup {
    fn get(&self, key: &str) -> Option<String>;
}

impl Lookup for std::collections::HashMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        self.get(key).cloned()
    }
}

struct Env;

impl Lookup for Env {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

fn num<L: Lookup, T: std::str::FromStr>(l: &L, key: &str, default: T) -> T {
    l.get(key)
        .and_then(|v| v.trim().parse::<T>().ok())
        .unwrap_or(default)
}

/// Parse a timeout in seconds, clamped to something `Duration` accepts.
///
/// `Duration::from_secs_f64` *panics* on a negative, NaN or infinite value, and
/// `parse::<f64>()` happily accepts `-1`, `NaN` and `inf`. So a single mistyped
/// env var used to abort the process at startup with a backtrace instead of
/// reporting a bad configuration. Anything unusable falls back to the default.
fn num_secs_f64<L: Lookup>(l: &L, key: &str, default: f64) -> f64 {
    const MIN: f64 = 0.1;
    const MAX: f64 = 600.0;
    match l.get(key).and_then(|v| v.trim().parse::<f64>().ok()) {
        Some(v) if v.is_finite() && v > 0.0 => v.clamp(MIN, MAX),
        Some(_) | None => default,
    }
}

impl Config {
    pub fn from_env() -> Self {
        Self::from_lookup(&Env)
    }

    /// Build from any value source. Kept separate from `from_env` so tests can
    /// exercise clamping without touching the process environment: `set_var`
    /// is global, and two tests doing it in parallel race (this flaked 3 runs
    /// in 10 before the split).
    pub fn from_lookup<L: Lookup>(l: &L) -> Self {
        let d = Self::default();

        // Clamped rather than rejected: a small value should not stop the
        // service, but it also should not turn it into a request flood.
        let requested = num(l, "INTERVAL_SECS", d.interval.as_secs());
        let interval_secs = requested.max(MIN_INTERVAL_SECS);

        Self {
            port: num(l, "PORT", d.port),
            interval: Duration::from_secs(interval_secs),
            timeout: Duration::from_secs_f64(num_secs_f64(
                l,
                "PROBE_TIMEOUT_SECS",
                d.timeout.as_secs_f64(),
            )),
            concurrency: num(l, "CONCURRENCY", d.concurrency).max(1),
            max_scan: num(l, "MAX_SCAN", d.max_scan).max(1),
            top_k: num(l, "TOP_K", d.top_k).max(1),
            repeats: num(l, "REPEATS", d.repeats).max(1),
            mode: l.get("HOSTS_MODE").unwrap_or(d.mode),
            state_dir: l.get("STATE_DIR").map(PathBuf::from).unwrap_or(d.state_dir),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg_with(pairs: &[(&str, &str)]) -> Config {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(&map)
    }

    #[test]
    fn default_interval_is_above_the_floor() {
        let c = Config::default();
        assert!(c.interval.as_secs() >= MIN_INTERVAL_SECS);
        assert_eq!(c.interval.as_secs(), DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn interval_below_the_floor_is_clamped_up() {
        // A caller passing a tiny value gets the floor, not a flood.
        assert_eq!(
            cfg_with(&[("INTERVAL_SECS", "5")]).interval.as_secs(),
            MIN_INTERVAL_SECS
        );
        assert_eq!(
            cfg_with(&[("INTERVAL_SECS", "0")]).interval.as_secs(),
            MIN_INTERVAL_SECS
        );
    }

    #[test]
    fn interval_above_the_floor_is_honoured() {
        assert_eq!(
            cfg_with(&[("INTERVAL_SECS", "3600")]).interval.as_secs(),
            3600
        );
    }

    #[test]
    fn unparsable_values_fall_back_to_defaults() {
        let c = cfg_with(&[("INTERVAL_SECS", "soon"), ("CONCURRENCY", "")]);
        assert_eq!(c.interval.as_secs(), DEFAULT_INTERVAL_SECS);
        assert_eq!(c.concurrency, Config::default().concurrency);
    }

    #[test]
    fn zero_is_raised_to_one_where_zero_would_stall() {
        let c = cfg_with(&[("CONCURRENCY", "0"), ("TOP_K", "0"), ("REPEATS", "0")]);
        assert_eq!(c.concurrency, 1);
        assert_eq!(c.top_k, 1);
        assert_eq!(c.repeats, 1);
    }

    #[test]
    fn non_positive_or_non_finite_timeout_does_not_panic() {
        // Duration::from_secs_f64 panics on these; a mistyped env var used to
        // abort the process at startup (verified exit code 101).
        let default = Config::default().timeout.as_secs_f64();
        for bad in ["-1", "NaN", "inf", "-inf", "0", ""] {
            let c = cfg_with(&[("PROBE_TIMEOUT_SECS", bad)]);
            assert!(
                c.timeout.as_secs_f64() > 0.0,
                "PROBE_TIMEOUT_SECS={bad:?} produced a non-positive timeout"
            );
            assert_eq!(
                c.timeout.as_secs_f64(),
                default,
                "expected fallback for {bad:?}"
            );
        }
    }

    #[test]
    fn a_sane_timeout_is_kept() {
        assert!(
            (cfg_with(&[("PROBE_TIMEOUT_SECS", "9.5")])
                .timeout
                .as_secs_f64()
                - 9.5)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn absurd_timeouts_are_clamped_not_honoured() {
        // A 7-hour per-socket timeout would make one blocked cycle run forever.
        assert!(
            cfg_with(&[("PROBE_TIMEOUT_SECS", "99999")])
                .timeout
                .as_secs()
                <= 600
        );
    }
}
