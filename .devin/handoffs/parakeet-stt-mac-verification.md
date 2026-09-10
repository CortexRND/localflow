# Cloud-agent handoff: Parakeet verified on Mac; skill commands need review

PR: https://github.com/CortexRND/localflow/pull/2
Branch: `devin/1789016679-parakeet-stt`
Starting PR head before the Mac fixes: `00aa01589a30a8b27ba35443c4ea760a991f4af0`.

## Status and ownership

The real Mac microphone/hotkey/paste test now works, and Taj confirmed it. This
handoff ships with the verified fixes and regression tests. Fetch the latest PR
branch in a clean checkout before continuing; do not redo the Parakeet backend.

**Do not merge yet.** Taj explicitly assigned known-skill command normalization
to the cloud agent and wants to review that follow-up before merging. The
normalization feature is not implemented in this verification change.

## Fixes made during verification

1. `uv.lock`: the original locked install selected NumPy 2.5.1, Numba 0.53.1,
   and llvmlite 0.36.0. A clean Apple Silicon install failed because llvmlite
   0.36.0 only supports Python below 3.10, whereas localflow requires 3.11+.
   Re-resolved to NumPy 2.4.6, Numba 0.66.0, and llvmlite 0.48.0. Numba 0.66
   requires NumPy below 2.5. A clean locked install with both Parakeet and MLX
   extras now succeeds. No dependency declarations or security policies changed.

2. `localflow/stt.py`: added `mx.eval(self._parakeet.parameters())` immediately
   after `from_pretrained`, before the existing silence warm-up. The library
   lazily casts loaded weights; silence decoding can bypass the token embedding.
   Consequently, silence warm-up alone did not evaluate every parameter.
   Desktop initialization happens on the main thread and transcription happens
   on a worker. Before the fix, every real microphone attempt failed with:

   ```text
   RuntimeError: There is no Stream(cpu, 1) in current thread.
   ```

   The same error was independently reproduced by loading/warming the real
   model on the main thread and submitting WAV transcription to a
   `ThreadPoolExecutor`. Evaluating all parameters on the loading thread fixes
   the actual thread-boundary problem without adding another worker or changing
   audio dtype. Do not remove this evaluation as redundant with silence warm-up.

3. `tests/test_stt.py`: ten offline tests covering eager load/evaluation/warm-up,
   main-thread initialization followed by worker-thread speech, short-input
   guards, the exact FFT-frame boundary, contiguous float32 conversion,
   evaluation-error propagation, and invalid backend selection. Three tests
   failed before the source fix; all ten pass afterward. MLX and Parakeet are
   stubbed, so these tests do not download models or require an Apple GPU.

## Verification evidence

Machine: Apple M5 Max, 48 GB RAM, macOS 26.6.2. Clean locked environment:
Python 3.13.14, parakeet-mlx 0.5.2, MLX 0.32.0. Initial real-model smoke and the
full mocked suite also ran with the original Python 3.14.6 environment.

- Clean `uv sync --locked --extra parakeet --extra mlx`: passed.
- `uv pip check`: all 89 packages in the clean environment compatible.
- Real-model short inputs, including empty input and 100 samples: `""`.
- One second of float32 silence at 16 kHz: `""`.
- In-memory speech: exact words on an 11.00675-second mono, 16 kHz WAV generated
  with macOS Samantha. Peak amplitude 0.8097; loaded as float32. All three
  backends matched the reference after case/punctuation normalization.
- Real main-thread load -> worker-thread speech: reproduced failure before the
  fix; passed after the fix, including repeated speech calls.
- Desktop: real built-in microphone, left Option, and paste into a text document
  passed after the fix, confirmed by Taj. Several real clips of approximately
  0.89-6.19 seconds logged 46-66 ms for transcription/symbol processing. These
  timings exclude the clipboard/keystroke delay.
- Post-fix `localflow-server`: `/healthz` and multipart `/transcribe` passed on
  loopback. Exact reference text, HTTP 200; 1,570 ms first request including
  model initialization, 106 ms warm including WAV decoding.
- Full suite: **110 passed**; `git diff --check` clean.

Warm medians on the same 11.00675-second synthesized clip, five measured calls:

| Backend | Median |
| --- | ---: |
| Parakeet, after fix, called from worker | 80.99 ms |
| `auto` selecting MLX Whisper small | 120.84 ms |
| faster-whisper small, CPU int8 | 1,312.55 ms |

