use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One entry from a `<pid>.json` session-registry file.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryEntry {
    pub cwd: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(rename = "startedAt", default)]
    pub started_at: u64,
    /// When the session's status last changed (ms since epoch). For a non-busy
    /// session this is when the agent stopped working — the idle-glow's t=0.
    #[serde(rename = "updatedAt", default)]
    pub updated_at: u64,
}

/// One rate-limit window from the usage file (the `resets_at` key is also
/// present on disk but currently unused, so it is simply ignored).
#[derive(Debug, Clone, Deserialize)]
pub struct RateWindow {
    #[serde(default)]
    pub used_percentage: f64,
}

/// The 5h + weekly windows; either may be absent (each is independently
/// populated only after the relevant window has seen traffic).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RateLimits {
    #[serde(default)]
    pub five_hour: Option<RateWindow>,
    #[serde(default)]
    pub seven_day: Option<RateWindow>,
}

/// Account-wide rate-limit utilisation, deserialized straight from
/// `~/.claude/agent-observer-usage.json` (written by the Claude Code status
/// line). `captured_at` is 0 when absent and is then filled from file mtime.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub rate_limits: RateLimits,
    #[serde(default)]
    pub captured_at: u64,
}

/// A resolved, displayable session.
#[derive(Debug, Clone)]
pub struct Session {
    /// Host-side pid of the `claude` process (used for click-to-focus).
    pub host_pid: i32,
    pub session_id: String,
    pub cwd: String,
    pub status: String,
    pub started_at: u64,
    /// When the status last changed (ms since epoch); 0 if unknown. For a
    /// non-busy session, `now - updated_at` is how long the agent has been done.
    pub updated_at: u64,
    pub title: Option<String>,
    /// True if the session runs inside a container (different mount namespace).
    pub in_container: bool,
    /// True if this session's host window is the currently-focused window.
    pub focused: bool,
    /// Host this session was discovered on over SSH (`None` for local ones).
    pub remote_host: Option<String>,
    /// True when this session's host last failed an SSH poll (shown dimmed,
    /// dropped once it has been stale too long). Always false for local ones.
    pub stale: bool,
}

impl Session {
    /// Last path component of cwd, e.g. "slamlab".
    pub fn project(&self) -> &str {
        basename(&self.cwd)
    }

    /// A borrowing view of just the fields needed to find or recognise this
    /// session's window. See [`WindowQuery`].
    pub fn window_query(&self) -> WindowQuery<'_> {
        WindowQuery {
            host_pid: self.host_pid,
            project: self.project(),
            in_container: self.in_container,
            remote_host: self.remote_host.as_deref(),
        }
    }

    /// Uptime in a short human form, e.g. "12m", "3h04m".
    pub fn uptime(&self) -> String {
        let now = now_ms();
        if self.started_at == 0 || now < self.started_at {
            return "—".into();
        }
        let secs = (now - self.started_at) / 1000;
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        if h > 0 {
            format!("{h}h{m:02}m")
        } else if m > 0 {
            format!("{m}m")
        } else {
            format!("{secs}s")
        }
    }

    /// Seconds since the status last changed (`updated_at`). For a non-busy
    /// session this is how long the agent has been done. 0 if `updated_at` is
    /// unknown or in the future (e.g. remote clock skew).
    pub fn idle_secs(&self) -> u64 {
        let now = now_ms();
        if self.updated_at == 0 || now < self.updated_at {
            return 0;
        }
        (now - self.updated_at) / 1000
    }
}

/// Current time in milliseconds since the Unix epoch (0 if the system clock is
/// somehow before the epoch).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Caches AI titles per session, keyed by transcript mtime so we only re-read
/// the (potentially large) .jsonl when it actually changed.
#[derive(Default)]
pub struct TitleCache {
    entries: HashMap<String, (Option<SystemTime>, Option<String>)>,
}

