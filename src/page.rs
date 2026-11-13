//! Server-rendered status page.
//!
//! Deliberately dependency-free: no JS framework, no build step, no external
//! fonts or icons. A status page that needs its own asset bundle would be an
//! odd thing to ship alongside a tool whose whole purpose is working around
//! broken asset delivery.

use crate::probe::Snapshot;
use crate::render::VERSION;
use chrono::{DateTime, Local};
use std::collections::BTreeMap;

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    Starting,
    Busy,
    Ok,
    Degraded,
    Down,
    /// Verified, git fine, but nothing has been published for several intervals.
    Stale,
}

/// Intervals without a new publish before the block is called stale.
const STALE_INTERVALS: i64 = 3;

/// A snapshot that has not been refreshed for several intervals is no longer a
/// live answer, whatever its numbers say. Shared by the status page and
/// `/healthz` so the two cannot disagree about freshness.
pub fn is_stale(snap: &Snapshot, interval_secs: u64, now: DateTime<Local>) -> bool {
    interval_secs > 0
        && (now - snap.generated_at).num_seconds() > STALE_INTERVALS * interval_secs as i64
}

impl Health {
    fn of(snap: Option<&Snapshot>, probing: bool, interval_secs: u64) -> Self {
        match snap {
            None if probing => Health::Busy,
            None => Health::Starting,
            Some(s) => {
                // A cycle that never publishes (blocked network, disabled
                // families) leaves the numbers looking healthy while the block
                // on disk keeps answering with stale addresses.
                if is_stale(s, interval_secs, Local::now()) {
                    Health::Stale
                } else if s.required_ok && s.git_ok {
                    Health::Ok
                } else if s.verified > 0 {
                    Health::Degraded
                } else {
                    Health::Down
                }
            }
        }
    }

    fn class(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Busy => "busy",
            Health::Degraded => "warn",
            Health::Starting => "warn",
            Health::Down => "bad",
            Health::Stale => "warn",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Health::Ok => "正常",
            Health::Busy => "检测中",
            Health::Degraded => "部分可用",
            Health::Starting => "启动中",
            Health::Down => "不可用",
            Health::Stale => "数据陈旧",
        }
    }
}

/// `host (连续 N 轮)` once a failure has outlived a single cycle.
fn with_streak(host: &str, failures: &BTreeMap<String, u32>) -> String {
    match failures.get(host) {
        Some(n) if *n > 1 => format!("{host} (连续 {n} 轮)"),
        _ => host.to_string(),
    }
}

fn human_ago(t: DateTime<Local>) -> String {
    let secs = (Local::now() - t).num_seconds().max(0);
    if secs < 60 {
        format!("{secs} 秒前")
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else {
        format!("{} 小时前", secs / 3600)
    }
}

/// Everything the status views need. Eight live arguments was past the point
/// where a positional call site is readable, and it keeps the two renderers in
/// step.
#[derive(Clone, Copy)]
pub struct Status<'a> {
    pub snap: Option<&'a Snapshot>,
    pub probing: bool,
    pub failures: &'a BTreeMap<String, u32>,
    pub next: Option<DateTime<Local>>,
    pub interval_secs: u64,
    pub mode: &'a str,
    pub state_desc: &'a str,
    pub save_error: Option<&'a str>,
}

