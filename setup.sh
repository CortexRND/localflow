#!/bin/bash
set -e

echo "localflow setup"
echo "==============="

# Check for Homebrew
if ! command -v brew &> /dev/null; then
    echo "Error: Homebrew not found. Install it with:"
    echo '  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"'
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
source .venv/bin/activate

# Install package
echo "Installing localflow..."
pip install -e ".[mlx,menubar]"

echo ""
echo "✓ Setup complete!"
echo ""
echo "Next steps:"
echo "1. Grant microphone + accessibility permissions (System Settings → Privacy & Security)"
echo "2. Start recording:  .venv/bin/localflow"
echo "3. Or start server:  .venv/bin/localflow-server"
echo ""
echo "Optional:"
echo "  To enable transcript cleanup (Ollama), install and pull:"
echo "    brew install ollama"
echo "    ollama pull llama3.2:3b"
echo "  Then set cleanup_enabled = true in ~/.localflow.toml"
echo ""
