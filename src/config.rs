use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// User configuration, loaded from `~/.config/agent-observer/config.toml`.
/// The file is auto-created with these defaults on first run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Bar height in pixels (also the reserved strut size).
    pub bar_height: i32,
    /// Screen edge to dock the bar to: "top" or "bottom".
    pub position: String,
    /// Hide the bar entirely (and free its reserved space) when no Claude
    /// sessions are running; show it again as soon as one appears.
    pub hide_when_empty: bool,
    /// How often to re-scan the session registry, in seconds.
    pub poll_interval_secs: u64,

    /// Thickness in pixels of the accent line along the bar's inner edge
    /// (bottom edge when docked top, top edge when docked bottom).
    pub line_width: i32,
    /// Width of the separator drawn between sessions, in pixels.
    pub separator_width: i32,

    /// Font family for row labels, e.g. "Sans", "JetBrains Mono".
    pub font_family: String,
    /// Font size in points.
    pub font_size: u32,
    /// Diameter in pixels of the status dot (the circle before each session).
    pub status_dot_size: u32,

    /// What is shown per session. Pango markup; field VALUES are auto-escaped.
    /// Placeholders: {idx} {project} {title} {status} {uptime} {pid} {cwd} {dc}
    ///   {idx} = jump number (1..9, 0 for the 10th; empty beyond)
    ///   {dc}  = devcontainer marker (pre-styled, empty for host sessions)
    pub label_format: String,
    /// Truncate the {title} field to this many characters.
    pub max_title_len: usize,
    /// Pulse the status dot of `busy` sessions.
    pub pulse_busy: bool,

    /// Show the 5h / weekly rate-limit bars on the far right of the bar.
    /// Requires the Claude Code status line to emit `rate_limits` to
    /// `~/.claude/agent-observer-usage.json` (see install.sh / statusline).
    pub show_usage: bool,
    /// Width in px of each usage bar's track.
    pub usage_bar_width: i32,
    /// Label drawn before the 5-hour bar.
    pub usage_label_5h: String,
    /// Label drawn before the 7-day (weekly) bar.
    pub usage_label_7d: String,
    /// Fill switches to `usage_med` at/above this percentage.
    pub usage_warn_pct: f64,
    /// Fill switches to `usage_high` at/above this percentage.
    pub usage_crit_pct: f64,
    /// Hide the bars if the captured data is older than this many seconds
    /// (0 = never hide, always show the last-known values).
    pub usage_max_age_secs: u64,

    /// Status -> color (any CSS color string).
    pub colors: Colors,
    /// Two-step jump shortcut.
    pub shortcut: Shortcut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Colors {
    pub busy: String,
    pub idle: String,
    pub waiting: String,
    pub interrupted: String,
    pub unknown: String,
    /// Bar background color.
    pub background: String,
    /// Row text color.
    pub text: String,
    /// {project} color of the currently-focused session.
    pub focused: String,
    /// Bottom line AND inter-session separator color.
    pub line: String,
    /// Empty (background) part of a usage bar.
    pub usage_track: String,
    /// Usage-bar fill below `usage_warn_pct`.
    pub usage_low: String,
    /// Usage-bar fill at/above `usage_warn_pct`.
    pub usage_med: String,
    /// Usage-bar fill at/above `usage_crit_pct`.
    pub usage_high: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcut {
    /// Enable the global two-step jump shortcut.
    pub enabled: bool,
    /// Prefix key. Press it, then a digit 1..9/0 to jump to that session.
    /// Format: "mod+mod+key", mods = ctrl|shift|alt|super, e.g. "ctrl+b".
    /// The key may itself be a bare modifier — set it to "super" to use the
    /// Windows/Super key alone as the prefix.
    /// NOTE: this is a GLOBAL grab — if you live in tmux, "ctrl+b" will be
    /// captured here instead of by tmux. Pick something free like "super+c".
    pub prefix: String,
}

impl Default for Colors {
    fn default() -> Self {
        Colors {
            busy: "#3fb950".into(),        // green
            idle: "#8b949e".into(),        // grey
            waiting: "#e3b341".into(),     // amber
            interrupted: "#f85149".into(), // red
            unknown: "#6e7681".into(),     // dim grey
            background: "#0d1117".into(),
            text: "#e6edf3".into(),
            focused: "#f2cc60".into(),     // yellow
            line: "#2f81f7".into(),        // blue
            usage_track: "#30363d".into(), // dim grey
            usage_low: "#3fb950".into(),   // green
            usage_med: "#e3b341".into(),   // amber
            usage_high: "#f85149".into(),  // red
        }
    }
}

impl Default for Shortcut {
    fn default() -> Self {
        Shortcut {
            enabled: true,
            prefix: "ctrl+b".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bar_height: 30,
            position: "top".into(),
            hide_when_empty: true,
            poll_interval_secs: 1,
            line_width: 5,
            separator_width: 5,
            font_family: "Sans".into(),
            font_size: 10,
            status_dot_size: 10,
            label_format:
                "<span size='small' alpha='45%'>{idx}</span>  \
                 <b>{project}</b>{dc}  \
                 <span size='small' alpha='65%'>{title}</span>"
                    .into(),
            max_title_len: 60,
            pulse_busy: true,
            show_usage: true,
            usage_bar_width: 70,
            usage_label_5h: "5h".into(),
            usage_label_7d: "wk".into(),
            usage_warn_pct: 50.0,
            usage_crit_pct: 80.0,
            usage_max_age_secs: 0,
            colors: Colors::default(),
            shortcut: Shortcut::default(),
        }
    }
}

impl Config {
    pub fn config_path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("agent-observer").join("config.toml")
    }

    /// Load config, creating the file with defaults if it does not exist.
    /// On any parse error, falls back to defaults (and logs to stderr).
    pub fn load() -> Config {
        let path = Self::config_path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            match toml::from_str::<Config>(&text) {
                Ok(cfg) => return cfg,
                Err(e) => {
                    eprintln!("agent-observer: config parse error ({e}); using defaults");
                    return Config::default();
                }
            }
        }
        let cfg = Config::default();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = toml::to_string_pretty(&cfg) {
            let _ = std::fs::write(&path, format!("{}{}", Self::header_comment(), text));
        }
        cfg
    }

    /// Comment block prepended to a freshly-generated config file.
    fn header_comment() -> &'static str {
        "# agent-observer configuration\n\
         #\n\
         # label_format placeholders (Pango markup; field VALUES are auto-escaped):\n\
         #   {idx}     jump number in square brackets, e.g. [1] (empty past the 10th)\n\
         #   {project} session project (cwd basename)\n\
         #   {title}   AI title / last prompt (truncated to max_title_len)\n\
         #   {status}  busy | idle | waiting | interrupted\n\
         #   {uptime}  session uptime, e.g. 12m, 3h04m\n\
         #   {pid}     host process id\n\
         #   {cwd}     full working directory\n\
         #   {dc}      devcontainer marker (empty for host sessions)\n\
         #\n\
         # position        : dock the bar to the \"top\" or \"bottom\" screen edge\n\
         # hide_when_empty : hide the bar (freeing its space) when no sessions run\n\
         # status_dot_size : diameter in px of the status circle before each session\n\
         # colors.focused : {project} color of the currently-focused session\n\
         # colors.line    : bottom line AND the inter-session separators\n\
         # line_width / separator_width : thickness in px\n\
         # shortcut.prefix: press it, then 1..9/0 to jump. GLOBAL grab \u{2014}\n\
         #                  change it if it clashes with tmux's ctrl+b.\n\
         #\n\
         # Usage bars (far right): 5h + weekly rate-limit utilisation, fed by the\n\
         # Claude Code status line writing ~/.claude/agent-observer-usage.json.\n\
         #   show_usage         : master on/off toggle\n\
         #   usage_bar_width    : px width of each bar's track\n\
         #   usage_label_5h/7d  : text drawn before each bar\n\
         #   usage_warn_pct     : fill turns colors.usage_med at/above this %\n\
         #   usage_crit_pct     : fill turns colors.usage_high at/above this %\n\
         #   usage_max_age_secs : hide bars if data older than this (0 = never)\n\
         #   colors.usage_track/usage_low/usage_med/usage_high : bar colors\n\
         \n"
    }
}
