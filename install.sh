#!/usr/bin/env bash
# Build agent-observer (release), install the binary to ~/.local/bin, and
# register an XFCE autostart entry so it launches on login.
set -euo pipefail

cd "$(dirname "$0")"

echo "==> building (release)…"
cargo build --release

BIN="$HOME/.local/bin/agent-observer"
mkdir -p "$HOME/.local/bin"
install -m 0755 target/release/agent-observer "$BIN"
echo "==> installed $BIN"

# Application-menu entry (so you can relaunch from the start menu after closing).
APPS_DIR="$HOME/.local/share/applications"
mkdir -p "$APPS_DIR"
cat > "$APPS_DIR/agent-observer.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=agent-observer
Comment=Overview of ongoing Claude Code sessions
Exec=$BIN
Icon=utilities-system-monitor
Terminal=false
StartupNotify=false
Categories=Utility;
EOF
echo "==> installed menu entry $APPS_DIR/agent-observer.desktop"
# Refresh the menu database if the tool is available (ignore if not).
update-desktop-database "$APPS_DIR" 2>/dev/null || true

# Autostart entry (launches on login). Reuses the menu entry plus the
# autostart-enabled flag.
AUTOSTART_DIR="$HOME/.config/autostart"
mkdir -p "$AUTOSTART_DIR"
cat > "$AUTOSTART_DIR/agent-observer.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=agent-observer
Comment=Overview of ongoing Claude Code sessions
Exec=$BIN
Icon=utilities-system-monitor
Terminal=false
X-GNOME-Autostart-enabled=true
StartupNotify=false
Categories=Utility;
EOF
echo "==> installed autostart entry $AUTOSTART_DIR/agent-observer.desktop"

echo
echo "Done. Run it now with:  $BIN"
echo "It will also start automatically on your next login."
echo
echo "Usage bars (5h/weekly): the bar reads ~/.claude/agent-observer-usage.json,"
echo "which the Claude Code status line must write. Add this near the top of your"
echo "statusLine command script (after it has read stdin into \$input):"
echo
echo '  rate_limits=$(echo "$input" | jq -c '"'"'.rate_limits // empty'"'"' 2>/dev/null)'
echo '  if [ -n "$rate_limits" ]; then'
echo '    t="$HOME/.claude/.agent-observer-usage.json.tmp"'
echo '    printf '"'"'{"captured_at":%s,"rate_limits":%s}\n'"'"' "$(date +%s)" "$rate_limits" \'
echo '      > "$t" && mv -f "$t" "$HOME/.claude/agent-observer-usage.json"'
echo '  fi'
