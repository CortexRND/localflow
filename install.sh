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
  LOCALFLOW_REPO_URL       Git repository URL or local path (git installs only)
  LOCALFLOW_REF            Git ref to check out, or tarball ref (default: main)

On macOS, Homebrew, git and Python are optional: without them the installer
uses a uv-managed Python 3.12 and a downloaded source tarball.
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
HAVE_BREW=0

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

# True when the Xcode Command Line Tools are installed. /usr/bin/git and
# /usr/bin/python3 are shims that pop up an install dialog without them.
mac_has_clt() {
    xcode-select -p >/dev/null 2>&1
}

# Succeeds when the executable at $1 is safe to invoke on macOS.
mac_exec_allowed() {
    if [[ "$1" == /usr/bin/* ]] && ! mac_has_clt; then
        return 1
    fi
    return 0
}

find_mac_python() {
    local candidate resolved
    for candidate in python3.14 python3.13 python3.12 python3.11 python3; do
        resolved="$(command -v "$candidate" 2>/dev/null)" || continue
        mac_exec_allowed "$resolved" || continue
        if "$resolved" -c \
            'import sys; raise SystemExit(sys.version_info < (3, 11))' \
            >/dev/null 2>&1; then
            PYTHON="$resolved"
            return 0
        fi
    done
    return 1
}

find_git() {
    local resolved
    resolved="$(command -v git 2>/dev/null)" || return 1
    if [[ "$OS" == "Darwin" ]] && ! mac_exec_allowed "$resolved"; then
        return 1
    fi
    return 0
}

ensure_uv() {
    export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
    if ! command -v uv >/dev/null 2>&1; then
        echo "Installing uv for this user."
        curl -fsSL https://astral.sh/uv/install.sh | sh
        export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
    fi
    if ! command -v uv >/dev/null 2>&1; then
        echo "uv installation failed or uv is not on PATH." >&2
        exit 1
    fi
    uv python install 3.12
    USE_UV=1
}

# Download the source tarball into $REPO, replacing older source but keeping .venv.
install_tarball() {
    local parent tmp url
    parent="$(dirname "$REPO")"
    mkdir -p "$parent"
    tmp="$(mktemp -d "$parent/.localflow-download.XXXXXX")"
    url="https://codeload.github.com/CortexRND/localflow/tar.gz/${LOCALFLOW_REF:-main}"
    echo "Downloading LocalFlow source (${LOCALFLOW_REF:-main})..."
    mkdir "$tmp/src"
    if ! curl -fsSL "$url" -o "$tmp/source.tar.gz" \
        || ! tar -xzf "$tmp/source.tar.gz" -C "$tmp/src" --strip-components=1 \
        || [[ ! -f "$tmp/src/pyproject.toml" ]]; then
        rm -rf "$tmp"
        echo "Could not download LocalFlow from $url" >&2
        exit 1
    fi
    : >"$tmp/src/.localflow-tarball"
    if [[ -e "$REPO" ]]; then
        if [[ -d "$REPO/.venv" ]]; then
            mv "$REPO/.venv" "$tmp/src/.venv"
        fi
        mv "$REPO" "$tmp/previous"
        if ! mv "$tmp/src" "$REPO"; then
            mv "$tmp/previous" "$REPO"
            if [[ -d "$tmp/src/.venv" ]]; then
                mv "$tmp/src/.venv" "$REPO/.venv"
            fi
            rm -rf "$tmp"
            echo "Could not replace $REPO" >&2
            exit 1
        fi
    else
        mv "$tmp/src" "$REPO"
    fi
    rm -rf "$tmp"
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
    for brew_dir in /opt/homebrew/bin /usr/local/bin; do
        if [[ -x "$brew_dir/brew" ]]; then
            case ":${PATH}:" in
                *":$brew_dir:"*) ;;
                *) PATH="$PATH:$brew_dir" ;;
            esac
        fi
    done
    if command -v brew >/dev/null 2>&1; then
        HAVE_BREW=1
    fi
    if [[ "$HAVE_BREW" -eq 0 ]] || ! find_mac_python; then
        echo "Using a uv-managed Python 3.12 (no Homebrew, or no Python 3.11+ found)."
        ensure_uv
    fi
    if ! command -v ffmpeg >/dev/null 2>&1; then
        if [[ "$HAVE_BREW" -eq 1 ]]; then
            echo "Installing ffmpeg..."
            brew install ffmpeg
        else
            echo "Note: ffmpeg was not installed (Homebrew not found)."
            echo "It is only needed to decode audio uploaded through the web page;"
            echo "dictation works without it. Install later with: brew install ffmpeg"
        fi
    fi
elif [[ "$OS" == "Linux" ]]; then
    if [[ "$NO_SYSTEM_DEPS" -eq 1 ]]; then
        echo "Skipping Linux system dependency installation."
    else
        install_linux_system_deps
    fi
    if ! find_linux_python; then
        echo "No Python 3.11+ with tkinter found; installing uv for this user."
        ensure_uv
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
    elif [[ -f "$REPO/.localflow-tarball" ]]; then
        install_tarball
    elif [[ -e "$REPO" ]]; then
        echo "Install destination exists but is not a LocalFlow install: $REPO" >&2
        exit 1
    elif find_git; then
        mkdir -p "$(dirname "$REPO")"
        git clone "${LOCALFLOW_REPO_URL:-https://github.com/CortexRND/localflow.git}" "$REPO"
    else
        echo "No usable git found; downloading a source tarball instead."
        install_tarball
    fi
fi

if [[ ! -e "$REPO/.git" ]]; then
    : # Tarball install: already at the requested ref.
elif ! find_git; then
    echo "Note: no usable git found; keeping the existing checkout at $REPO as is."
elif [[ -n "${LOCALFLOW_REF:-}" ]]; then
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
EXTRAS=""
if [[ "$OS" == "Darwin" ]]; then
    if [[ "$(uname -m)" == "arm64" ]]; then
        EXTRAS="[mlx,parakeet,menubar,macos-permissions]"
    else
        EXTRAS="[macos-permissions]"
    fi
fi
if [[ "$USE_UV" -eq 1 ]]; then
    uv venv --allow-existing --python 3.12 .venv
    if [[ "$OS" == "Linux" ]] \
        && ! .venv/bin/python -c 'import tkinter' >/dev/null 2>&1; then
        echo "The uv-managed Python does not include tkinter." >&2
        exit 1
    fi
    uv pip install --python .venv/bin/python --upgrade pip
    uv pip install --python .venv/bin/python -e ".${EXTRAS}"
else
    "$PYTHON" -m venv .venv
    if [[ "$OS" == "Linux" ]] \
        && ! .venv/bin/python -c 'import tkinter' >/dev/null 2>&1; then
        echo "Python 3.11+ with tkinter is required." >&2
        exit 1
    fi
    .venv/bin/python -m pip install --upgrade pip
    .venv/bin/pip install -e ".${EXTRAS}"
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

if [[ "$OS" == "Darwin" ]]; then
    # First run routes to the setup window; later runs open the Hub.
    echo "Opening LocalFlow..."
    exec open "$HOME/Applications/LocalFlow.app"
fi

echo "Opening LocalFlow Setup..."
exec .venv/bin/lf setup
