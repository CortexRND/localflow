#!/usr/bin/env bash
set -euo pipefail
output=$1
shift
mkdir -p "$output"
rm -f "$output/pages.csv" "$output/pages.done"
LF_HUB_RSS_PROFILE="$output/pages.csv" "$@" >"$output/hub.log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true' EXIT
for ((tick=0; tick<600; tick++)); do
  if ! kill -0 "$pid" 2>/dev/null; then cat "$output/hub.log"; exit 1; fi
  if [[ -f "$output/pages.done" ]]; then break; fi
  sleep 1
done
[[ -f "$output/pages.done" ]] || { cat "$output/hub.log"; echo 'Page profiling timed out'; exit 1; }
curl --noproxy '*' --fail --silent --show-error --max-time 3 http://127.0.0.1:8756/healthz > "$output/healthz.json"
lsof -nP -a -p "$pid" -iTCP:8756 -sTCP:LISTEN > "$output/socket.txt"
python3 - "$output" "$pid" <<'PY'
import csv, json, os, subprocess, sys
from pathlib import Path
from urllib.request import Request, urlopen
root = Path(sys.argv[1])
processes = [line.split()[0] for line in subprocess.check_output(['ps', '-axo', 'pid=,comm='], text=True).splitlines() if Path(line.split(maxsplit=1)[1]).name == 'lf-hub']
assert processes == [sys.argv[2]], processes
token_root = Path(os.environ.get('XDG_CONFIG_HOME', str(Path.home() / 'Library/Application Support')))
token = (token_root / 'localflow/api-token').read_text().strip()
with urlopen(Request('http://127.0.0.1:8756/api/status', headers={'X-Localflow-Token': token}), timeout=3) as response:
    assert json.load(response)['stt']['loaded'] is False
assert json.loads((root / 'healthz.json').read_text())['ok'] is True
rows = list(csv.reader((root / 'pages.csv').open()))
assert len(rows) == 21, rows
text = '| State | RSS (KiB) |\n| --- | ---: |\n' + ''.join(f'| {name} | {rss} |\n' for name, rss in rows)
(root / 'pages.md').write_text(text)
print(text)
print('All pages sampled; one Hub process; server owned by Hub; health OK; speech unloaded')
PY