impl TitleCache {
    /// Discover live interactive sessions by scanning every running `claude`
    /// process on the host — including ones inside containers, whose registry
    /// is read through `/proc/<pid>/root` + the process's own `$HOME`.
    /// Returns sessions sorted: `waiting` first, then by start time, plus the
    /// freshest rate-limit usage found across the discovered session homes.
    pub fn scan(&mut self, remote: &[Session]) -> (Vec<Session>, Option<Usage>) {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        // (root, home) pairs to probe for a usage file. Our own home is always
        // a candidate, so usage survives even when no session is detected.
        let mut homes: HashSet<(String, String)> = HashSet::new();
        if let Some(h) = dirs::home_dir() {
            homes.insert((String::new(), h.to_string_lossy().into_owned()));
        }

        // Resolve windows + active window once for focus highlighting.
        let wins = list_windows();
        let active = active_window_id();

        let Ok(read) = std::fs::read_dir("/proc") else {
            return (out, read_best_usage(&homes));
        };

        for entry in read.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
            else {
                continue; // not a pid dir
            };

            // Only claude processes.
            match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
                Ok(c) if c.trim() == "claude" => {}
                _ => continue,
            }

            // Skip processes that can't be a usable session even though they
            // still exist: stopped (T — e.g. a claude orphaned to init when its
            // editor/terminal window closed), zombie (Z) or dead (X).
            if matches!(proc_state(pid), Some('T') | Some('Z') | Some('X')) {
                continue;
            }

            let Some(home) = proc_home(pid) else { continue };
            let container_pid = proc_container_pid(pid).unwrap_or(pid);
            let root = format!("/proc/{pid}/root");
            homes.insert((root.clone(), home.clone()));

            // <root><home>/.claude/sessions/<container_pid>.json
            let reg_path = PathBuf::from(format!(
                "{root}{home}/.claude/sessions/{container_pid}.json"
            ));
            let Ok(text) = std::fs::read_to_string(&reg_path) else {
                continue;
            };
            let Ok(reg) = serde_json::from_str::<RegistryEntry>(&text) else {
                continue;
            };
            if reg.kind.as_deref() != Some("interactive") {
                continue;
            }
            if !seen.insert(reg.session_id.clone()) {
                continue; // de-dupe across sibling processes
            }

            let title = self.title_for(&root, &home, &reg);
            let container = in_container(pid);
            out.push(Session {
                host_pid: pid,
                session_id: reg.session_id,
                cwd: reg.cwd,
                status: reg.status.unwrap_or_else(|| "unknown".into()),
                started_at: reg.started_at,
                updated_at: reg.updated_at,
                title,
                in_container: container,
                focused: false,
                remote_host: None,
                stale: false,
            });
        }

        // Merge remote (SSH-discovered) sessions, de-duped against locals by
        // session id, then resolve focus uniformly: a session is focused when
        // the active window's title names its project — which covers VS Code's
        // one-process-many-windows case for local and remote rows alike.
        for r in remote {
            if seen.insert(r.session_id.clone()) {
                out.push(r.clone());
            }
        }
        for s in &mut out {
            s.focused = is_focused(&wins, active, s.window_query());
        }

        // Stable order: `waiting` (needs you) first, then everything else by
        // start time. Crucially, all non-waiting statuses share one rank so a
        // session flipping busy<->idle doesn't jump position; `session_id` is
        // the final tiebreaker so the order never depends on `/proc` readdir or
        // the remote merge's (HashMap) iteration order.
        out.sort_by(|a, b| {
            let rank = |s: &str| if s == "waiting" { 0 } else { 1 };
            rank(&a.status)
                .cmp(&rank(&b.status))
                .then(a.started_at.cmp(&b.started_at))
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        (out, read_best_usage(&homes))
    }

    fn title_for(&mut self, root: &str, home: &str, reg: &RegistryEntry) -> Option<String> {
        let path = transcript_path(root, home, &reg.cwd, &reg.session_id);
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();

        if let Some((cached_mtime, title)) = self.entries.get(&reg.session_id) {
            if *cached_mtime == mtime {
                return title.clone();
            }
        }

        let title = read_title(&path);
        self.entries
            .insert(reg.session_id.clone(), (mtime, title.clone()));
        title
    }
}

