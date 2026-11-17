#!/bin/bash
set -euo pipefail

LABEL=com.cortexrnd.localflow
AGENT="$HOME/Library/LaunchAgents/$LABEL.plist"
LOGS="$HOME/.localflow"
TARGET="gui/$(id -u)/$LABEL"
DOMAIN="gui/$(id -u)"
restart_rust=0

xml() {
    printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
}

program() {
    plutil -extract ProgramArguments.0 raw -o - "$1" 2>/dev/null || true
}

is_rust() { [[ "$1" == */Contents/MacOS/lf-hub ]]; }

bootout() {
    if [[ "${LOCALFLOW_SKIP_LAUNCHCTL:-0}" != 1 ]]; then
        launchctl bootout "$TARGET" >/dev/null 2>&1 || true
    fi
}

load_agent() {
    if [[ "${LOCALFLOW_SKIP_LAUNCHCTL:-0}" == 1 ]]; then
        echo "Skipping launchctl (LOCALFLOW_SKIP_LAUNCHCTL=1; installer testing only)."
        return
    fi
    local bootstrap_error load_error
    if bootstrap_error="$(launchctl bootstrap "$DOMAIN" "$AGENT" 2>&1)"; then
        return
    fi
    if load_error="$(launchctl load -w "$AGENT" 2>&1)"; then
        return
    fi
    echo "launchctl bootstrap failed: $bootstrap_error; launchctl load fallback also failed: $load_error" >&2
    return 1
}

rust_pids() {
    ps -axww -o pid= -o comm= | while read -r pid executable; do
        if [[ "$executable" == "$EXE" ]]; then printf '%s\n' "$pid"; fi
    done
}

stop_rust() {
    local pid attempt
    old_rust_pids="$(rust_pids)"
    for pid in $old_rust_pids; do kill -TERM "$pid" 2>/dev/null || true; done
    for ((attempt=0; attempt<50; attempt++)); do
        [[ -z "$(rust_pids)" ]] && return 0
        sleep 0.1
    done
    for pid in $(rust_pids); do kill -KILL "$pid" 2>/dev/null || true; done
    for ((attempt=0; attempt<20; attempt++)); do
        [[ -z "$(rust_pids)" ]] && return 0
        sleep 0.1
    done
    echo "Could not stop running Rust LocalFlow processes: $EXE" >&2
    return 1
}

verify_rust_pid() {
    local pid old_pid current_pid attempt fresh
    for ((attempt=0; attempt<50; attempt++)); do
        pid="$(launchctl print "$TARGET" 2>/dev/null | awk '$1 == "pid" && $2 == "=" {print $3; exit}')" || pid=""
        if [[ "$pid" =~ ^[0-9]+$ ]]; then
            fresh=1
            for old_pid in $old_rust_pids; do
                if [[ "$pid" == "$old_pid" ]]; then fresh=0; fi
            done
            if [[ "$fresh" == 1 ]]; then
                for current_pid in $(rust_pids); do
                    if [[ "$current_pid" == "$pid" ]]; then
                        echo "Restarted Rust LocalFlow (PID $pid)"
                        return 0
                    fi
                done
            fi
        fi
        sleep 0.1
    done
    echo "Rust LaunchAgent did not start a new lf-hub process: $EXE" >&2
    return 1
}

case "${1:-}" in
    uninstall)
        rm -f "$AGENT"
        bootout
        exit 0
        ;;
    restore-python)
        # A normal Python installation does not opt the user into autostart.
        [[ -f "$AGENT" ]] && is_rust "$(program "$AGENT")" || exit 0
        runtime=python
        ;;
    install-rust) runtime=rust ;;
    restart-rust) runtime=rust; restart_rust=1 ;;
    *) echo "Usage: $0 install-rust EXE | restart-rust EXE | restore-python EXE | uninstall" >&2; exit 2 ;;
esac

EXE="${2:?runtime executable required}"
[[ "$EXE" == /* && -x "$EXE" ]] || { echo "Runtime executable is missing or not absolute: $EXE" >&2; exit 1; }
if [[ "$runtime" == rust && "$restart_rust" == 0 && -f "$AGENT" && "$(program "$AGENT")" == "$EXE" ]]; then
    # Do not boot out the Hub that is currently enabling its own login agent.
    if [[ "${LOCALFLOW_SKIP_LAUNCHCTL:-0}" == 1 ]] || launchctl print "$TARGET" >/dev/null 2>&1; then
        echo "Rust LaunchAgent already installed: $AGENT"
        exit 0
    fi
fi
mkdir -p "$(dirname "$AGENT")" "$LOGS"
previous="$(mktemp "${AGENT}.previous.XXXXXX")"
pending="$(mktemp "${AGENT}.pending.XXXXXX")"
trap 'rm -f "$previous" "$pending"' EXIT
had_agent=0
if [[ -f "$AGENT" ]]; then
    cp -p "$AGENT" "$previous"
    had_agent=1
    if is_rust "$(program "$AGENT")"; then
        if [[ "$runtime" == python ]]; then cp -p "$AGENT" "$AGENT.rust.bak"; fi
    else
        cp -p "$AGENT" "$AGENT.python.bak"
    fi
fi

if [[ "$runtime" == python && -f "$AGENT.python.bak" ]] \
    && [[ "$(plutil -extract Label raw -o - "$AGENT.python.bak")" == "$LABEL" ]] \
    && ! is_rust "$(program "$AGENT.python.bak")" \
    && [[ -x "$(program "$AGENT.python.bak")" ]]; then
    cp -p "$AGENT.python.bak" "$pending"
else
    cat >"$pending" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>$LABEL</string>
<key>ProgramArguments</key><array><string>$(xml "$EXE")</string></array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
<key>LimitLoadToSessionType</key><string>Aqua</string>
<key>StandardOutPath</key><string>$(xml "$LOGS/agent.out.log")</string>
<key>StandardErrorPath</key><string>$(xml "$LOGS/agent.err.log")</string>
</dict></plist>
EOF
fi
plutil -lint "$pending"
bootout
if [[ "$restart_rust" == 1 && "${LOCALFLOW_SKIP_LAUNCHCTL:-0}" != 1 ]]; then stop_rust; fi
mv -f "$pending" "$AGENT"
if ! load_agent; then
    bootout
    if [[ "$had_agent" == 1 ]]; then
        cp -p "$previous" "$AGENT"
        load_agent || true
    else
        rm -f "$AGENT"
    fi
    exit 1
fi
if [[ "$restart_rust" == 1 && "${LOCALFLOW_SKIP_LAUNCHCTL:-0}" != 1 ]]; then
    launchctl kickstart -k "$TARGET"
    verify_rust_pid
fi
echo "Installed $runtime LaunchAgent: $AGENT"
