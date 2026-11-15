// LocalFlow meeting sim: Jev second judge (trusted: answer, abstention; advisory: notes faithfulness). Input/Output JSON paths.
import { readFileSync, writeFileSync } from 'node:fs';
import { TypeSafeClient, noul } from '@typesafe-ai/sdk';
import { createJudge } from '../evals/sim/judge.js' /* run from a CortexRND/cortex checkout: copy to cortex/scripts/ */;
const [input, output] = process.argv.slice(2);
const items = JSON.parse(readFileSync(input, 'utf8')) as Array<any>;
const judge = createJudge({ mode: 'live' });
const client = new TypeSafeClient({});
const out: any[] = [];
let inTok = 0, outTok = 0;
for (const it of items) {
  const row: any = { repeat: it.repeat, scenario: it.scenario };
  if (it.type === 'chat') {
    Object.assign(row, { q: it.q, kind: it.kind, answer: it.answer });
    const ab = await judge.gradeAbstention([it.transcript], it.q);
    row.sourcesSuffice = ab.sourcesSuffice; inTok += ab.meta.usage?.input_tokens ?? 0; outTok += ab.meta.usage?.output_tokens ?? 0;
    row.model = ab.meta.model;
    if (it.kind === 'answerable') {
      const g = await judge.gradeAnswer({ statement: it.truth, rationale: '', decider: '', date: '' }, { text: it.answer, citations: [] });
      row.verdict = g.verdict; row.statesDecision = g.statesDecision;
      inTok += g.meta.usage?.input_tokens ?? 0; outTok += g.meta.usage?.output_tokens ?? 0;
    }
  } else {
    const r = await client.systemOne({
      state: { transcript: it.transcript.slice(0, 20000), user_notes: it.user_notes, notes: it.notes.slice(0, 6000) },
      questions: {
        contradicts: noul('Does notes contain any statement that contradicts the transcript or user_notes?'),
        unsupported: noul('Does notes contain a specific claim (a name, number, date, owner or decision) that appears in neither the transcript nor user_notes?'),
        keepsStructure: noul('If user_notes is non-empty, does notes keep the user_notes headings in the same order? (Answer yes if user_notes is empty.)'),
      },
    } as any);
    row.notes = r.answers; row.model = r.model; inTok += r.usage.input_tokens; outTok += r.usage.output_tokens;
  }
  out.push(row);
}
writeFileSync(output, JSON.stringify({ usage: { input_tokens: inTok, output_tokens: outTok }, rows: out }, null, 1));
console.log(JSON.stringify({ rows: out.length, inTok, outTok }));