/// Read the usage file under each candidate home and return the freshest one.
/// The file is account-wide, so the most recently captured render wins — i.e.
/// the session you were last actively driving. Falls back to file mtime when
/// the `captured_at` field is missing.
fn read_best_usage(homes: &HashSet<(String, String)>) -> Option<Usage> {
    let mut best: Option<Usage> = None;
    for (root, home) in homes {
        let path =
            PathBuf::from(format!("{root}{home}/.claude/agent-observer-usage.json"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut usage) = serde_json::from_str::<Usage>(&text) else {
            continue;
        };
        if usage.captured_at == 0 {
            usage.captured_at = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
        }
        if best.as_ref().is_none_or(|b| usage.captured_at >= b.captured_at) {
            best = Some(usage);
        }
    }
    best
}

/// `$HOME` of a process from its environ (readable when we own the process).
fn proc_home(pid: i32) -> Option<String> {
    let data = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    for kv in data.split(|&b| b == 0) {
        if let Some(rest) = kv.strip_prefix(b"HOME=") {
            return Some(String::from_utf8_lossy(rest).into_owned());
        }
    }
    None
}

/// The pid as seen inside the process's own pid namespace (last field of
/// `NSpid:` in `/proc/<pid>/status`) — this is what names the registry file.
fn proc_container_pid(pid: i32) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("NSpid:"))?;
    line.split_whitespace().last()?.parse::<i32>().ok()
}

/// A process is "in a container" if its mount namespace differs from ours.
fn in_container(pid: i32) -> bool {
    let ours = std::fs::read_link("/proc/self/ns/mnt").ok();
    let theirs = std::fs::read_link(format!("/proc/{pid}/ns/mnt")).ok();
    match (ours, theirs) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    }
}

/// `<root><home>/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`
fn transcript_path(root: &str, home: &str, cwd: &str, session_id: &str) -> PathBuf {
    let encoded: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    PathBuf::from(format!(
        "{root}{home}/.claude/projects/{encoded}/{session_id}.jsonl"
    ))
}

/// Most recent `ai-title` in the transcript; falls back to the last prompt.
fn read_title(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut title: Option<String> = None;
    let mut last_prompt: Option<String> = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                    title = Some(t.to_string());
                }
            }
            Some("last-prompt") => {
                if let Some(p) = v.get("lastPrompt").and_then(|t| t.as_str()) {
                    last_prompt = Some(p.to_string());
                }
            }
            _ => {}
        }
    }
    title.or(last_prompt).map(|s| s.trim().to_string())
}

/// A top-level window from `wmctrl -lp`.
struct Win {
    id: String,
    pid: i32,
    title: String,
}

/// Last path component of a path, e.g. "/workspaces/slamlab" -> "slamlab".
fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// The fields needed to find or recognise a session's window. A single VS Code
/// process backs every workspace window, so we can't tell them apart by pid;
/// the window title (project name + remote indicator) is the discriminator,
/// with process ancestry as a terminal-only fallback. Borrowed, so it's cheap
/// to pass around — build one with [`Session::window_query`].
#[derive(Clone, Copy)]
pub struct WindowQuery<'a> {
    /// Host-side pid of the `claude` process (used for the ancestry fallback).
    pub host_pid: i32,
    /// Project (cwd basename) the window's title must name.
    pub project: &'a str,
    /// Whether the session runs in a container (disables the ancestry fallback,
    /// whose pids live in another namespace).
    pub in_container: bool,
    /// Host the session lives on (`None` = local); the window's remote indicator
    /// must agree. See [`window_on_host`].
    pub remote_host: Option<&'a str>,
}

/// Raise/focus the window hosting a session.
pub fn focus_session(q: WindowQuery) -> bool {
    let wins = list_windows();
    match window_for(&wins, q) {
        Some(w) => activate(&w.id),
        None => false,
    }
}

/// Find the window hosting a session. We
/// (1) prefer a VS Code window whose title matches the session (project name
///     *and* remote indicator, so the same folder open both locally and in a
///     remote/devcontainer window doesn't focus the wrong one), then
/// (2) for host sessions, walk the process ancestry (works for terminals,
///     which own a unique window), then
/// (3) fall back to any window matching the session.
fn window_for<'a>(wins: &'a [Win], q: WindowQuery) -> Option<&'a Win> {
    if !q.project.is_empty() {
        if let Some(w) = wins.iter().find(|w| {
            w.title.to_lowercase().contains("visual studio code") && window_matches(&w.title, q)
        }) {
            return Some(w);
        }
    }

    if !q.in_container {
        if let Some(w) =
            ancestry(q.host_pid).find_map(|pid| wins.iter().find(|w| w.pid == pid))
        {
            return Some(w);
        }
    }

    if q.project.is_empty() {
        return None;
    }
    wins.iter().find(|w| window_matches(&w.title, q))
}