/// Plain-text status, for `curl /` and anything that does not speak HTML.
pub fn status_text(st: Status<'_>) -> String {
    let Status {
        snap,
        probing,
        failures,
        next,
        interval_secs,
        mode,
        state_desc,
        save_error,
    } = st;
    let health = Health::of(snap, probing, interval_secs);
    let mut s = String::new();
    s.push_str(&format!("github-hosts {VERSION}\n\n"));
    s.push_str(&format!("status   {}\n", health.label()));
    match snap {
        Some(p) => {
            s.push_str(&format!("cycle    {}\n", p.cycle_id));
            s.push_str(&format!(
                "endpoints {}/{}  git {}\n",
                p.verified,
                p.expected,
                if p.git_ok { "ok" } else { "FAIL" }
            ));
            s.push_str(&format!(
                "probe     {}ms, {} ({})\n",
                p.elapsed_ms,
                p.generated_at.format("%Y-%m-%d %H:%M:%S"),
                human_ago(p.generated_at)
            ));
            if !p.unverified.is_empty() {
                let list: Vec<String> = p
                    .unverified
                    .iter()
                    .map(|h| with_streak(h, failures))
                    .collect();
                s.push_str(&format!("unverified {}\n", list.join(", ")));
            }
            for e in &p.endpoints {
                s.push_str(&format!(
                    "  {:<16} {:<42} {:>6.0}ms\n",
                    e.ip, e.host, e.latency_ms
                ));
            }
        }
        None => s.push_str("cycle    (not completed yet)\n"),
    }
    match (probing, next) {
        (true, _) => s.push_str("next     正在检测\n"),
        (false, Some(t)) => s.push_str(&format!(
            "next     {} ({} 秒后)\n",
            t.format("%H:%M:%S"),
            (t - Local::now()).num_seconds().max(0)
        )),
        _ => {}
    }
    s.push_str(&format!("mode     {mode}  interval {interval_secs}s\n"));
    s.push_str(&format!("state    {state_desc}\n"));
    if let Some(e) = save_error {
        s.push_str(&format!("persist  FAILED: {e}\n"));
    }
    s.push('\n');
    s.push_str("GET  /hosts            hosts-file block\n");
    s.push_str("GET  /v1/snapshot      full probe detail (JSON)\n");
    s.push_str("POST /v1/refresh       force a probe cycle\n");
    s.push_str("GET  /healthz          liveness (JSON)\n");
    s
}

