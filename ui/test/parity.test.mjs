// JS-mirror ↔ Rust-engine parity.
//
// Half one: the expression cases in tests/parity/expr-cases.json, which the
// Rust unit test `rules::expr::tests::parity_cases` also runs — one file, two
// engines. Half two: whole-graph evaluation — the embedded wasm build of the
// Rust engine versus ui/eval.js over the sample scans, for a matrix of
// simulations, comparing outcomes for every job and traces for every job
// under a smaller matrix.
import { describe, test, before } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { loadEvaluator } from "./wasm.mjs";
import { SAMPLES, readSample } from "./samples.mjs";

const require = createRequire(import.meta.url);
const E = require("../eval.js");
const CASES = JSON.parse(readFileSync(new URL("../../tests/parity/expr-cases.json", import.meta.url), "utf8"));

function tableFrom(vars, into = new Map()) {
  for (const [name, spec] of Object.entries(vars)) {
    into.set(name, spec[0] === "known" ? { k: "known", v: spec[1] } : { k: spec[0] });
  }
  return into;
}

describe("rules:if expression cases", () => {
  const base = tableFrom(CASES.vars);
  for (const c of CASES.cases) {
    test(c.id, () => {
      const vars = new Map(base);
      if (c.vars) tableFrom(c.vars, vars);
      const r = E.evalIf(c.expr, vars);
      assert.equal(r.result, c.result, "result");
      assert.deepEqual(r.notes, c.notes, "notes");
      if (c.vars_used) {
        assert.deepEqual(r.varsUsed.map(([n, s]) => [n, E.stateText(s)]), c.vars_used, "vars_used");
      }
    });
  }
});

/* ---------------- whole-graph parity ---------------- */

const REFS = [
  { ref: "", tag: false },
  { ref: "release-1.0", tag: false },
  { ref: "v1.2.3", tag: true },
  { ref: "feature/ünïcode-" + "x".repeat(70), tag: false },
];
const ASSUMPTIONS = [[null, null], [true, false], [false, true]];
const VARS = [
  [],
  [["DEPLOY_ENV", "production"]],
  [["DEPLOY_ENV", "(unset)"], ["REPORT_KIND", "weekly"], ["SRC_DIR", "src"]],
  [["", "ignored"], ["CI_COMMIT_BRANCH", "(unset)"]],
];
// changed-file overrides: none (the embedded diff, if any), a matching list,
// a non-matching list, and an explicit empty diff
const CHANGED = [null, ["src/main.rs"], ["docs/sub/x.md", "README.md"], []];

function* sims(full) {
  for (const source of E.SOURCES) {
    for (const r of REFS) {
      for (const [ac, ae] of full ? ASSUMPTIONS : [ASSUMPTIONS[0]]) {
        for (const vars of full ? VARS : [VARS[0]]) {
          for (const cf of full ? CHANGED : [null, CHANGED[1]]) {
            yield { source, ref: r.ref, tag: r.tag, vars, assumeChanges: ac, assumeExists: ae, changedFiles: cf };
          }
        }
      }
    }
  }
}
const simLabel = (s) =>
  `${s.source} ref=${JSON.stringify(s.ref)} tag=${s.tag} assume=${s.assumeChanges},${s.assumeExists} ` +
  `vars=${JSON.stringify(s.vars)} changed=${JSON.stringify(s.changedFiles)}`;

// One canonical shape for both engines (the Rust JSON omits empty/none keys).
const canonTrace = (t) => ({
  index: t.index,
  result: t.result,
  clause: t.clause,
  when: t.when ?? null,
  varsUsed: t.varsUsed ?? t.vars_used ?? [],
  note: t.note ?? null,
});
const canonEval = (e) => ({
  outcome: e.outcome,
  blockedBy: e.blockedBy ?? e.blocked_by ?? null,
  variables: e.variables ?? {},
  trace: (e.trace ?? []).map(canonTrace),
});
const TRACE_KEYS = ["index", "result", "clause", "when", "varsUsed", "note"];

for (const sample of SAMPLES) {
  describe(`wasm vs JS mirror: ${sample.name}`, () => {
    let G, W, baseJobOf;
    before(async () => {
      const s = readSample(sample);
      G = s.graph;
      W = await loadEvaluator(s.jsonText);
      baseJobOf = E.baseJobMap(G);
    });

    test("ABI exports", () => {
      for (const name of ["glpv_alloc", "glpv_dealloc", "glpv_init", "glpv_eval", "glpv_result_len", "memory"]) {
        assert.ok(W.exports.includes(name), name);
      }
    });

    test("outcomes agree for every job under the full simulation matrix", () => {
      let sims_ = 0;
      for (const sim of sims(true)) {
        sims_++;
        const out = W.eval(E.wasmSimOf(sim, null));
        const js = E.evaluateGraph(G, sim, baseJobOf);
        for (const p of G.pipelines) {
          const byBase = out.pipelines[p.id] ?? {};
          const bases = new Map();
          for (const j of p.jobs) {
            const base = j.base_name || j.name;
            if (!bases.has(base)) bases.set(base, j);
          }
          assert.deepEqual(
            Object.keys(byBase).sort(),
            [...bases.keys()].sort(),
            `${simLabel(sim)}: base-job keys of ${p.project.path}`,
          );
          for (const [base, j] of bases) {
            const ev = js.get(j.id);
            assert.deepEqual(
              byBase[base],
              [ev.outcome, ev.blockedBy ?? null],
              `${simLabel(sim)}: ${p.project.path} / ${base}`,
            );
          }
        }
      }
      assert.ok(sims_ >= 2000, "matrix size " + sims_);
    });

    test("traces agree for every job under the reduced matrix", () => {
      let checked = 0;
      for (const sim of sims(false)) {
        const js = E.evaluateGraph(G, sim, baseJobOf);
        for (const p of G.pipelines) {
          for (const j of p.jobs) {
            const out = W.eval(E.wasmSimOf(sim, j.id));
            assert.ok(out.trace, `${simLabel(sim)}: wasm returned no trace for ${j.name}`);
            const jsEval = js.get(j.id);
            for (const t of jsEval.trace) {
              assert.deepEqual(Object.keys(t).sort(), [...TRACE_KEYS].sort(), "JS trace entry carries every key");
            }
            assert.deepEqual(
              canonEval(jsEval),
              canonEval(out.trace),
              `${simLabel(sim)}: ${p.project.path} / ${j.name}`,
            );
            checked++;
          }
        }
      }
      assert.ok(checked > 0);
    });
  });
}
