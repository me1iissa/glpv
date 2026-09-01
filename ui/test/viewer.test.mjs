// Headless smoke run of the whole viewer over both sample scans, with the
// wasm evaluator and with the JS mirror: boot without errors, layout sanity,
// selection → panel, enable-in-simulation, the outcome explorer.
import { describe, test, before, after } from "node:test";
import assert from "node:assert/strict";
import { SAMPLES, readSample } from "./samples.mjs";
import { loadViewer, hasCanvas, labelsOverlap, labelHitsPill } from "./harness.mjs";
import { shot } from "./shots.mjs";

const BY_NAME = (name) => (s) => s.name === name;
const FIXTURES = SAMPLES.find(BY_NAME("fixtures"));
const DEMO = SAMPLES.find(BY_NAME("demo"));

function jobId(G, projectPath, jobName) {
  const p = G.pipelines.find((x) => x.project.path === projectPath && x.kind === "root");
  assert.ok(p, `root pipeline ${projectPath}`);
  const j = p.jobs.find((x) => x.name === jobName);
  assert.ok(j, `job ${jobName} in ${projectPath}`);
  return j.id;
}

function expectedNeedsEdges(G, glpv) {
  const payload = (j) => glpv.payloadJob(j);
  const seen = new Set();
  for (const p of G.pipelines) {
    for (const j of p.jobs) {
      const jBase = payload(j);
      for (const n of j.needs || []) {
        if (n.kind !== "normal") continue;
        const target = glpv.jobById(p.id + "/" + n.job);
        if (!target) continue;
        const tBase = payload(target);
        if (tBase.id === jBase.id) continue;
        if (!glpv.scene.pillByJob.has(tBase.id) || !glpv.scene.pillByJob.has(jBase.id)) continue;
        seen.add(p.id + "|" + tBase.id + "|" + jBase.id);
      }
    }
  }
  return seen.size;
}

function recount(G, glpv) {
  const res = glpv.lastEval();
  const c = { runs: 0, manual: 0, skipped: 0, blocked: 0, unknown: 0, delayed: 0 };
  for (const p of G.pipelines) {
    const off = res.status.get(p.id) === "off";
    for (const j of p.jobs) {
      const ev = res.jobEval.get(j.id);
      if (!ev) continue;
      if (off && p.kind !== "root") { c.skipped++; continue; }
      c[ev.outcome] = (c[ev.outcome] || 0) + 1;
    }
  }
  return c;
}

const clickExplore = (panel, text) => {
  const btn = [...panel.querySelectorAll("button.explore")].find((b) => b.textContent === text);
  assert.ok(btn, `button "${text}"`);
  btn.click();
};

