#!/bin/bash
set -e

echo "localflow setup"
echo "==============="

# Check for Homebrew
if ! command -v brew &> /dev/null; then
    echo "Error: Homebrew not found. Install it with:"
    printf "  %s\n" "/bin/bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\""
    exit 1
fi

# Check for ffmpeg; install if missing
if ! command -v ffmpeg &> /dev/null; then
    echo "Installing ffmpeg..."
    brew install ffmpeg
else
    echo "✓ ffmpeg found"
fi

# Create venv
echo "Creating virtual environment..."
python3 -m venv .venv

# Install package
echo "Installing localflow..."
.venv/bin/pip install -e ".[mlx,menubar,macos-permissions]"

# The hot-phrases window needs tkinter (Homebrew Python doesn't ship it).
if ! .venv/bin/python -c "import tkinter" 2>/dev/null; then
    ver=$(.venv/bin/python -c "import sys; print(f'{sys.version_info[0]}.{sys.version_info[1]}')")
    echo ""
    echo "⚠ tkinter is not available — the hot phrases window (lf phrases) won't run."
    echo "  Install it with: brew install python-tk@${ver}"
fi

echo ""
echo "✓ Setup complete — opening the LocalFlow setup wizard."
.venv/bin/lf setup
