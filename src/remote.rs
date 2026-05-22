//! Discovery of Claude Code sessions running on remote hosts (VS Code
//! Remote-SSH, optionally into a devcontainer on that host).
//!
//! The local `/proc` scan in `sessions.rs` can't see these — the `claude`
//! process and its registry live on the remote host. So a background thread
//! SSHes to each relevant host and runs the same `/proc` walk *there*, emitting
//! per live session its registry JSON plus the session's title line, which we
//! parse locally.
//!
//! Hosts are derived from open VS Code window titles ("[Dev Container: … @
//! host]" / "[SSH: host]") — the very windows we already click-to-focus — plus
//! any static `[remote] hosts`. Polling runs off the GTK main loop and writes
//! into a shared snapshot that `rebuild` merges each tick, so SSH latency never
//! blocks the UI.

use crate::config::Remote;
use crate::sessions::{RegistryEntry, Session};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a host's last-known sessions survive after its SSH poll starts
/// failing, before they are dropped. Bridges transient SSH hiccups without
/// leaving ghost rows for a host that is genuinely gone.
const STALE_GRACE: Duration = Duration::from_secs(15);

/// Shell run on each remote host (via `ssh … bash -s`). Walks the remote
/// `/proc` and prints, one line per live `claude` process, a tab-separated
/// record: the registry JSON, then the session's title line (the latest
/// `ai-title`, else the latest `last-prompt`, from the transcript) or empty.
/// Mirrors the local scan's `/proc/<pid>/root` + `$HOME` + `NSpid` traversal so
/// it also reaches a registry/transcript inside a devcontainer. The title is
/// emitted as its raw JSON line and decoded locally, so JSON escapes survive.
const SCAN_SNIPPET: &str = r#"
for d in /proc/[0-9]*; do
  [ -r "$d/comm" ] || continue
  read -r comm < "$d/comm" || continue
  [ "$comm" = claude ] || continue
  stat=$(cat "$d/stat" 2>/dev/null) || continue
  rest=${stat##*) }
  state=${rest%% *}
  case "$state" in T|Z|X) continue ;; esac
  home=$(tr '\0' '\n' < "$d/environ" 2>/dev/null | sed -n 's/^HOME=//p' | head -n1)
  [ -n "$home" ] || continue
  nspid=$(sed -n 's/^NSpid:[[:space:]]*//p' "$d/status" 2>/dev/null | awk '{print $NF}')
  [ -n "$nspid" ] || nspid=${d#/proc/}
  reg="$d/root$home/.claude/sessions/$nspid.json"
  [ -r "$reg" ] || continue
  json=$(tr -d '\r\n' < "$reg")
  [ -n "$json" ] || continue
  cwd=$(printf '%s' "$json" | sed -n 's/.*"cwd":"\([^"]*\)".*/\1/p')
  sid=$(printf '%s' "$json" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p')
  title=""
  if [ -n "$cwd" ] && [ -n "$sid" ]; then
    enc=$(printf '%s' "$cwd" | LC_ALL=C tr -c 'a-zA-Z0-9' '-')
    tx="$d/root$home/.claude/projects/$enc/$sid.jsonl"
    if [ -r "$tx" ]; then
      title=$(tac "$tx" 2>/dev/null | grep -m1 '"type":"ai-title"')
      [ -n "$title" ] || title=$(tac "$tx" 2>/dev/null | grep -m1 '"type":"last-prompt"')
      title=$(printf '%s' "$title" | tr -d '\r\n\t')
    fi
  fi
  printf '%s\t%s\n' "$json" "$title"
done
"#;

/// Per-host cache: the last sessions seen and when the last poll succeeded.
struct HostState {
    sessions: Vec<Session>,
    last_ok: Instant,
}

/// Start the background SSH poller. Returns immediately; the thread lives for
/// the process, refreshing `snapshot` every `cfg.interval_secs`.
pub fn spawn_poller(cfg: Remote, snapshot: Arc<Mutex<Vec<Session>>>) {
    std::thread::spawn(move || {
        let interval = Duration::from_secs(cfg.interval_secs.max(1));
        let mut states: HashMap<String, HostState> = HashMap::new();
        loop {
            poll_once(&cfg, &mut states);
            let merged: Vec<Session> =
                states.values().flat_map(|s| s.sessions.iter().cloned()).collect();
            if let Ok(mut g) = snapshot.lock() {
                *g = merged;
            }
            std::thread::sleep(interval);
        }
    });
}

/// One poll round: refresh the host set, then SSH each host. A host that drops
/// out of the set (its window closed) is forgotten immediately; one whose poll
/// fails keeps its last sessions — flagged stale — until `STALE_GRACE` passes.
fn poll_once(cfg: &Remote, states: &mut HashMap<String, HostState>) {
    let hosts = derive_hosts(&cfg.hosts);
    states.retain(|h, _| hosts.contains(h));
    for host in &hosts {
        match scan_host(host, cfg.connect_timeout_secs) {
            Ok(mut sessions) => {
                for s in &mut sessions {
                    s.stale = false;
                }
                states.insert(host.clone(), HostState { sessions, last_ok: Instant::now() });
            }
            Err(()) => match states.get_mut(host) {
                Some(st) if st.last_ok.elapsed() <= STALE_GRACE => {
                    for s in &mut st.sessions {
                        s.stale = true;
                    }
                }
                _ => {
                    states.remove(host);
                }
            },
        }
    }
}

/// SSH to `host`, run the scan snippet, and parse the emitted registry JSON
/// into sessions. Returns `Err` on any SSH/connection failure (caller then
/// keeps the previous snapshot). Reuses one persistent connection per host.
fn scan_host(host: &str, connect_timeout: u64) -> Result<Vec<Session>, ()> {
    let mut child = Command::new("ssh")
        .args([
            "-o", "BatchMode=yes",
            "-o", &format!("ConnectTimeout={connect_timeout}"),
            "-o", "ControlMaster=auto",
            "-o", &format!("ControlPath={}", control_path()),
            "-o", "ControlPersist=30s",
            host, "bash", "-s",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;

    // Feed the snippet, then drop stdin so the remote bash sees EOF and runs.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(SCAN_SNIPPET.as_bytes());
    }
    let out = child.wait_with_output().map_err(|_| ())?;
    if !out.status.success() {
        return Err(());
    }

    let mut sessions = Vec::new();
    for raw in String::from_utf8_lossy(&out.stdout).lines() {
        if raw.trim().is_empty() {
            continue;
        }
        // Each record is "<registry-json>\t<title-json-line>" (title may be
        // empty). JSON escapes control chars, so neither half has a literal tab.
        let (json, title_line) = raw.split_once('\t').unwrap_or((raw, ""));
        let Ok(reg) = serde_json::from_str::<RegistryEntry>(json.trim()) else {
            continue;
        };
        if reg.kind.as_deref() != Some("interactive") {
            continue;
        }
        sessions.push(Session {
            host_pid: 0, // remote pid; unused — focus is by window title
            session_id: reg.session_id,
            cwd: reg.cwd,
            status: reg.status.unwrap_or_else(|| "unknown".into()),
            started_at: reg.started_at,
            updated_at: reg.updated_at,
            title: title_from_line(title_line),
            in_container: true, // forces window_for's title-match focus path
            focused: false,
            remote_host: Some(host.to_string()),
            stale: false,
        });
    }
    Ok(sessions)
}

/// Pull a display title from a transcript line emitted by the scan snippet — an
/// `ai-title` (preferred) or `last-prompt` object. Parsed with serde_json so
/// JSON escapes decode exactly as the local title reader does. Empty/garbage
/// lines yield `None`.
fn title_from_line(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let t = v
        .get("aiTitle")
        .or_else(|| v.get("lastPrompt"))
        .and_then(|x| x.as_str())?
        .trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Hosts to scan: those named in open VS Code window titles, plus any static
/// `hosts`, minus the local machine (so we never SSH to ourselves).
fn derive_hosts(static_hosts: &[String]) -> HashSet<String> {
    let mut hosts = HashSet::new();
    if let Ok(out) = Command::new("wmctrl").arg("-lp").output() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(h) = host_from_title(line) {
                hosts.insert(h);
            }
        }
    }
    for h in static_hosts {
        hosts.insert(h.clone());
    }
    let local = local_names();
    hosts.retain(|h| !local.contains(&h.to_lowercase()));
    hosts
}

/// Extract the remote host from a VS Code window title:
/// "… [Dev Container: <name> @ <host>] …" or "… [SSH: <host>] …".
/// A local devcontainer (no "@ host") yields `None` — /proc already sees it.
fn host_from_title(title: &str) -> Option<String> {
    if let Some(i) = title.find("[Dev Container:") {
        if let Some(at) = title[i..].find("@ ") {
            if let Some(host) = bracketed_host(&title[i + at + 2..]) {
                return Some(host);
            }
        }
    }
    if let Some(i) = title.find("[SSH:") {
        if let Some(host) = bracketed_host(&title[i + 5..]) {
            return Some(host);
        }
    }
    None
}

/// The trimmed text up to the next `]`, if non-empty.
fn bracketed_host(after: &str) -> Option<String> {
    let end = after.find(']')?;
    let host = after[..end].trim();
    (!host.is_empty()).then(|| host.to_string())
}

/// Names that mean "this machine", so a window pointing here isn't SSH-scanned.
fn local_names() -> HashSet<String> {
    let mut names: HashSet<String> =
        ["localhost", "127.0.0.1"].iter().map(|s| s.to_string()).collect();
    if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let h = h.trim().to_lowercase();
        if !h.is_empty() {
            if let Some(short) = h.split('.').next() {
                names.insert(short.to_string());
            }
            names.insert(h);
        }
    }
    names
}

/// Path for the shared SSH control socket — one persistent connection per host,
/// reused across polls. `%r@%h:%p` keeps it unique per user/host/port.
fn control_path() -> String {
    let base = dirs::home_dir()
        .map(|h| h.join(".ssh"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    base.join("cm-agent-observer-%r@%h:%p").to_string_lossy().into_owned()
}
