#!/bin/bash
set -euo pipefail

# The release a default install downloads and verifies. Keep synchronized
# with package metadata and the other release launchers.
LOCALFLOW_RELEASE="v0.2.1"
LOCALFLOW_VERSION="${LOCALFLOW_RELEASE#v}"
# Default installs trust only the release's source archive, checked against the
# release's SHA256SUMS. There is deliberately no pinned git commit: a release is
# tagged on the merge commit that contains this file, so that commit's hash
# cannot be written into this file in advance, and a fetched tag is never
# trusted to name the expected source.
# LOCALFLOW_RELEASE_BASE_URL is for testing only: it points the verified
# download at another copy of the release assets (e.g. a file:// directory).
RELEASE_BASE_URL="${LOCALFLOW_RELEASE_BASE_URL:-https://github.com/CortexRND/localflow/releases/download/$LOCALFLOW_RELEASE}"

usage() {
    cat <<EOF
Usage: install.sh [--with-ollama] [--no-launch] [--no-system-deps] [--uninstall] [--help]

Install LocalFlow on macOS or Linux. Piped installs use ~/.localflow/app.

Options:
  --with-ollama     Install and start Ollama
  --no-launch       Do not open the setup wizard after installation
  --no-system-deps  Skip Linux package-manager installs
  --uninstall      On macOS, remove either runtime's app bundles and LaunchAgent
  -h, --help        Show this help

Environment:
  LOCALFLOW_RUNTIME       rust (macOS default), python (Linux default; macOS fallback)
                           Rust and Xcode CLT are provisioned; CMake uses Homebrew if missing
  LOCALFLOW_WITH_OLLAMA=1  Same as --with-ollama
  LOCALFLOW_NO_LAUNCH=1    Same as --no-launch
  LOCALFLOW_HOME           Install location
  LOCALFLOW_REPO_URL       Git repository URL or local path (LOCALFLOW_REF git
                           installs only)
  LOCALFLOW_REF            Git ref to check out, or tarball ref, instead of the
                           pinned release $LOCALFLOW_RELEASE. A custom ref is NOT
                           checksum-verified.
  LOCALFLOW_RELEASE_BASE_URL
                           Testing only: alternative location of the release
                           assets (localflow-$LOCALFLOW_VERSION.tar.gz and SHA256SUMS)

By default, with or without git, the installer downloads
localflow-$LOCALFLOW_VERSION.tar.gz from the $LOCALFLOW_RELEASE GitHub release and
verifies it against the release's SHA256SUMS before unpacking. An existing
LocalFlow install at the destination, including an older git checkout, is
replaced only after verification succeeds; its .venv is kept. A replaced
directory that contains .git is never deleted: it is moved, with all local
changes, commits and stashes, to a sibling <name>.git-backup-<time>-<pid>
directory whose path is printed. Git commits are
not pinned: the release tag points at the merge commit that contains this
script, whose hash cannot be known when the script is written.
Running install.sh from inside a git checkout installs that checkout as is.

On macOS, the native Rust app needs no Python or virtual environment. Accept
the Xcode Command Line Tools dialog when prompted. For the Python app, run
LOCALFLOW_RUNTIME=python bash install.sh; it can provision Python using uv.
EOF
}

WITH_OLLAMA=0
NO_LAUNCH=0
NO_SYSTEM_DEPS=0
UNINSTALL=0
RUNTIME="${LOCALFLOW_RUNTIME-python}"
case "$RUNTIME" in
    python|rust) ;;
    *) echo "Invalid LOCALFLOW_RUNTIME: $RUNTIME (expected python or rust)." >&2; exit 2 ;;
esac
OS="$(uname -s)"
if [[ "${LOCALFLOW_RUNTIME+x}" != x && "$OS" == Darwin ]]; then RUNTIME=rust; fi
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
        --uninstall) UNINSTALL=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -n "${LOCALFLOW_REF:-}" ]]; then
    echo "Warning: LOCALFLOW_REF=$LOCALFLOW_REF is a custom ref; its source is not checksum-verified." >&2
fi

PYTHON=""
USE_UV=0
HAVE_BREW=0