/// Does this window belong to the queried session? Both must hold: the title
/// names the project as a whole token, and the window's remote indicator agrees
/// with where the session lives. See [`title_names_project`], [`window_on_host`].
fn window_matches(title: &str, q: WindowQuery) -> bool {
    title_names_project(title, q.project) && window_on_host(title, q.remote_host)
}

/// Does this window's remote indicator match the session's host? A remote
/// session (`remote_host = Some`) belongs to the window whose "[Dev Container:
/// … @ host]" / "[SSH: host]" names that same host; a local session
/// (`remote_host = None`) belongs to a window with no remote indicator (a plain
/// local window, or a *local* devcontainer with no "@ host"). This is what keeps
/// a project opened both locally and over SSH from focusing the wrong window.
fn window_on_host(title: &str, remote_host: Option<&str>) -> bool {
    match (remote_host, crate::remote::host_from_title(title)) {
        (Some(want), Some(have)) => want.eq_ignore_ascii_case(&have),
        (None, None) => true,
        _ => false,
    }
}

/// Is the active window attributable to this session?
///
/// Primary signal: the active window matches the session — title names the
/// project *and* its remote indicator agrees (covers VS Code's many-windows-
/// one-process case without a local window lighting up a remote row, or vice
/// versa). Fallback for terminals: the active window is the *unique* window
/// owned by a pid in the session's ancestry.
fn is_focused(wins: &[Win], active: Option<u64>, q: WindowQuery) -> bool {
    let Some(active_id) = active else { return false };
    let Some(aw) = wins.iter().find(|w| norm_winid(&w.id) == Some(active_id)) else {
        return false;
    };

    if window_matches(&aw.title, q) {
        return true;
    }

    if !q.in_container && wins.iter().filter(|w| w.pid == aw.pid).count() == 1 {
        return ancestry(q.host_pid).any(|pid| pid == aw.pid);
    }
    false
}

/// Does this window title name the given project? Matches the project as a
/// whole token, so "slamlab" does not match the window for "slamlab-paper".
/// VS Code's bracketed remote indicator ("[Dev Container: … @ host]" / "[SSH:
/// host]") is stripped first: its human-chosen label can coincidentally contain
/// a project name (e.g. "[Dev Container: SLAMLAB Development @ …]"). Empty
/// project never matches; comparison is case-insensitive.
fn title_names_project(title: &str, project: &str) -> bool {
    if project.is_empty() {
        return false;
    }
    let haystack = strip_brackets(title).to_lowercase();
    let needle = project.to_lowercase();
    // A folder name is bounded by non-name characters; treat alphanumerics and
    // the usual path/identifier punctuation as "inside a name".
    let is_name_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.');
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(&needle) {
        let i = from + rel;
        let before = haystack[..i].chars().next_back();
        let after = haystack[i + needle.len()..].chars().next();
        if !before.is_some_and(is_name_char) && !after.is_some_and(is_name_char) {
            return true;
        }
        from = i + 1;
    }
    false
}