for (const sample of SAMPLES) {
  for (const wasm of [true, false]) {
    describe(`${sample.name} (${wasm ? "wasm" : "js"})`, () => {
      let v, G, glpv, window;
      before(async () => {
        const s = readSample(sample);
        v = await loadViewer(s.html, { wasm });
        ({ glpv, window } = v);
        G = glpv.G;
        await shot(glpv, `${sample.name}-${wasm ? "wasm" : "js"}-boot`);
      });
      after(() => v && v.close());
      const clean = () => assert.equal(v.errors.length, 0, v.errors.join("\n"));

      test("boots without errors", () => {
        clean();
        assert.equal(glpv.wasmActive(), wasm);
        assert.equal(glpv.mode, hasCanvas ? "canvas2d" : "none");
        assert.equal(!!window.document.querySelector(".render-note"), !hasCanvas);
        assert.equal(G.pipelines.length > 0, true);
      });

      test("scene statistics are consistent with the graph", () => {
        const { scene } = glpv;
        assert.equal(scene.cards.length, G.pipelines.length, "one card per pipeline");
        let bases = 0;
        for (const p of G.pipelines) bases += new Set(p.jobs.map((j) => j.base_name || j.name)).size;
        assert.equal(scene.pills.length, bases, "one pill per base job");
        assert.equal(scene.pillByJob.size, bases);
        assert.equal(
          scene.edges.filter((e) => e.cls === "needs").length,
          expectedNeedsEdges(G, glpv),
          "needs edges deduplicated to base-job pairs",
        );
        for (const l of scene.labels) {
          assert.ok(l.w > 0 && l.h > 0 && l.lines.length > 0, "label has a size and text");
        }
        for (let i = 0; i < scene.labels.length; i++) {
          for (let k = i + 1; k < scene.labels.length; k++) {
            assert.ok(!labelsOverlap(scene.labels[i], scene.labels[k]), `labels ${i} and ${k} overlap`);
          }
          for (const p of scene.pills) {
            assert.ok(!labelHitsPill(scene.labels[i], p), `label ${i} covers pill ${p.name}`);
          }
        }
        clean();
      });

      test("topbar counts match a recount of the evaluation", () => {
        const c = recount(G, glpv);
        const text = glpv.counts.textContent;
        assert.match(text, new RegExp(`\\b${c.runs} run\\b`));
        assert.match(text, new RegExp(`\\b${c.skipped + c.blocked} skipped\\b`));
        if (c.manual) assert.match(text, new RegExp(`\\b${c.manual} manual\\b`));
        if (c.unknown) assert.match(text, new RegExp(`\\b${c.unknown} unknown\\b`));
        clean();
      });

      if (wasm) {
        test("the JS mirror reproduces the wasm evaluation in-page", () => {
          const snapshot = new Map();
          for (const [id, ev] of glpv.lastEval().jobEval) snapshot.set(id, [ev.outcome, ev.blockedBy ?? null]);
          glpv.disableWasm();
          assert.equal(glpv.wasmActive(), false);
          for (const [id, ev] of glpv.lastEval().jobEval) {
            assert.deepEqual([ev.outcome, ev.blockedBy ?? null], snapshot.get(id), id);
          }
          clean();
        });
      }

      if (sample === FIXTURES) {
        test("selecting a job renders its panel", async () => {
          const id = jobId(G, "fx/sim", "deploy-prod");
          glpv.selectJob(id);
          assert.equal(glpv.selectedJob, id);
          assert.ok(!glpv.panel.classList.contains("hidden"));
          assert.equal(glpv.panel.querySelector("h2").textContent, "deploy-prod");
          assert.match(glpv.panel.querySelector(".badge").textContent, /^unknown/);
          const h3s = [...glpv.panel.querySelectorAll("h3")].map((h) => h.textContent);
          assert.ok(h3s.includes("Invocation simulation"), h3s.join(" | "));
          if (wasm) assert.ok(h3s.includes("Rule trace (current simulation)"), h3s.join(" | "));
          await shot(glpv, `${sample.name}-${wasm ? "wasm" : "js"}-selected`);
          clean();
        });

        test("enable in simulation: sets the variable the gate needs", () => {
          const id = jobId(G, "fx/sim", "deploy-prod");
          if (glpv.selectedJob !== id) glpv.selectJob(id);
          clickExplore(glpv.panel, "⚡ Enable in simulation");
          assert.ok(
            glpv.sim.vars.some(([k, val]) => k === "DEPLOY_ENV" && val === "production"),
            JSON.stringify(glpv.sim.vars),
          );
          assert.equal(glpv.lastEval().jobEval.get(id).outcome, "runs");
          const row = [...glpv.simbar.querySelectorAll(".var-row input")].find((i) => i.value === "DEPLOY_ENV");
          assert.ok(row, "sim bar shows the DEPLOY_ENV row");
          clean();
        });

        test("enable in simulation: decides a changes/exists gate", () => {
          const id = jobId(G, "fx/sim", "infra-apply");
          glpv.selectJob(id);
          assert.equal(glpv.lastEval().jobEval.get(id).outcome, "unknown");
          clickExplore(glpv.panel, "⚡ Enable in simulation");
          const ev = glpv.lastEval().jobEval.get(id);
          assert.ok(ev.outcome === "runs" || ev.outcome === "manual", ev.outcome);
          assert.notEqual(glpv.sim.assumeChanges === null && glpv.sim.assumeExists === null, true, "an assumption was set");
          clean();
        });

        test("a workflow-blocked job can be enabled by switching the source", () => {
          glpv.sim.source = "merge_request_event";
          glpv.sim.vars = [];
          glpv.sim.assumeChanges = null;
          glpv.sim.assumeExists = null;
          glpv.refreshSimBar();
          glpv.applyEval();
          const id = jobId(G, "fx/app", "build");
          const before_ = glpv.lastEval().jobEval.get(id);
          assert.equal(before_.outcome, "blocked");
          assert.equal(before_.blockedBy, "workflow:rules");
          if (glpv.selectedJob !== id) glpv.selectJob(id);
          assert.match(glpv.panel.querySelector(".badge").textContent, /^blocked \(workflow:rules\)/);
          assert.match(glpv.panel.textContent, /blocks the pipeline/);
          clickExplore(glpv.panel, "⚡ Enable in simulation");
          assert.equal(glpv.sim.source, "push");
          assert.equal(glpv.lastEval().jobEval.get(id).outcome, "runs");
          assert.equal(glpv.simbar.querySelector("select").value, "push");
          clean();
        });

        test("the outcome explorer finds several outcomes for a gated job", async () => {
          for (const name of ["deploy-prod", "infra-apply"]) {
            const id = jobId(G, "fx/sim", name);
            if (glpv.selectedJob !== id) glpv.selectJob(id);
            clickExplore(glpv.panel, "Explore all possible outcomes →");
            const cards = glpv.panel.querySelectorAll(".otree-box .oc-list .oc");
            assert.ok(cards.length >= 2, `${name}: ${cards.length} outcome card(s)`);
            const note = glpv.panel.querySelector(".otree-box .note");
            assert.match(note.textContent, /^\d+ distinct outcome\(s\)/);
            assert.equal(Number(note.textContent.split(" ")[0]), cards.length);
          }
          await shot(glpv, `${sample.name}-${wasm ? "wasm" : "js"}-explorer`);
          clean();
        });
      }

      if (sample === DEMO) {
        test("a bridge gated on the branch switches its child pipeline off and on", () => {
          const id = jobId(G, "pipelines-demo/shop", "deploy-review");
          const child = G.pipelines.find((p) => p.parent && p.parent[1] === "deploy-review");
          assert.ok(child, "child pipeline of deploy-review");
          glpv.sim.source = "merge_request_event";
          glpv.refreshSimBar();
          glpv.applyEval();
          // no workflow gate in this project: the branch rule simply does not match
          assert.equal(glpv.lastEval().jobEval.get(id).outcome, "skipped");
          assert.equal(glpv.lastEval().status.get(child.id), "off");
          glpv.selectJob(id);
          clickExplore(glpv.panel, "⚡ Enable in simulation");
          assert.equal(glpv.sim.source, "push");
          assert.equal(glpv.lastEval().jobEval.get(id).outcome, "runs");
          assert.equal(glpv.lastEval().status.get(child.id), "on");
          const before_ = { ...glpv.view };
          glpv.flyTo(glpv.scene.pillByJob.get(id));
          assert.notDeepEqual(glpv.view, before_, "flyTo moved the camera");
          clean();
        });
      }
    });
  }
}