if [[ "$UNINSTALL" == 1 ]]; then
    [[ "$OS" == Darwin ]] || { echo "--uninstall currently supports macOS only." >&2; exit 1; }
    rm -f "$HOME/Library/LaunchAgents/com.cortexrnd.localflow.plist"
    rm -rf "$HOME/Applications/LocalFlow.app" "$HOME/Applications/LocalFlow Setup.app"
    echo "Removed LocalFlow app bundles and autostart; settings, models and agent backups retained."
    if [[ "${LOCALFLOW_SKIP_LAUNCHCTL:-0}" != 1 ]]; then
        launchctl bootout "gui/$(id -u)/com.cortexrnd.localflow" >/dev/null 2>&1 || true
    fi
    exit 0
fi
if [[ "$RUNTIME" == rust && "$OS" != Darwin ]]; then
    echo "LOCALFLOW_RUNTIME=rust currently supports macOS only." >&2
    exit 1
fi

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

# Print the lowercase SHA-256 of file $1 (sha256sum, or shasum on macOS).
sha256_of() {
    local digest
    if command -v sha256sum >/dev/null 2>&1; then
        read -r digest _ < <(sha256sum "$1") || return 1
    elif command -v shasum >/dev/null 2>&1; then
        read -r digest _ < <(shasum -a 256 "$1") || return 1
    else
        echo "Neither sha256sum nor shasum is available to verify the download." >&2
        return 1
    fi
    printf '%s\n' "$digest" | tr '[:upper:]' '[:lower:]'
}

# Succeed only when file $1 matches the SHA-256 listed for name $3 in the
# sha256sum-format checksum file $2.
verify_sha256() {
    local file="$1" sums="$2" name="$3" hash entry expected="" actual
    while read -r hash entry || [[ -n "$hash" ]]; do
        entry="${entry%$'\r'}"
        entry="${entry#\*}"
        if [[ "$entry" == "$name" ]]; then
            expected="$(printf '%s\n' "$hash" | tr '[:upper:]' '[:lower:]')"
            break
        fi
    done <"$sums"
    if [[ ! "$expected" =~ ^[0-9a-f]{64}$ ]]; then
        echo "SHA256SUMS has no valid entry for $name." >&2
        return 1
    fi
    actual="$(sha256_of "$file")" || return 1
    if [[ "$actual" != "$expected" ]]; then
        echo "SHA-256 mismatch for $name: expected $expected, got $actual." >&2
        return 1
    fi
    echo "Verified SHA-256 of $name."
}

# Succeed when directory $1 is a git checkout of LocalFlow: it has .git and a
# pyproject.toml declaring name = "localflow". Only such a checkout may be
# replaced by the verified release archive.
is_localflow_checkout() {
    local line pattern="^[[:space:]]*name[[:space:]]*=[[:space:]]*[\"']localflow[\"'][[:space:]]*(#.*)?\$"
    [[ -e "$1/.git" && -f "$1/pyproject.toml" ]] || return 1
    while IFS= read -r line || [[ -n "$line" ]]; do
        line="${line%$'\r'}"
        if [[ "$line" =~ $pattern ]]; then
            return 0
        fi
    done <"$1/pyproject.toml"
    return 1
}

# Print an unused path next to directory $1 for preserving a replaced git
# checkout: <name>.git-backup-<timestamp>-<pid>[-<n>]. Needs neither git nor
# any tool beyond date (optional).
git_backup_path() {
    local trimmed="$1" parent base stamp candidate n=1
    while [[ "$trimmed" == */ && "$trimmed" != / ]]; do trimmed="${trimmed%/}"; done
    parent="$(dirname "$trimmed")"
    base="${trimmed##*/}"
    stamp="$(date +%Y%m%d-%H%M%S 2>/dev/null)" || stamp=""
    candidate="$parent/$base.git-backup-${stamp:+$stamp-}$$"
    while [[ -e "$candidate" || -L "$candidate" ]]; do
        n=$((n + 1))
        candidate="$parent/$base.git-backup-${stamp:+$stamp-}$$-$n"
    done
    printf '%s\n' "$candidate"
}

