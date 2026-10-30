#!/bin/bash
set -euo pipefail

usage() {
    echo "Usage: $0 [--with-ollama]"
    echo "Install LocalFlow and open its setup wizard (macOS only)."
}

WITH_OLLAMA=0
for arg in "$@"; do
    case "$arg" in
        --with-ollama) WITH_OLLAMA=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "LocalFlow's launcher installer currently supports macOS only." >&2
    exit 1
fi
if ! command -v brew >/dev/null 2>&1; then
    echo "Homebrew is required. Install it from https://brew.sh, then rerun this script." >&2
    exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "Python 3.11+ is required. Install it with: brew install python@3.12" >&2
    exit 1
fi
if ! python3 -c 'import sys; raise SystemExit(sys.version_info < (3, 11))'; then
    echo "Python 3.11+ is required; install it with: brew install python@3.12" >&2
    exit 1
fi
if ! command -v ffmpeg >/dev/null 2>&1; then
    echo "Installing ffmpeg..."
    brew install ffmpeg
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -f "$SCRIPT_DIR/pyproject.toml" && -d "$SCRIPT_DIR/.git" ]]; then
    REPO="$SCRIPT_DIR"
else
    REPO="${LOCALFLOW_HOME:-$HOME/.localflow/app}"
    if [[ ! -d "$REPO/.git" ]]; then
        if [[ -e "$REPO" ]]; then
            echo "Install destination exists but is not a git checkout: $REPO" >&2
            exit 1
        fi
        mkdir -p "$(dirname "$REPO")"
        git clone "${LOCALFLOW_REPO_URL:-https://github.com/CortexRND/localflow.git}" "$REPO"
    else
        git -C "$REPO" pull --ff-only
    fi
fi

cd "$REPO"
python3 -m venv .venv
.venv/bin/python -m pip install --upgrade pip
if [[ "$(uname -m)" == "arm64" ]]; then
    .venv/bin/pip install -e ".[mlx,menubar]"
else
    .venv/bin/pip install -e .
fi

if [[ "$WITH_OLLAMA" -eq 1 ]]; then
    if ! command -v ollama >/dev/null 2>&1; then
        brew install ollama
    fi
    if ! pgrep -x ollama >/dev/null 2>&1; then
        mkdir -p "$HOME/Library/Logs"
        nohup ollama serve >"$HOME/Library/Logs/localflow-ollama.log" 2>&1 </dev/null &
    fi
    for _ in $(seq 1 30); do
        if curl --silent --fail http://127.0.0.1:11434/api/tags >/dev/null; then
            break
        fi
        sleep 1
    done
fi

.venv/bin/lf setup install-app
echo "Opening LocalFlow Setup..."
exec .venv/bin/lf setup
