#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: install.sh [--with-ollama] [--no-launch] [--no-system-deps] [--help]

Install LocalFlow on macOS or Linux. Piped installs use ~/.localflow/app.

Options:
  --with-ollama     Install and start Ollama
  --no-launch       Do not open the setup wizard after installation
  --no-system-deps  Skip Linux package-manager installs
  -h, --help        Show this help

Environment:
  LOCALFLOW_WITH_OLLAMA=1  Same as --with-ollama
  LOCALFLOW_NO_LAUNCH=1    Same as --no-launch
  LOCALFLOW_HOME           Install checkout location
  LOCALFLOW_REPO_URL       Git repository URL or local path
  LOCALFLOW_REF            Git ref to check out after cloning or pulling
EOF
}

WITH_OLLAMA=0
NO_LAUNCH=0
NO_SYSTEM_DEPS=0
if [[ "${LOCALFLOW_WITH_OLLAMA:-0}" == "1" ]]; then
    WITH_OLLAMA=1
fi
if [[ "${LOCALFLOW_NO_LAUNCH:-0}" == "1" ]]; then
    NO_LAUNCH=1
fi

for arg in "$@"; do
    case "$arg" in
        --with-ollama) WITH_OLLAMA=1 ;;
        --no-launch) NO_LAUNCH=1 ;;
        --no-system-deps) NO_SYSTEM_DEPS=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

OS="$(uname -s)"
PYTHON=""
USE_UV=0

install_linux_system_deps() {
    local -a wayland_packages=()
    local -a packages=()
    if [[ -n "${WAYLAND_DISPLAY:-}" ]]; then
        wayland_packages=(wtype)
    fi

    if command -v apt-get >/dev/null 2>&1; then
        packages=(git python3 python3-venv python3-tk libportaudio2 ffmpeg
            xdotool libnotify-bin pulseaudio-utils "${wayland_packages[@]}")
        echo "Running: sudo apt-get update"
        sudo apt-get update
        printf 'Running: sudo apt-get install -y %s\n' "${packages[*]}"
        sudo apt-get install -y "${packages[@]}"
    elif command -v dnf >/dev/null 2>&1; then
        packages=(git python3 python3-tkinter portaudio ffmpeg-free xdotool
            libnotify pulseaudio-utils "${wayland_packages[@]}")
        printf 'Running: sudo dnf install -y %s\n' "${packages[*]}"
        sudo dnf install -y "${packages[@]}"
    elif command -v pacman >/dev/null 2>&1; then
        packages=(git python tk portaudio ffmpeg xdotool libnotify libpulse
            "${wayland_packages[@]}")
        printf 'Running: sudo pacman --needed --noconfirm -S %s\n' "${packages[*]}"
        sudo pacman --needed --noconfirm -S "${packages[@]}"
    elif command -v zypper >/dev/null 2>&1; then
        packages=(git python3 python3-tk portaudio ffmpeg xdotool
            libnotify-tools pulseaudio-utils "${wayland_packages[@]}")
        printf 'Running: sudo zypper install -y %s\n' "${packages[*]}"
        sudo zypper install -y "${packages[@]}"
    else
        echo "Unknown package manager. Install git, Python 3.11+ with tkinter and venv support,"
        echo "PortAudio, ffmpeg, xdotool, libnotify, and PulseAudio utilities."
        if [[ -n "${WAYLAND_DISPLAY:-}" ]]; then
            echo "Wayland detected; also install wtype."
        fi
    fi
}

find_linux_python() {
    local candidate
    for candidate in python3.13 python3.12 python3.11 python3; do
        if command -v "$candidate" >/dev/null 2>&1 \
            && "$candidate" -c \
                'import sys, tkinter; raise SystemExit(sys.version_info < (3, 11))' \
                >/dev/null 2>&1; then
            PYTHON="$candidate"
            return 0
        fi
    done
    return 1
}

wait_for_ollama() {
    local attempt
    for ((attempt = 1; attempt <= 30; attempt++)); do
        if curl --silent --fail http://127.0.0.1:11434/api/tags >/dev/null; then
            return 0
        fi
        sleep 1
    done
    echo "Warning: Ollama did not respond within 30 seconds; LocalFlow setup will show it as unavailable." >&2
    return 0
}

if [[ "$OS" == "Darwin" ]]; then
    if ! command -v brew >/dev/null 2>&1; then
        echo "Homebrew is required. Install it from https://brew.sh, then rerun this script." >&2
        exit 1
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        echo "Python 3.11+ is required. Install it with: brew install python@3.12" >&2
        exit 1
    fi
    if ! python3 -c \
        'import sys; raise SystemExit(sys.version_info < (3, 11))'; then
        echo "Python 3.11+ is required; install it with: brew install python@3.12" >&2
        exit 1
    fi
    PYTHON=python3
    if ! command -v ffmpeg >/dev/null 2>&1; then
        echo "Installing ffmpeg..."
        brew install ffmpeg
    fi
elif [[ "$OS" == "Linux" ]]; then
    if [[ "$NO_SYSTEM_DEPS" -eq 1 ]]; then
        echo "Skipping Linux system dependency installation."
    else
        install_linux_system_deps
    fi
    if ! find_linux_python; then
        echo "No Python 3.11+ with tkinter found; installing uv for this user."
        if ! command -v uv >/dev/null 2>&1; then
            curl -fsSL https://astral.sh/uv/install.sh | sh
            export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
        fi
        if ! command -v uv >/dev/null 2>&1; then
            echo "uv installation failed or uv is not on PATH." >&2
            exit 1
        fi
        uv python install 3.12
        USE_UV=1
    fi