/// Drop "[…]" segments from a window title — these hold VS Code's remote
/// indicators, whose label is unrelated to (but can echo) the folder name.
fn strip_brackets(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut depth = 0u32;
    for c in title.chars() {
        match c {
            '[' => depth += 1,
            ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// A process's pid and its ancestors (host_pid, parent, grandparent, …), up to
/// a sane depth. A single VS Code process backs many windows, so this only
/// reliably identifies a window for processes that own exactly one (terminals).
fn ancestry(host_pid: i32) -> impl Iterator<Item = i32> {
    std::iter::successors(Some(host_pid), |&p| parent_pid(p))
        .take_while(|&p| p > 1)
        .take(32)
}

/// All top-level windows, parsed from `wmctrl -lp` (winid desktop pid host title).
fn list_windows() -> Vec<Win> {
    let Ok(out) = std::process::Command::new("wmctrl").arg("-lp").output() else {
        return Vec::new();
    };
    let mut wins = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // Columns are whitespace-aligned, so the gaps vary (e.g. a short winid
        // is padded with extra spaces). Split on runs of whitespace for the
        // four fixed columns, then take the remainder as the title.
        let mut it = line.split_whitespace();
        let (Some(id), Some(_desktop), Some(pid), Some(_host)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        let Ok(pid) = pid.parse::<i32>() else { continue };
        wins.push(Win {
            id: id.to_string(),
            pid,
            title: it.collect::<Vec<_>>().join(" "),
        });
    }
    wins
}

/// Currently-focused window id (from `_NET_ACTIVE_WINDOW`), normalized.
fn active_window_id() -> Option<u64> {
    let out = std::process::Command::new("xprop")
        .args(["-root", "_NET_ACTIVE_WINDOW"])
        .output()
        .ok()?;
    // Format: "_NET_ACTIVE_WINDOW(WINDOW): window id # 0x4800004, 0x0"
    let text = String::from_utf8_lossy(&out.stdout);
    let after_hash = text.split('#').nth(1)?;
    let tok = after_hash.split(',').next()?.trim();
    norm_winid(tok)
}

/// Parse a window id like "0x04800004" / "0x4800004" into a number for compare.
fn norm_winid(s: &str) -> Option<u64> {
    let hex = s.trim().trim_start_matches("0x");
    u64::from_str_radix(hex, 16).ok()
}

fn activate(winid: &str) -> bool {
    std::process::Command::new("wmctrl")
        .args(["-i", "-a", winid])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The `/proc/<pid>/stat` fields after the "(comm)" — i.e. starting at `state`
/// (`state ppid pgrp …`). Splitting on the last ')' is robust to spaces and
/// parens inside comm, which is the whole reason both callers go through here.
fn proc_stat_after_comm(pid: i32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(text[text.rfind(')')? + 1..].trim_start().to_owned())
}

/// Parent pid from `/proc/<pid>/stat` (the field after `state`).
fn parent_pid(pid: i32) -> Option<i32> {
    proc_stat_after_comm(pid)?.split_whitespace().nth(1)?.parse().ok()
}

/// Process state char from `/proc/<pid>/stat` (e.g. 'R', 'S', 'T', 'Z').
fn proc_state(pid: i32) -> Option<char> {
    proc_stat_after_comm(pid)?.split_whitespace().next()?.chars().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: &str, title: &str) -> Win {
        Win { id: id.to_string(), pid: 0, title: title.to_string() }
    }

    fn query<'a>(project: &'a str, in_container: bool, remote_host: Option<&'a str>) -> WindowQuery<'a> {
        WindowQuery { host_pid: 0, project, in_container, remote_host }
    }

    #[test]
    fn host_discriminates_same_project_local_vs_remote() {
        // Same folder open both locally and in a remote devcontainer.
        let wins = vec![
            win("0x1", "slamlab — Visual Studio Code"),
            win("0x2", "slamlab [Dev Container: dev @ bigbox] — Visual Studio Code"),
        ];

        // Remote session must pick the remote window, not the first one.
        let w = window_for(&wins, query("slamlab", true, Some("bigbox"))).unwrap();
        assert_eq!(w.id, "0x2");

        // Local session must pick the plain window. No ancestry match (pid 0),
        // so it falls back to the title path.
        let w = window_for(&wins, query("slamlab", false, None));
        assert_eq!(w.unwrap().id, "0x1");
    }

    #[test]
    fn window_on_host_matches_indicator() {
        assert!(window_on_host("p [SSH: bigbox] — Visual Studio Code", Some("bigbox")));
        assert!(window_on_host("p [SSH: BigBox]", Some("bigbox"))); // case-insensitive
        assert!(!window_on_host("p [SSH: other]", Some("bigbox")));
        assert!(!window_on_host("p — Visual Studio Code", Some("bigbox"))); // remote wants host, window has none
        assert!(!window_on_host("p [SSH: bigbox]", None)); // local wants no host
        assert!(window_on_host("p — Visual Studio Code", None));
        // A local devcontainer (no "@ host") counts as local.
        assert!(window_on_host("p [Dev Container: dev]", None));
    }
}
