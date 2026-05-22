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

    /// Uptime in a short human form, e.g. "12m", "3h04m".
    pub fn uptime(&self) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
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
            let project = basename(&s.cwd).to_string();
            s.focused = is_focused(&wins, active, s.host_pid, &project, s.in_container);
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

/// Raise/focus the window hosting a session.
pub fn focus_session(host_pid: i32, project: &str, in_container: bool) -> bool {
    let wins = list_windows();
    match window_for(&wins, host_pid, project, in_container) {
        Some(w) => activate(&w.id),
        None => false,
    }
}

/// Find the window hosting a session.
///
/// A single VS Code process backs every workspace window, so process ancestry
/// can't tell them apart — the title is the only discriminator. We therefore
/// (1) prefer a VS Code window whose title names the project, then
/// (2) for host sessions, walk the process ancestry (works for terminals,
///     which own a unique window), then
/// (3) fall back to any window naming the project.
fn window_for<'a>(
    wins: &'a [Win],
    host_pid: i32,
    project: &str,
    in_container: bool,
) -> Option<&'a Win> {
    if !project.is_empty() {
        if let Some(w) = wins.iter().find(|w| {
            w.title.to_lowercase().contains("visual studio code")
                && title_names_project(&w.title, project)
        }) {
            return Some(w);
        }
    }

    if !in_container {
        if let Some(w) =
            ancestry(host_pid).find_map(|pid| wins.iter().find(|w| w.pid == pid))
        {
            return Some(w);
        }
    }

    if project.is_empty() {
        return None;
    }
    wins.iter().find(|w| title_names_project(&w.title, project))
}

/// Is the active window attributable to this session?
///
/// Primary signal: the active window's title names the project (covers VS
/// Code's many-windows-one-process case). Fallback for terminals: the active
/// window is the *unique* window owned by a pid in the session's ancestry.
fn is_focused(
    wins: &[Win],
    active: Option<u64>,
    host_pid: i32,
    project: &str,
    in_container: bool,
) -> bool {
    let Some(active_id) = active else { return false };
    let Some(aw) = wins.iter().find(|w| norm_winid(&w.id) == Some(active_id)) else {
        return false;
    };

    if title_names_project(&aw.title, project) {
        return true;
    }

    if !in_container && wins.iter().filter(|w| w.pid == aw.pid).count() == 1 {
        return ancestry(host_pid).any(|pid| pid == aw.pid);
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
