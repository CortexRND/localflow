# LocalFlow meeting sim — protocol (adapted from !sim_testing), frozen before the first run
SUT: CortexRND/localflow @ 27f2bba (PR #19 head; product code == c54aca8), worktree ~/work/sim-sut (detached).
LLM under test: Ollama llama3.2:3b (shipped default), CPU, product's own prompts/params (no changes).
STT: scripted transcriber (each line encoded as 5 s of tone on the speaker's source, 5 s silence on the other source; clocks aligned).
Audio: sd.InputStream patched to a no-op; system audio = fake source (ok) or open_system_audio raises (fail scenario).
Driver: real FastAPI app via TestClient: /meeting/templates, /meeting/start {title,template,user_notes=notes_initial},
  feed lines, /meeting/status mid-way, /meeting/notes {notes_final} (autosave), /meeting/stop, /meeting/chat per question,
  then simulated restart (server._last_saved=None) + one chat => restart path.
Repeats N=3 per scenario (6 scenarios, 21 chat questions). Results per repeat: ~/sim-runs/<run>/rK/results/<scenario>.json.
Judge: Jev via cortex evals/sim/judge.ts (live). Trusted this run: answer (1.000), abstention (1.000) [calibration 2026-11-15].
  Advisory: citation (0.878), custom notes-faithfulness questions (uncalibrated).

## Deterministic gates (pass = passes in every repeat; flaky = differs between repeats)
M1 lifecycle: all HTTP calls 2xx; notes+log files exist; payload.template == scenario.template; payload.enhanced == bool(notes_final). Threshold: all.
M2 attribution: logged speaker labels/order match scripted lines (system ok) — rate 1.0.
M3 raw notes: log contains "## My raw notes\n\n" + notes_final.strip() exactly (when notes non-empty) and NOT notes_initial-only — all.
M4 structure: every user heading appears as a Markdown heading in notes_md, same relative order — every enhanced meeting.
M5 user-point coverage: pooled share of user_points with >=1 keyword (case-insens.) in notes_md — >= 0.90.
M6 fact recall: pooled share of facts with >=1 keyword in notes_md — >= 0.70.
M7 unsupported numbers: share of numeric tokens in notes_md not present in LLM input (transcript as formatted + user notes + template text) — <= 0.05.
M8 template fidelity (no-notes scenario): every template section header present as heading — all.
M9 chat answerable: share of answers containing >=1 expected keyword — >= 0.80.
M10 chat abstention (unanswerable): share of answers matching abstain regex — >= 0.80.
M11 notes-only: answer abstains AND does not contain token — all.
M12 mic-only fallback: start.system_audio=="error" with non-empty error; status reports it; meeting saves; no "Others" lines; all Me lines present — all.
M13 restart chat: after _last_saved=None, chat 200 and log_path == saved log — all.
Not exercised (by design): long-transcript chunked summarize/merge and chat map-reduce (>2500/4000 words), real STT, real ScreenCaptureKit, native Hub UI.
Abstain regex: (doesn'?t|does not|did not|didn'?t|isn'?t|wasn'?t|is not|was not|not)\s+(mention|mentioned|contain|say|said|discuss|discussed|specif|state|include|provide|cover)|no (mention|information|record|details?)|not (in|part of) the transcript|transcript (doesn'?t|does not)

## Run log
- run dir: ~/sim-runs/$(cat ~/sim-runs/CURRENT); smoke (design-standup-nonotes) OK; full matrix: run_matrix.sh (N=3 sequential).
- Next after matrix: gates.py <run>; jev (cortex createJudge live: gradeAbstention on all chats with read_transcript_log text, gradeAnswer for answerable); repeatability check (re-run 1 scenario in fresh dir); findings PR on branch devin/<ts>-sim-quick-27f2bba off PR #19 branch with evals/meeting_sim/* + findings/sim/<date>-meetings/.
- Pending lead items: security scan form (localflow-security-1) awaiting submit -> create_scan commit_sha 27f2bba, effort deep; merge #19 then #20 once CI green + review clean (user authorized); sidekick on CI loop; Devin Review on 27f2bba/7155605 pending.
- Jev pipeline validated on smoke: `python jev_inputs.py <run>` then in ~/repos/cortex: `TYPESAFE_API_KEY="$JEV_API_Key" JUDGE_MODE=live npx tsx --conditions=development scripts/_lf-meeting-jev.ts <run>/jev_inputs.json <run>/jev_out.json` (model jev-1.14.0; script must be copied into findings PR as evals/meeting_sim/jev.ts, then deleted from cortex checkout — do NOT commit to cortex).
- Smoke finding: design-standup no-notes missed the "icon set later" follow-up (M6 0.667), and listed "Me: reviewing Lee's PR" under Yesterday (Jev advisory contradicts 0.69).
- Jev signals: suspect abstention = unanswerable/notes_only with sourcesSuffice.p>=0.8 trusted; contradicts = answerable verdict.pick=='contradicts' trusted; misses = statesDecision.p<0.5; truth check = answerable with sourcesSuffice.p<0.5 => truth bug suspect.
- Baseline matrix @27f2bba DONE: 18/18 cells, commit verified. Gates: M1,M2(117/117),M3,M5,M6(0.72-0.83),M8,M9(33/33),M10(26/27 raw),M11,M12,M13 pass; M4 FAIL 3/15 kept user headings (0/3 repeats); M7 raw fail 0.067-0.077.
- Adjudications: M7 only unsupported token is "11" = model echoing "1:1 with Jordan" title as "# 11 with Jordan" heading (title echo, not fabricated fact) -> adjudicated 0 fabricated numbers; M10 r3 "The annual contract discount wasn't agreed upon." is correct (regex miss) -> 27/27 adjudicated. Jev: 0 contradicts, 22 correct/11 partially of 33 answerable; Jev flagged "Did we discuss salary?" as sources-suffice (yes/no framing: transcript suffices to say "no") -> judge-truth framing, product answer correct.
- Consistent misses (3/3): design-standup icon-set follow-up -> "None recorded"; pricing churn reason. Duplicate title H1 in one-on-one.
- Product fix dispatched to sidekick: heading skeleton + one retry in summarize (PR #19). After push: re-run matrix at new SHA into ~/sim-runs/<run>/after-<sha>/rK, compare M4.
- Repeatability check (fresh dir, pricing-review + incident-retro @27f2bba): M4 fails again 0/2, M5 1.0, M6 0.857, M7 0, M9/M10 1.0 -> consistent with matrix.
- After-fix matrix @0577a73 launched: ~/sim-runs/<run>/after-0577a73 (SIM_SUT=~/work/sim-sut-after). gates.py imports from sim-sut (27f2bba) -- template/transcript helpers unchanged, ok.
- PR #19 head now 0909647 (retry length guard + unicode headings); #20 dac562a. Review job 68055601 triaged+ended. After-matrix @0577a73 (prompt skeleton identical to 0909647; only acceptance guard differs).
- AFTER @0577a73 (18/18, commit verified): M4 15/15 PASS (was 3/15); retry never fired (summarize_calls=1 all) -> skeleton alone fixed it. M6 flaky 2/3 (r1 0.611; pooled 41/54=0.759 vs baseline 42/54=0.778 -> no pooled change); pricing r1 kept user headings but dropped template Action Items (Marcus/Friday lost) and wrote placeholder "[Current Date]". M7 only "11" title echo again (no-title bullet ignored by 3B). M9 r1 one flake: vendor questionnaire answer was bare "[00:20]" (32/33). M10 27/27. Jev after-run in after-0577a73/jev_out.json.
- Jev notes advisory (uncalibrated): contradicts>=0.5 baseline 5/18 -> after 11/18; unsupported 4/18 -> 11/18; keepsStructure mean 0.54 -> 0.82. BUT no-notes scenario (prompt unchanged) moved 0.62->0.78 mean => high model/judge variance. Manual adjudication of after r1 eng-standup: real errors (Omar credited with Me's auth PR review; flaky-tests follow-up listed as Blocker; Blockers duplicated). after r2 incident-retro: faithful except audit owner assigned to Kim (unassigned). Baseline had same class (Me's PR review under Yesterday). Open finding: speaker/owner attribution errors with 3B model, not caused by fix; recommend larger default model eval. Not merge-blocking.
- CORRECTION after review of the harness: baseline M4 is 2/15, not 3/15 (13 of 15 dropped headings). M7 "11" came from gates.py not stripping the writer's sanitized title (`# 11 with Jordan`), not the model; after the fix M7 is 0 at both SHAs. The gates now also: fail M1 for missing scenarios (not exercised), count extra transcript lines against M2, and take M7 support only from the first summarize prompt (not log metadata) (so a retry's embedded draft can't launder numbers). Jev inputs now use stored log_text (identical results here). Churn was missed 3/3 at baseline and 2/3 after.