else
    echo "LocalFlow's installer supports macOS and Linux." >&2
    exit 1
fi

SCRIPT_SOURCE="${BASH_SOURCE[0]:-}"
SCRIPT_DIR=""
EXISTING_INSTALL=0
if [[ -n "$SCRIPT_SOURCE" && -f "$SCRIPT_SOURCE" ]]; then
    SCRIPT_DIR="$(cd -- "$(dirname -- "$SCRIPT_SOURCE")" && pwd)"
fi
if [[ -n "$SCRIPT_DIR" && -f "$SCRIPT_DIR/pyproject.toml" \
    && -e "$SCRIPT_DIR/.git" ]]; then
    REPO="$SCRIPT_DIR"
else
    REPO="${LOCALFLOW_HOME:-$HOME/.localflow/app}"
    if [[ -e "$REPO/.git" ]]; then
        EXISTING_INSTALL=1
    else
        if [[ -e "$REPO" ]]; then
            echo "Install destination exists but is not a git checkout: $REPO" >&2
            exit 1
        fi
        mkdir -p "$(dirname "$REPO")"
        git clone "${LOCALFLOW_REPO_URL:-https://github.com/CortexRND/localflow.git}" "$REPO"
    fi
fi

if [[ -n "${LOCALFLOW_REF:-}" ]]; then
    git -C "$REPO" fetch origin
    if ! git -C "$REPO" rev-parse --verify --quiet \
        "${LOCALFLOW_REF}^{commit}" >/dev/null; then
        git -C "$REPO" fetch origin "$LOCALFLOW_REF"
    fi
    git -C "$REPO" checkout "$LOCALFLOW_REF"
elif [[ "$EXISTING_INSTALL" -eq 1 ]]; then
    git -C "$REPO" pull --ff-only
fi

cd "$REPO"
if [[ "$USE_UV" -eq 1 ]]; then
    uv venv --python 3.12 .venv
    if [[ "$OS" == "Linux" ]] \
        && ! .venv/bin/python -c 'import tkinter' >/dev/null 2>&1; then
        echo "The uv-managed Python does not include tkinter." >&2
        exit 1
    fi
    uv pip install --python .venv/bin/python --upgrade pip
    uv pip install --python .venv/bin/python -e .
else
    "$PYTHON" -m venv .venv
    if [[ "$OS" == "Linux" ]] \
        && ! .venv/bin/python -c 'import tkinter' >/dev/null 2>&1; then
        echo "Python 3.11+ with tkinter is required." >&2
        exit 1
    fi
    .venv/bin/python -m pip install --upgrade pip
    if [[ "$OS" == "Darwin" ]]; then
        if [[ "$(uname -m)" == "arm64" ]]; then
            .venv/bin/pip install -e ".[mlx,parakeet,menubar,macos-permissions]"
        else
            .venv/bin/pip install -e ".[macos-permissions]"
        fi
    else
        .venv/bin/pip install -e .
    fi
fi

if [[ "$WITH_OLLAMA" -eq 1 ]]; then
    if [[ "$OS" == "Darwin" ]]; then
        if ! command -v ollama >/dev/null 2>&1; then
            brew install ollama
        fi
        if ! pgrep -x ollama >/dev/null 2>&1; then
            mkdir -p "$HOME/Library/Logs"
            nohup ollama serve >"$HOME/Library/Logs/localflow-ollama.log" 2>&1 </dev/null &
        fi
    else
        if ! command -v ollama >/dev/null 2>&1; then
            curl -fsSL https://ollama.com/install.sh | sh
        fi
        if ! pgrep -x ollama >/dev/null 2>&1; then
            mkdir -p "$HOME/.local/state/localflow"
            nohup ollama serve >"$HOME/.local/state/localflow/ollama.log" \
                2>&1 </dev/null &
        fi
    fi
    wait_for_ollama
fi

if [[ "$OS" == "Linux" ]]; then
    mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"
    ln -sfn "$REPO/.venv/bin/lf" "$HOME/.local/bin/lf"
    case ":${PATH}:" in
        *":$HOME/.local/bin:"*) ;;
        *) echo "Warning: add $HOME/.local/bin to PATH to use the lf command." >&2 ;;
    esac

    desktop_exec() {
        local executable="$1"
        executable="${executable//\\/\\\\}"
        executable="${executable//\"/\\\"}"
        printf '"%s"' "$executable"
    }
    {
        printf '[Desktop Entry]\nType=Application\nName=LocalFlow\n'
        printf 'Exec=%s\nTerminal=false\nCategories=Utility;Accessibility;\n' \
            "$(desktop_exec "$REPO/.venv/bin/localflow")"
    } >"$HOME/.local/share/applications/localflow.desktop"
    {
        printf '[Desktop Entry]\nType=Application\nName=LocalFlow Setup\n'
        printf 'Exec=%s setup\nTerminal=false\nCategories=Utility;Accessibility;\n' \
            "$(desktop_exec "$REPO/.venv/bin/lf")"
    } >"$HOME/.local/share/applications/localflow-setup.desktop"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$HOME/.local/share/applications"
    fi
elif [[ "$OS" == "Darwin" ]]; then
    .venv/bin/lf setup install-app
fi

if [[ "$NO_LAUNCH" -eq 1 ]]; then
    echo "Skipping setup launch."
    exit 0
fi

echo "Opening LocalFlow Setup..."
exec .venv/bin/lf setup