Parakeet was about 1.5x faster than MLX Whisper small in this matched comparison.
The Whisper branches were unchanged by the fix. These are not directly
comparable to the README's older 13.8-second clip or its hardware. Cache-only
Parakeet load plus warm-up after the fix was 1.43 seconds; the initial observed
load including normal Hub access was 23.7 seconds.

## Required follow-up: recognized skill names

Taj's original example was `/skill_part`: dictating the words of a recognized
skill should produce the separator in its registered name without requiring
him to say "underscore" each time.

Taj clarified the policy: **use exact registered spelling, not underscores for
all names.** For the registered skill `candidate-solutioning`, the command is
`/candidate-solutioning`. For a registered skill named `skill_part`, it is
`/skill_part`. Do not invent unsupported underscore aliases for hyphenated names.

Current `localflow/symbols.py` only rewrites literal symbol words. Verified:

```text
"slash skill part"            -> "/skill part"
"slash skill underscore part" -> "/skill_part"
"Slash candidate solutioning." -> "/candidate solutioning."
"/skill_part"                 -> "/skill_part"
```

The live retry also produced `/candidate solutioning.` and
`/candidate solution.`. This is separate from the now-fixed STT thread failure;
there is no known-skill registry or automatic name joining in the current path.

### Implementation and review requirements

- Establish an explicit source of registered skill/command names for the target
  agent. Keep the collection injectable for offline tests; do not hard-code
  Taj's home-directory paths or assume every command can be found in this repo.
- Normalize only known names or explicitly supported aliases in slash-command
  context. Match spoken word boundaries/case while preserving the exact
  canonical separator. Do not blindly join every word after a slash.
- Handle STT-added punctuation for standalone known commands so the final
  command is usable, not `/skill_part.`. Preserve prose and command arguments.
- Preserve already-correct commands, unknown commands/paths, and ordinary prose.
  Do not apply command formatting to meeting transcripts.
- Keep desktop and HTTP dictation behavior consistent. Current integration
  points are `app._process_clip` and `server._transcribe_sync`, after optional
  cleanup. Exercise cleanup-enabled/disabled behavior with a mocked cleaner;
  do not use a live model API in the suite.
- Do not silently fuzzy-map ambiguous speech such as `candidate solution` to a
  command. Define and show the ambiguity/alias policy for Taj's review.

Minimum examples using a fixture registry containing `skill_part` and
`candidate-solutioning`:

| Input | Expected |
| --- | --- |
| `slash skill part` | `/skill_part` |
| `Slash skill part.` | `/skill_part` |
| `slash candidate solutioning` | `/candidate-solutioning` |
| `/candidate-solutioning` | unchanged |
| `/skill_part` | unchanged |
| `run slash skill part with these arguments` | `run /skill_part with these arguments` |
| `slash unknown words` | `/unknown words`, not an invented registered command |
| `we discussed candidate solutioning today` | unchanged |

Also test overlapping registered names, explicit spoken separators, punctuation,
and argument boundaries. Present the code diff and before/after examples to Taj.
A fresh Mac dictation check for the new normalization remains necessary before
claiming its end-to-end behavior is verified.

## Working-copy and verification constraints

- The original Mac checkout is a dirty `feat/note-to-prompt` branch with unrelated
  work. It was not switched, stashed, or edited. Verification/fixes used a
  detached worktree; do not include the unrelated idea-pipeline changes.
- That branch already has different symbol-regex handling from this PR. Review
  relevant upstream changes rather than overwriting them while adding the new
  registry-aware behavior.
- There was no `~/.localflow.toml` on this Mac. Tests used a temporary HOME/config
  with Parakeet selected and LLM cleanup, meeting watch, automatic meeting
  recording, and work prompts disabled. The real home config was never changed.
  Test app/server processes were stopped. Temporary audio/logs stayed outside
  tracked source; package/model caches remain available for further Mac checks.
- New agent skills/runners/configuration belong under `.devin/`.
- Use Python 3.11+ and the existing `.venv` for verification, with pytest present:

  ```bash
  .venv/bin/python -B -m pytest -p no:cacheprovider -ra --tb=short
  git diff --check
  ```

- Unit tests must not invoke live model APIs, Devin sessions, microphones, or
  clipboard actions. Keep future live Mac checks separately authorized.
- Push follow-up commits to the same PR branch and request Taj's review.
  **Do not merge until Taj has reviewed the skill-command behavior.**
