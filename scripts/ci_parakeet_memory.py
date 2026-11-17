"""Run each real native configuration in an isolated process on macOS."""
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from urllib.request import urlopen

root, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
output.mkdir(parents=True, exist_ok=True)
binary = root / 'rust/target/release/examples/parakeet_memory'
shim = output / 'ort-memory-options.dylib'
shim_error = None
try:
    library = next((root / 'rust/target/release').glob('libonnxruntime*.dylib'))
    class Base(ctypes.Structure):
        _fields_ = [('api', ctypes.c_void_p), ('version', ctypes.CFUNCTYPE(ctypes.c_char_p))]
    native = ctypes.CDLL(str(library))
    native.OrtGetApiBase.restype = ctypes.POINTER(Base)
    version = native.OrtGetApiBase().contents.version().decode()
    assert re.fullmatch(r'\d+\.\d+\.\d+', version), version
    headers = output / 'include'
    headers.mkdir(exist_ok=True)
    pending = ['onnxruntime_c_api.h']
    seen = set()
    while pending:
        name = pending.pop()
        if name in seen:
            continue
        assert re.fullmatch(r'onnxruntime_[a-z0-9_]+\.h', name), name
        seen.add(name)
        with urlopen(f'https://raw.githubusercontent.com/microsoft/onnxruntime/v{version}/include/onnxruntime/core/session/{name}', timeout=30) as response:
            data = response.read()
        (headers / name).write_bytes(data)
        pending.extend(re.findall(r'^#include "([^"]+)"', data.decode(), re.M))
    result = subprocess.run(['cc', '-dynamiclib', '-O2', '-Wall', '-Werror', '-I', str(headers),
                             '-undefined', 'dynamic_lookup', '-o', str(shim),
                             str(root / 'scripts/ort_memory_options.c')], capture_output=True, text=True, check=True)
    (output / 'shim-build.txt').write_text(f'Exact native ORT headers: {version}\n' + result.stdout + result.stderr)
except Exception as error:
    shim_error = f'{error}\n{getattr(error, "stderr", "")}'
    (output / 'shim-error.txt').write_text(shim_error)

cases = [
    ('int8-default', [], {}),
    ('int8-coreml', ['--provider', 'coreml'], {}),
    ('int8-threads1', ['--threads', '1'], {}),
    ('int8-threads2', ['--threads', '2'], {}),
    ('int8-intra1-inter1', [], {'LF_ORT_INTRA': '1', 'LF_ORT_INTER': '1'}),
    ('int8-arena-off', [], {'LF_ORT_ARENA': '1'}),
    ('int8-pattern-off', [], {'LF_ORT_PATTERN': '1'}),
    ('int8-graph-basic', [], {'LF_ORT_GRAPH': '1'}),
    ('int8-graph-off', [], {'LF_ORT_GRAPH': '0'}),
    ('int8-memory', ['--threads', '1'], {'LF_ORT_ARENA': '1', 'LF_ORT_PATTERN': '1', 'LF_ORT_GRAPH': '1', 'LF_ORT_INTER': '1'}),
    ('fp32-default', ['--fp32'], {}),
    ('fp32-memory', ['--fp32', '--threads', '1'], {'LF_ORT_ARENA': '1', 'LF_ORT_PATTERN': '1', 'LF_ORT_GRAPH': '1', 'LF_ORT_INTER': '1'}),
]
results = []
for name, args, tuning in cases:
    entry = {'name': name, 'options': tuning}
    try:
        if tuning and shim_error:
            raise RuntimeError('Session-options shim unavailable; see shim-error.txt')
        env = os.environ.copy()
        if tuning:
            env.update(tuning, DYLD_INSERT_LIBRARIES=str(shim))
        destination = output / f'{name}.json'
        result = subprocess.run([str(binary), '--output', str(destination), *args], env=env,
                                capture_output=True, text=True, timeout=900)
        (output / f'{name}.log').write_text(result.stdout + result.stderr)
        if result.returncode:
            if tuning:
                shim_error = f'Shim dropped after first failed repaired attempt: exit {result.returncode}: {result.stderr[-1500:]}'
                (output / 'shim-dropped.txt').write_text(shim_error)
            raise RuntimeError(f'exit {result.returncode}: ' + result.stderr[-1500:])
        if tuning and result.stderr.count('ORT_MEMORY_OPTIONS applied:') < 3:
            shim_error = 'Shim dropped: options were NOT verified on all three ONNX sessions'
            (output / 'shim-dropped.txt').write_text(shim_error)
            raise RuntimeError('Options were NOT verified on all three ONNX sessions')
        entry.update(json.loads(destination.read_text()))
        entry['clip_sha256'] = hashlib.sha256(Path(entry['clip']).read_bytes()).hexdigest()
    except Exception as error:
        entry['error'] = str(error)
    results.append(entry)
    print(json.dumps(entry), flush=True)

baseline = next((r.get('transcript') for r in results if r['name'] == 'int8-default'), None)
hashes = {row['clip_sha256'] for row in results if 'clip_sha256' in row}
assert len(hashes) <= 1, 'The clip changed during measurement'
lines = ['All memory columns are MiB. Footprint is the kernel phys_footprint ledger (proc_pid_rusage), not shared clean RSS.', '',
         '| Configuration | Provider | Load RSS | Load footprint | Transcribed RSS | Transcribed footprint | Load ms | Inference ms | Exact |',
         '| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |']
for row in results:
    if 'error' in row:
        lines.append(f'| {row["name"]} | ERROR (see log/JSON) | — | — | — | — | — | — | — |')
        continue
    row['exact_match'] = row['transcript'] == baseline if baseline is not None else None
    lines.append(f'| {row["name"]} | {row["provider"]} | {row["rss_after_load"]/2**20:.1f} | {row["footprint_load"]/2**20:.1f} | {row["rss_after_transcribe"]/2**20:.1f} | {row["footprint_transcribe"]/2**20:.1f} | {row["load_ms"]:.1f} | {row["latency_ms"]:.1f} | {row["exact_match"]} |')
lines += ['', '| Configuration | Unload RSS | Unload footprint | Drop RSS | Drop footprint | Relief RSS | Relief footprint | Allocator bytes relieved (MiB) |',
          '| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |']
for row in results:
    if 'error' in row:
        lines.append(f'| {row["name"]} | ERROR | — | — | — | — | — | — |')
        continue
    values = [row[key] / 2**20 for key in ['rss_after_unload', 'footprint_unload', 'rss_after_drop', 'footprint_drop', 'rss_after_relief', 'footprint_relief', 'pressure_relief_bytes']]
    lines.append('| ' + row['name'] + ' | ' + ' | '.join(f'{value:.1f}' for value in values) + ' |')
lines += ['', 'Comparison baseline requested: approximately 1.1 GB. RSS includes the process, not just model weights.',
          'Load includes the runtime’s existing one-second silence warmup. Every case uses cached test_wavs/0.wav.',
          'Exact match is relative to the default native int8 CPU transcript, not a human-labeled WER.',
          'Session overrides are measurement-only. Production defaults are unchanged.', '']
for row in results:
    lines.append(f'**{row["name"]}:** {row.get("transcript", row.get("error"))}\n')
(output / 'results.json').write_text(json.dumps(results, indent=2))
table = '\n'.join(lines)
(output / 'results.md').write_text(table)
print(table)
if os.environ.get('GITHUB_STEP_SUMMARY'):
    with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as summary:
        summary.write(table)