# Download the source tarball into $REPO, replacing older source but keeping .venv.
# Without LOCALFLOW_REF this is the pinned release asset, verified against the
# release's SHA256SUMS; a custom LOCALFLOW_REF uses GitHub's unverified codeload.
# A replaced directory containing .git (a checkout, worktree or submodule) is
# never deleted: it is renamed, intact, to a sibling backup whose path is
# printed. Everything is downloaded and verified before $REPO is touched, and
# a failed replacement restores the previous directory and its .venv.
install_tarball() {
    local parent tmp url asset="" previous backup="" venv_moved=0
    parent="$(dirname "$REPO")"
    mkdir -p "$parent"
    tmp="$(mktemp -d "$parent/.localflow-download.XXXXXX")"
    if [[ -n "${LOCALFLOW_REF:-}" ]]; then
        url="https://codeload.github.com/CortexRND/localflow/tar.gz/$LOCALFLOW_REF"
        echo "Downloading LocalFlow source ($LOCALFLOW_REF, not checksum-verified)..."
    else
        asset="localflow-$LOCALFLOW_VERSION.tar.gz"
        url="$RELEASE_BASE_URL/$asset"
        echo "Downloading LocalFlow $LOCALFLOW_RELEASE..."
    fi
    mkdir "$tmp/src"
    if ! curl -fsSL "$url" -o "$tmp/source.tar.gz"; then
        rm -rf "$tmp"
        echo "Could not download LocalFlow from $url" >&2
        exit 1
    fi
    if [[ -n "$asset" ]]; then
        if ! curl -fsSL "$RELEASE_BASE_URL/SHA256SUMS" -o "$tmp/SHA256SUMS"; then
            rm -rf "$tmp"
            echo "Could not download LocalFlow from $RELEASE_BASE_URL/SHA256SUMS" >&2
            exit 1
        fi
        if ! verify_sha256 "$tmp/source.tar.gz" "$tmp/SHA256SUMS" "$asset"; then
            rm -rf "$tmp"
            echo "Refusing to install: $url failed checksum verification; the download was removed." >&2
            exit 1
        fi
    fi
    if ! tar -xzf "$tmp/source.tar.gz" -C "$tmp/src" --strip-components=1 \
        || [[ ! -f "$tmp/src/pyproject.toml" ]]; then
        rm -rf "$tmp"
        echo "Could not download LocalFlow from $url" >&2
        exit 1
    fi
    : >"$tmp/src/.localflow-tarball"
    if [[ -e "$REPO" ]]; then
        previous="$tmp/previous"
        if [[ -e "$REPO/.git" || -L "$REPO/.git" ]]; then
            # Uncommitted, staged and untracked files, unpushed commits,
            # stashes and git metadata all live here: keep the whole tree.
            backup="$(git_backup_path "$REPO")"
            previous="$backup"
            if [[ -d "$REPO/.venv" ]]; then
                echo "Backing up the existing git checkout at $REPO to $backup (its .venv is reused by the new install)."
            else
                echo "Backing up the existing git checkout at $REPO to $backup."
            fi
        fi
        if [[ -d "$REPO/.venv" ]]; then
            if ! mv "$REPO/.venv" "$tmp/src/.venv"; then
                rm -rf "$tmp"
                echo "Could not move $REPO/.venv; $REPO was left unchanged." >&2
                exit 1
            fi
            venv_moved=1
        fi
        if ! mv "$REPO" "$previous"; then
            if [[ "$venv_moved" == 1 && ! -e "$REPO/.venv" ]]; then
                mv "$tmp/src/.venv" "$REPO/.venv" || {
                    echo "Could not restore $REPO/.venv; it remains at $tmp/src/.venv" >&2
                    exit 1
                }
            fi
            rm -rf "$tmp"
            echo "Could not move $REPO aside${backup:+ to $backup}; $REPO was left unchanged." >&2
            exit 1
        fi
        if ! mv "$tmp/src" "$REPO"; then
            if ! mv "$previous" "$REPO"; then
                # Keep $tmp: it may hold the previous install or its .venv.
                echo "Could not replace $REPO or restore it; the previous install is at $previous" >&2
                if [[ "$venv_moved" == 1 ]]; then
                    echo "Its .venv is at $tmp/src/.venv" >&2
                fi
                exit 1
            fi
            if [[ "$venv_moved" == 1 ]] && ! mv "$tmp/src/.venv" "$REPO/.venv"; then
                echo "Restored $REPO but not its .venv; it remains at $tmp/src/.venv" >&2
                exit 1
            fi
            rm -rf "$tmp"
            echo "Could not replace $REPO; the previous install was restored." >&2
            exit 1
        fi
        if [[ -n "$backup" ]]; then
            echo "Previous git checkout preserved at: $backup"
            if [[ -f "$backup/.git" ]]; then
                echo "Its .git is a gitdir link; run 'git -C \"$backup\" worktree repair' if git reports it as missing."
            fi
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

if [[ "$OS" == Darwin ]]; then
    for brew_dir in /opt/homebrew/bin /usr/local/bin; do
        if [[ -x "$brew_dir/brew" ]]; then export PATH="$PATH:$brew_dir"; fi
    done
fi
if [[ "$OS" == "Darwin" && "$RUNTIME" == rust ]]; then
    if ! mac_has_clt; then
        echo "Opening Apple's Xcode Command Line Tools installer. Click Install and accept the license; waiting up to 30 minutes..."
        if ! xcode-select --install; then
            echo "Could not start Xcode Command Line Tools installation. Run xcode-select --install, complete installation, then rerun LocalFlow." >&2
            exit 1
        fi
        for ((attempt = 0; attempt < 180; attempt++)); do
            if mac_has_clt; then break; fi
            sleep 10
        done
        mac_has_clt || { echo "Timed out waiting for Xcode Command Line Tools. Complete the Apple installer, then rerun LocalFlow." >&2; exit 1; }
    fi
    export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
    if ! command -v cargo >/dev/null 2>&1 || ! cargo --version >/dev/null 2>&1; then
        echo "Installing Rust (stable, minimal profile) for this user..."
        rustup_installer="$(mktemp)"
        if ! curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$rustup_installer" \
            || ! sh "$rustup_installer" -y --profile minimal --default-toolchain stable; then
            rm -f "$rustup_installer"
            echo "Rust installation failed. Check your internet connection and rerun LocalFlow (https://rustup.rs)." >&2
            exit 1
        fi
        rm -f "$rustup_installer"
        if ! command -v cargo >/dev/null 2>&1 || ! cargo --version >/dev/null 2>&1; then
            echo "Rust installation completed but Cargo is unavailable; check CARGO_HOME and rerun LocalFlow." >&2
            exit 1
        fi
    fi
    if ! command -v cmake >/dev/null 2>&1; then
        if command -v brew >/dev/null 2>&1; then
            echo "Installing CMake..."
            brew install cmake || { echo "CMake installation failed. Check Homebrew and rerun LocalFlow." >&2; exit 1; }
        else
            echo "LOCALFLOW_RUNTIME=rust requires CMake; install cmake (https://cmake.org/download/) or install Homebrew and rerun." >&2
            exit 1
        fi
    fi
elif [[ "$OS" == "Darwin" ]]; then
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
FROM_CHECKOUT=0
if [[ -n "$SCRIPT_SOURCE" && -f "$SCRIPT_SOURCE" ]]; then
    SCRIPT_DIR="$(cd -- "$(dirname -- "$SCRIPT_SOURCE")" && pwd)"
fi
if [[ -n "$SCRIPT_DIR" && -f "$SCRIPT_DIR/pyproject.toml" \
    && -e "$SCRIPT_DIR/.git" ]]; then
    REPO="$SCRIPT_DIR"
    FROM_CHECKOUT=1
else
    REPO="${LOCALFLOW_HOME:-$HOME/.localflow/app}"
    if [[ -n "${LOCALFLOW_REF:-}" ]]; then
        # Custom ref (unverified): use git when available, else codeload.
        if [[ -e "$REPO/.git" ]]; then
            : # Existing git install; checked out at LOCALFLOW_REF below.
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
    elif [[ -f "$REPO/.localflow-tarball" ]]; then
        install_tarball
    elif is_localflow_checkout "$REPO"; then
        # Default installs never trust git: an older managed git checkout is
        # replaced by the verified release archive (keeping .venv), and only
        # after the download passes its SHA256SUMS check. The checkout itself
        # is moved to a sibling backup, never deleted.
        echo "Existing git checkout at $REPO will be replaced by the $LOCALFLOW_RELEASE release archive once it passes SHA256SUMS verification (.venv is kept; the checkout is backed up next to it)."
        install_tarball
    elif [[ -e "$REPO" ]]; then
        echo "Install destination exists but is not a LocalFlow install: $REPO" >&2
        exit 1
    else
        install_tarball
    fi
fi

if [[ ! -e "$REPO/.git" ]]; then
    : # Archive install: already at the requested (and, by default, verified) release.
elif [[ -z "${LOCALFLOW_REF:-}" && "$FROM_CHECKOUT" -ne 1 ]]; then
    # Unreachable by design: default managed installs are archive installs.
    echo "Unexpected git checkout at $REPO; refusing to install unverified source." >&2
    exit 1
elif ! find_git; then
    echo "Note: no usable git found; keeping the existing checkout at $REPO as is."
elif [[ -n "${LOCALFLOW_REF:-}" ]]; then
    git -C "$REPO" fetch origin
    if ! git -C "$REPO" rev-parse --verify --quiet \
        "${LOCALFLOW_REF}^{commit}" >/dev/null; then
        git -C "$REPO" fetch origin "$LOCALFLOW_REF"
    fi
    git -C "$REPO" checkout "$LOCALFLOW_REF"
fi

cd "$REPO"
if [[ "$RUNTIME" == python ]]; then
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
fi

if [[ "$WITH_OLLAMA" -eq 1 ]]; then
    if [[ "$OS" == "Darwin" ]]; then
        if ! command -v ollama >/dev/null 2>&1; then
            command -v brew >/dev/null 2>&1 || { echo "Installing Ollama requires Homebrew. Install Ollama from https://ollama.com/download/mac or rerun without --with-ollama." >&2; exit 1; }
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

if [[ "$RUNTIME" == rust ]]; then
    bash scripts/build_rust_app.sh "$HOME/Applications/LocalFlow.app"
    bash "$HOME/Applications/LocalFlow.app/Contents/Resources/macos_launchagent.sh" \
        restart-rust "$HOME/Applications/LocalFlow.app/Contents/MacOS/lf-hub"
elif [[ "$OS" == "Linux" ]]; then
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
    if [[ -f scripts/macos_launchagent.sh ]]; then
        bash scripts/macos_launchagent.sh restore-python "$REPO/.venv/bin/localflow"
    fi
    app="$HOME/Applications/LocalFlow.app"
    if [[ -f "$app/Contents/Info.plist" ]] \
        && [[ "$(plutil -extract LocalFlowRuntime raw -o - "$app/Contents/Info.plist" 2>/dev/null || true)" == rust ]]; then
        rm -rf "$app"
    fi
    .venv/bin/lf setup install-app
fi

if [[ "$NO_LAUNCH" -eq 1 ]]; then
    echo "Skipping setup launch."
    exit 0
fi

if [[ "$OS" == "Darwin" ]]; then
    # First run routes to the setup window; later runs open the Hub. The
    # install already succeeded (build and setup failures exited above), so a
    # failed open -- e.g. LaunchServices error -600 while the login agent's copy
    # is still starting -- is retried briefly, then only reported.
    echo "Opening LocalFlow..."
    open_error=""
    for ((attempt = 1; attempt <= 5; attempt++)); do
        if open_error="$(open "$HOME/Applications/LocalFlow.app" 2>&1)"; then
            exit 0
        fi
        if ((attempt < 5)); then sleep 2; fi
    done
    echo "Warning: LocalFlow is installed but could not be opened automatically${open_error:+ ($open_error)}." >&2
    echo "Open it from Applications → LocalFlow (in your home folder)." >&2
    exit 0
fi

echo "Opening LocalFlow Setup..."
exec .venv/bin/lf setup
