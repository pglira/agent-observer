# agent-observer

An always-on-top **top bar** for XFCE/X11 that gives an at-a-glance overview of your
ongoing Claude Code sessions, and lets you click one to jump to the window hosting it.

## Goal

A thin, full-width bar pinned to the top edge of the screen showing one row per live
Claude Code session: a status dot + the project name and the session's AI title.
Clicking a row raises/focuses the terminal or VS Code window running that session.

## Decisions (from design interview)

| Area | Decision |
|------|----------|
| Form factor | Standalone always-on-top bar, full screen width, pinned to top edge (y=0). No autohide. |
| Language / toolkit | **Rust + GTK3** (`gtk` 0.18). GTK3 keeps the X11 panel APIs GTK4 dropped. |
| Window behavior | Borderless, `DOCK` type hint, keep-above, **reserves `_NET_WM_STRUT`** (top ~30px). |
| Focus highlight | The session whose host window is currently active shows its `{project}` in `colors.focused` (yellow). Detected by matching the active window's title to the project (VS Code uses one process for many windows, so title — not pid — is the reliable signal), with a unique-window ancestry fallback for terminals. Updates **immediately** on focus change: `active_watch.rs` watches `_NET_ACTIVE_WINDOW` on the root window and re-renders on change, rather than waiting for the poll tick. |
| Accent line | A `line_width`px line (default 5, `colors.line` blue) along the bar's inner edge — the bottom when docked top, the top when docked bottom (CSS `border-*` on `#rowbox`). |
| Separators | A `separator_width`px divider between sessions, same `colors.line` color. Drawn as a `border-right` on each session's clickable cell (not a standalone widget), so the whole inter-divider strip — dividers included — is part of a session's click target. |
| Data source | **Process-anchored**: scan `/proc` for running `claude` processes (host *and* containers), read each one's registry via `/proc/<pid>/root` + the process's own `$HOME`. Covers Dev Containers, whose registry lives inside the container FS. |
| Which sessions | Live **interactive** only: any running `claude` process with `kind == "interactive"`. Liveness is implicit (the process exists); de-duped by `sessionId`. |
| Container detection | A session is "in a container" when its mount namespace (`/proc/<pid>/ns/mnt`) differs from ours. Marked with a dim `⬢dc` tag. |
| Status | `status` field: `busy` / `idle` / `waiting` / `interrupted`. |
| Icon | Status dot (icon == status). `waiting`=amber, `busy`=green (pulsing), `idle`=grey, `interrupted`=red. |
| Session name | `cwd` basename (project) + AI title from the session `.jsonl` (`ai-title` line), falls back to last prompt. |
| Click → jump | Host sessions: walk the process ancestry to the window-owning pid (`wmctrl -lp`) then `wmctrl -ia`. Container sessions (ancestry can't cross the boundary): match a window whose title contains the project name, preferring VS Code. |
| Refresh | GLib timer, re-scan registry every `poll_interval_secs` (default 1s). |
| Config | `~/.config/agent-observer/config.toml`, auto-created with defaults. |
| Label content | Configurable `label_format` Pango-markup template. Placeholders: `{idx} {project} {title} {status} {uptime} {pid} {cwd} {dc}` (values auto-escaped). `{idx}` renders as the bare jump number `1`..`9`, `0` for the tenth (decorate it in the template if you want, e.g. `[{idx}]`). The base text color is applied via markup (not CSS) so the focused-name color can override per-run. |
| Font / size | `font_family` + `font_size` (pt). Plus `bar_height` (forced via min size-request, not just default-size), `[colors]`. No opacity param. |
| Jump shortcut | Two-step global grab: `[shortcut] prefix` (e.g. `ctrl+b`), then a digit `1..9`/`0` focuses the Nth visible session. Implemented via `XGrabKey` on root + `XGrabKeyboard` for the second key, driven by the GLib main loop (`hotkey.rs`). Auto-disarms after 2s. NB: a global `ctrl+b` grab will shadow tmux's prefix — change it if you live in tmux. |
| Launch | `cargo build --release` + `~/.config/autostart/agent-observer.desktop` (starts on login). |
| Sort | `waiting` first, then by `started_at`. |
| Empty state | Bar stays visible showing "No active Claude sessions". |
| Extras | Right-click menu: Reload config / Quit. Hover tooltip: full cwd, status, uptime. |

## Data model

A registry file `~/.claude/sessions/<pid>.json` looks like:

```json
{"pid":5653,"sessionId":"091cd655-...","cwd":"/data2/repos/agent-observer",
 "startedAt":1779295601143,"version":"2.1.145","kind":"interactive",
 "entrypoint":"cli","status":"busy","updatedAt":1779295949872,
 "bridgeSessionId":"session_..."}
```

The AI title lives in `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl` as a line
`{"type":"ai-title","aiTitle":"...","sessionId":"..."}`. `<encoded-cwd>` is the cwd
with every non-alphanumeric character replaced by `-`.

## Module layout

- `src/config.rs` — TOML config load/auto-create (with a commented placeholder reference header); colors, fonts, height, lines/separators, label template, shortcut.
- `src/sessions.rs` — process-anchored session discovery (host + containers), AI-title cache, window focus (ancestry + title fallback).
- `src/hotkey.rs` — global two-step jump shortcut (X11 key grab driven by the GLib loop).
- `src/main.rs` — GTK3 bar: dock window, strut, rows, label templating, polling, click + context menu.

## Build & install

```bash
cargo build --release
./install.sh          # copies binary to ~/.local/bin and installs autostart entry
```