pub fn status_html(st: Status<'_>) -> String {
    let Status {
        snap,
        probing,
        failures,
        next,
        interval_secs,
        mode,
        state_desc,
        save_error,
    } = st;
    let health = Health::of(snap, probing, interval_secs);
    let mut h = String::with_capacity(8192);

    h.push_str("<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    // No JS: a meta refresh is enough for a status page and survives every
    // browser, including ones with scripting disabled.
    h.push_str("<meta http-equiv=\"refresh\" content=\"30\">");
    h.push_str("<title>github-hosts 检测状态</title><style>");
    h.push_str(
        r#":root{--bg:#fff;--fg:#1f2328;--dim:#57606a;--line:#d8dee4;--card:#f6f8fa;
--ok:#1a7f37;--okbg:#dafbe1;--warn:#9a6700;--warnbg:#fff8c5;--bad:#cf222e;--badbg:#ffebe9;
--busy:#0969da;--busybg:#ddf4ff;--mono:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
@media (prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--dim:#8b949e;--line:#30363d;
--card:#161b22;--ok:#3fb950;--okbg:#0f2d18;--warn:#d29922;--warnbg:#3b2e00;--bad:#f85149;
--badbg:#3d1113;--busy:#58a6ff;--busybg:#0d2d5e}}
*{box-sizing:border-box}
body{margin:0;padding:32px 20px 56px;background:var(--bg);color:var(--fg);
font:15px/1.6 system-ui,-apple-system,"Segoe UI","Noto Sans SC",sans-serif}
main{max-width:760px;margin:0 auto}
header{display:flex;align-items:center;gap:12px;flex-wrap:wrap;margin-bottom:4px}
h1{margin:0;font-size:19px;font-weight:600;letter-spacing:-.01em}
.badge{display:inline-flex;align-items:center;gap:6px;padding:3px 10px;border-radius:999px;
font-size:12.5px;font-weight:600;box-shadow:inset 0 0 0 1px rgba(128,128,128,.22)}
.badge::before{content:"";width:7px;height:7px;border-radius:50%;background:currentColor}
/* Scoped to .badge on purpose: as bare class rules these also matched the
   `dd.warn` / `dd.bad` value cells and painted a coloured block behind them. */
.badge.ok{color:var(--ok);background:var(--okbg)}
.badge.warn{color:var(--warn);background:var(--warnbg)}
.badge.bad{color:var(--bad);background:var(--badbg)}
.badge.busy{color:var(--busy);background:var(--busybg)}
.sub{color:var(--dim);font-size:13px;margin:0 0 26px}
dl{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:1px;
background:var(--line);border:1px solid var(--line);border-radius:8px;overflow:hidden;margin:0 0 26px}
dl>div{background:var(--card);padding:12px 14px}
dt{color:var(--dim);font-size:11.5px;text-transform:uppercase;letter-spacing:.06em;margin-bottom:3px}
dd{margin:0;font-size:15px;font-variant-numeric:tabular-nums}
dd.warn{color:var(--warn)}
dd.bad{color:var(--bad)}
/* Rows are a grid, not a table: at narrow widths the hostname can then take a
   whole line of its own instead of being squeezed until ".com" breaks in half. */
.list{margin-top:2px}
.row{display:grid;grid-template-columns:minmax(0,1fr) 178px 64px;gap:10px;
align-items:baseline;padding:7px 0;border-bottom:1px solid var(--line)}
.row:last-child{border-bottom:0}
.row:hover{background:var(--card)}
/* No text-transform here: it would turn the unit "ms" into "MS", and SI unit
   symbols are case-sensitive. */
.row.head{padding:0 0 8px;color:var(--dim);font-size:11.5px;letter-spacing:.06em}
/* The status dot sits inside the host cell, so the header needs the same
   indent to line up with the text rather than with the dot. */
.row.head span:first-child{padding-left:15px}
.row.head .ms{text-align:right}
.host{font-family:var(--mono);font-size:12.5px;min-width:0;overflow-wrap:anywhere}
.ip{font-family:var(--mono);font-size:12.5px;color:var(--dim);min-width:0;overflow-wrap:anywhere}
.ms{font-family:var(--mono);font-size:12.5px;text-align:right;
font-variant-numeric:tabular-nums;color:var(--dim);white-space:nowrap}
/* Dim only the text of an unverified row, never the status dot: dimming the
   whole cell turned the red dot into pale pink and made the state hard to see. */
.row.miss .host{color:var(--dim);font-weight:400}
.dot{display:inline-block;width:7px;height:7px;border-radius:50%;margin-right:8px;
vertical-align:1px;background:var(--ok)}
.row.miss .dot{background:var(--bad)}
footer{margin-top:26px;padding-top:16px;border-top:1px solid var(--line);
color:var(--dim);font-size:12.5px;display:flex;gap:18px;flex-wrap:wrap}
a{color:inherit}
footer a{text-decoration:none;border-bottom:1px solid var(--line)}
footer a:hover{color:var(--fg);border-color:var(--fg)}
@media (max-width:560px){
body{padding:24px 14px 44px}
dl{grid-template-columns:repeat(2,1fr)}
/* Two lines per entry: hostname owns the first, ip and latency share the
   second. Keeps long names from wrapping mid-token. */
.row{grid-template-columns:minmax(0,1fr) auto;gap:1px 10px}
.row.head{display:none}
.row .host{grid-column:1/-1}
}
.warnline{margin:22px 0 0;padding:10px 12px;border-radius:6px;background:var(--warnbg);
color:var(--warn);font-size:12.5px;font-family:var(--mono);word-break:break-word}
.legend{color:var(--dim);font-size:12px;margin:14px 0 0}"#,
    );
    h.push_str("</style></head><body><main>");

    // header
    h.push_str("<header><h1>github-hosts</h1>");
    h.push_str(&format!(
        "<span class=\"badge {}\">{}</span></header>",
        health.class(),
        health.label()
    ));

    // subtitle: one line carrying freshness and cadence, so the fact grid can
    // stay a clean 4-cell row.
    let next_txt = if probing {
        "新一轮检测进行中".to_string()
    } else {
        match next {
            Some(t) => format!(
                "下次检测 {}（{} 秒后）",
                t.format("%H:%M:%S"),
                (t - Local::now()).num_seconds().max(0)
            ),
            None => String::new(),
        }
    };
    let sub = match (snap, probing) {
        (None, _) => "首轮检测进行中，完成后将自动发布。".to_string(),
        (Some(p), _) => {
            let mut s = format!("上次检测 {}", human_ago(p.generated_at));
            if !next_txt.is_empty() {
                s.push_str(" · ");
                s.push_str(&next_txt);
            }
            s
        }
    };
    h.push_str(&format!("<p class=\"sub\">{}</p>", esc(&sub)));

    // facts: exactly four cells, no ragged second row
    if let Some(p) = snap {
        h.push_str("<dl>");
        let cov_cls = if p.verified >= p.expected {
            ""
        } else {
            " class=\"warn\""
        };
        h.push_str(&format!(
            "<div><dt>已验证接口</dt><dd{}>{} / {}</dd></div>",
            cov_cls, p.verified, p.expected
        ));
        h.push_str(&format!(
            "<div><dt>Git 协议</dt><dd{}>{}</dd></div>",
            if p.git_ok { "" } else { " class=\"bad\"" },
            if p.git_ok { "正常" } else { "未通过" }
        ));
        h.push_str(&format!(
            "<div><dt>本轮耗时</dt><dd>{} 秒</dd></div>",
            p.elapsed_ms / 1000
        ));
        h.push_str(&format!(
            "<div><dt>检测次数</dt><dd>#{}</dd></div>",
            p.cycle_id
        ));
        h.push_str("</dl>");
    }

    // endpoint list
    if let Some(p) = snap {
        h.push_str("<div class=\"list\">");
        // Unit lives in the header so the rows stay narrow and scannable.
        h.push_str(
            "<div class=\"row head\"><span>接口</span><span>IP</span>\
             <span class=\"ms\">延迟 (ms)</span></div>",
        );
        for e in &p.endpoints {
            h.push_str(&format!(
                "<div class=\"row\"><span class=\"host\"><span class=\"dot\" \
                 title=\"已验证\"></span>{}</span>\
                 <span class=\"ip\">{}</span><span class=\"ms\">{:.0}</span></div>",
                esc(&e.host),
                esc(&e.ip),
                e.latency_ms
            ));
        }
        for host in &p.unverified {
            // The streak goes in the numeric column: it is a count, while the IP
            // column is reserved for an address this host does not have.
            let (text, title) = match failures.get(host) {
                Some(n) if *n > 1 => (format!("{n} 轮"), format!("已连续 {n} 轮未验证")),
                _ => ("—".to_string(), "未验证".to_string()),
            };
            h.push_str(&format!(
                "<div class=\"row miss\"><span class=\"host\"><span class=\"dot\" \
                 title=\"未验证\"></span>{}</span>\
                 <span class=\"ip\">—</span><span class=\"ms\" title=\"{}\">{}</span></div>",
                esc(host),
                esc(&title),
                esc(&text)
            ));
        }
        h.push_str("</div>");
        if !p.unverified.is_empty() {
            h.push_str(
                "<p class=\"legend\">未验证的域名不会写入 hosts，继续使用系统 DNS 解析。</p>",
            );
        }

        if !p.git_ok {
            h.push_str(&format!(
                "<p class=\"warnline\">git: {}</p>",
                esc(&p.git_detail)
            ));
        }
        // One badge slot cannot show "stale" and "degraded" at once, so the age
        // is repeated here rather than being hidden behind the badge label.
        if is_stale(p, interval_secs, Local::now()) {
            h.push_str(&format!(
                "<p class=\"warnline\">上次成功检测在{}（超过 {STALE_INTERVALS} 个检测周期）：块里的 IP 来自那一轮，不会自动刷新。</p>",
                human_ago(p.generated_at)
            ));
        }
    }

    // A failed write is the one condition that silently breaks the contract
    // "what the API returns is what is on disk", so it gets its own banner.
    if let Some(e) = save_error {
        h.push_str(&format!(
            "<p class=\"warnline\">落盘失败，接口仍从内存回退提供内容：{}</p>",
            esc(e)
        ));
    }

    h.push_str(&format!(
        "<footer><a href=\"/hosts\">/hosts</a><a href=\"/v1/snapshot\">/v1/snapshot</a>\
         <span style=\"margin-left:auto\">v{VERSION} · mode={} · 每 {} 秒检测 · 落盘 {} · 30 秒自动刷新</span></footer>",
        esc(mode),
        interval_secs,
        esc(state_desc)
    ));

    h.push_str("</main></body></html>");
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::test_snapshot;

    const INTERVAL: u64 = 900;

    fn view<'a>(s: &'a Snapshot, failures: &'a BTreeMap<String, u32>) -> Status<'a> {
        Status {
            snap: Some(s),
            probing: false,
            failures,
            next: None,
            interval_secs: INTERVAL,
            mode: "pin",
            state_desc: "data",
            save_error: None,
        }
    }

    fn at(offset_secs: i64) -> Snapshot {
        let mut s = test_snapshot(&["github.com"], &[], true);
        s.generated_at = Local::now() + chrono::Duration::seconds(offset_secs);
        s
    }

    #[test]
    fn a_fresh_snapshot_is_ok() {
        let s = test_snapshot(&["github.com"], &[], true);
        assert_eq!(Health::of(Some(&s), false, INTERVAL), Health::Ok);
    }

    #[test]
    fn a_snapshot_older_than_three_intervals_is_stale() {
        // Nothing published for 50 minutes while the interval is 15: whatever
        // the numbers say, the block on disk is no longer a live answer.
        let mut s = test_snapshot(&["github.com"], &[], true);
        s.generated_at = Local::now() - chrono::Duration::minutes(50);
        assert_eq!(Health::of(Some(&s), false, INTERVAL), Health::Stale);
    }

    #[test]
    fn staleness_needs_three_missed_intervals() {
        assert!(!is_stale(&at(0), INTERVAL, Local::now()));
        assert!(!is_stale(&at(-2 * 900), INTERVAL, Local::now()));
        assert!(is_stale(&at(-3 * 900 - 60), INTERVAL, Local::now()));
    }

    #[test]
    fn a_clock_that_jumps_backwards_is_not_stale() {
        assert!(!is_stale(&at(3600), INTERVAL, Local::now()));
    }

    #[test]
    fn a_persistent_failure_is_shown_with_its_streak() {
        let snap = test_snapshot(&["github.com"], &["gist.github.com"], true);
        let mut failures = BTreeMap::new();
        failures.insert("gist.github.com".to_string(), 5);
        let text = status_text(view(&snap, &failures));
        assert!(
            text.contains("gist.github.com (连续 5 轮)"),
            "a host that keeps failing must say so, got:
{text}"
        );
        let html = status_html(view(&snap, &failures));
        assert!(html.contains("连续 5 轮"));
    }

    #[test]
    fn a_single_miss_does_not_look_like_a_trend() {
        let snap = test_snapshot(&["github.com"], &["gist.github.com"], true);
        let mut failures = BTreeMap::new();
        failures.insert("gist.github.com".to_string(), 1);
        let text = status_text(view(&snap, &failures));
        assert!(!text.contains("连续"), "one bad cycle is not a streak");
    }

    #[test]
    fn a_stale_snapshot_explains_itself_on_the_page() {
        let snap = at(-3 * 900 - 60);
        let failures = BTreeMap::new();
        let html = status_html(view(&snap, &failures));
        assert!(
            html.contains("上次成功检测在"),
            "the page must say the copy is stale"
        );
        assert!(html.contains("不会自动刷新"));
    }
}
