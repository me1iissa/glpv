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

/* ---------------- search + shareable URL state ---------------- */

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

describe("search and URL state (fixtures, js)", () => {
  let v, glpv, window, G, html;
  const j = (x) => JSON.parse(JSON.stringify(x)); // window-realm values → plain
  const key = (win, target, key, init = {}) =>
    target.dispatchEvent(new win.KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init }));
  const type = (win, input, text) => {
    input.value = text;
    input.dispatchEvent(new win.Event("input", { bubbles: true }));
  };
  before(async () => {
    html = readSample(FIXTURES).html;
    v = await loadViewer(html, { wasm: false });
    ({ glpv, window } = v);
    G = glpv.G;
  });
  after(() => v && v.close());
  const clean = () => assert.equal(v.errors.length, 0, v.errors.join("\n"));

  test("index covers jobs, pipelines and stages; ranking is prefix > word start > anywhere > subsequence", () => {
    const kinds = new Set(glpv.search.index().map((e) => e.kind));
    assert.deepEqual([...kinds].sort(), ["job", "pipeline", "stage"]);
    const res = glpv.search.run("deploy");
    assert.ok(res.length >= 1 && res.length <= 12, String(res.length));
    assert.ok(res.every((r) => r.entry.kind && r.entry.label && "score" in r));
    const sc = glpv.search.score;
    assert.ok(sc("build", "build") > sc("build", "build-image"), "exact beats prefix");
    assert.ok(sc("build", "build-image") > sc("build", "docker-build"), "prefix beats word start");
    assert.ok(sc("build", "docker-build") > sc("build", "rebuild"), "word start beats substring");
    assert.ok(sc("build", "rebuild") > sc("bd", "build"), "substring beats subsequence");
    assert.equal(sc("xyz", "build"), -1);
    assert.equal(glpv.search.run("").length, 0);
    clean();
  });

  test("typing rings the matches and opens the list; Escape clears both", () => {
    const { input, list } = glpv.search.ui();
    type(window, input, "deploy");
    assert.ok(glpv.search.matches() && glpv.search.matches().size >= 1, "matches ringed");
    assert.equal(list.hidden, false);
    assert.ok(list.querySelectorAll(".search-row").length >= 1);
    key(window, input, "Escape");
    assert.equal(glpv.search.matches(), null);
    assert.equal(input.value, "");
    assert.equal(list.hidden, true);
    clean();
  });

  test("Enter on a job result flies to it and selects it", () => {
    const id = jobId(G, "fx/sim", "deploy-prod");
    const { input } = glpv.search.ui();
    type(window, input, "deploy-prod");
    key(window, input, "Enter");
    assert.equal(glpv.selectedJob, id);
    assert.ok(glpv.view.scale >= 0.7);
    assert.equal(glpv.search.ui().list.hidden, true);
    clean();
  });

  test("a pipeline result fits its card in the viewport", () => {
    const r = glpv.search.run("fx/legacy").find((x) => x.entry.kind === "pipeline");
    assert.ok(r, "pipeline result for fx/legacy");
    glpv.search.choose(r);
    const c = glpv.scene.cards[r.entry.cardIdx];
    const cx = (c.x + c.w / 2) * glpv.view.scale + glpv.view.tx;
    assert.ok(Math.abs(cx - 700) < 1, "card centred horizontally (vw=1400): " + cx);
    clean();
  });

  test("'/' focuses the search box unless the user is typing elsewhere", () => {
    const { input } = glpv.search.ui();
    window.document.body.focus();
    key(window, window.document.body, "/");
    assert.equal(window.document.activeElement, input);
    input.blur();
    const ref = glpv.simbar.querySelector("input.ref");
    ref.focus();
    const ev = new window.KeyboardEvent("keydown", { key: "/", bubbles: true, cancelable: true });
    ref.dispatchEvent(ev);
    assert.equal(ev.defaultPrevented, false, "slash typed into another input is not intercepted");
    assert.equal(window.document.activeElement, ref);
    ref.blur();
    clean();
  });

  test("state omits defaults and round-trips through the hash encoding", () => {
    glpv.search.clear();
    glpv.state.apply({ v: 1 });
    const st = glpv.state.current();
    assert.deepEqual(j(Object.keys(st)), ["v", "cam"]);
    assert.deepEqual(j(glpv.state.decode(glpv.state.encode(st))), j(st));
    glpv.sim.vars = [["A", "1"], ["", "x"], ["B", "(unset)"]];
    assert.deepEqual(j(glpv.state.current().vars), [["A", "1"], ["B", "(unset)"]]);
    assert.equal(glpv.state.decode("#foo"), null);
    assert.equal(glpv.state.decode(""), null);
    assert.deepEqual(j(glpv.state.decode(glpv.state.encode({ v: 9, zzz: 1 }))), { v: 9, zzz: 1 });
    clean();
  });

  test("apply is defensive about every key", () => {
    const id = jobId(G, "fx/sim", "build");
    glpv.state.apply({ v: 1, sel: "nope" });
    assert.equal(glpv.selectedJob, null);
    glpv.state.apply({ v: 1, cam: [NaN, 0, 0] });
    assert.ok(Number.isFinite(glpv.view.scale) && Number.isFinite(glpv.view.tx) && Number.isFinite(glpv.view.ty));
    glpv.state.apply({ v: 1, mode: "all" });
    assert.equal(glpv.edgeMode, "all");
    assert.equal(window.document.querySelector(".topbar select").value, "all");
    glpv.state.apply({ v: 1, mode: "bogus" });
    assert.equal(glpv.edgeMode, "focus");
    glpv.state.apply({ v: 9, s: "web", r: "main", t: 1, ac: true, ae: "yes", vars: [["X", "1"], "junk", ["", "2"]], zzz: 1 });
    assert.equal(glpv.sim.source, "web");
    assert.equal(glpv.sim.ref, "main");
    assert.equal(glpv.sim.tag, true);
    assert.equal(glpv.sim.assumeChanges, true);
    assert.equal(glpv.sim.assumeExists, null);
    assert.deepEqual(j(glpv.sim.vars), [["X", "1"]]);
    assert.equal(glpv.simbar.querySelector("select").value, "web");
    assert.equal(glpv.simbar.querySelector("input.ref").value, "main");
    assert.equal(glpv.simbar.querySelector('input[type="checkbox"]').checked, true);
    assert.equal(glpv.simbar.querySelector('select[data-assume="changes"]').value, "true");
    glpv.state.apply({ v: 1, s: "nonsense", sel: id, cam: [1.5, 100, 50] });
    assert.equal(glpv.sim.source, "push");
    assert.equal(glpv.selectedJob, id);
    assert.equal(glpv.view.scale, 1.5);
    assert.ok(Math.abs((700 - glpv.view.tx) / 1.5 - 100) < 1, "camera centre x restored");
    glpv.state.apply({ v: 1 });
    assert.equal(glpv.selectedJob, null);
    clean();
  });

  test("changes write the hash (debounced); hashchange re-applies; a refusing replaceState is swallowed", async () => {
    const id = jobId(G, "fx/sim", "build");
    glpv.state.apply({ v: 1 });
    glpv.selectJob(id);
    await sleep(350);
    assert.equal(glpv.state.decode(window.location.hash).sel, id);
    assert.equal(glpv.state.lastHash(), window.location.hash);
    window.location.hash = glpv.state.encode({ v: 1, s: "schedule" });
    await sleep(50);
    assert.equal(glpv.sim.source, "schedule");
    assert.equal(glpv.selectedJob, null);
    const orig = window.history.replaceState;
    window.history.replaceState = () => { throw new Error("sandboxed"); };
    try {
      glpv.sim.ref = "hotfix";
      glpv.applyEval();
      glpv.state.write();
      assert.equal(glpv.state.decode(glpv.state.lastHash()).r, "hotfix");
    } finally {
      window.history.replaceState = orig;
    }
    clean();
  });

  test("a link restores simulation, selection, edge mode and camera in a fresh window", async () => {
    const id = jobId(G, "fx/sim", "deploy-prod");
    const st = { v: 1, s: "merge_request_event", vars: [["DEPLOY_ENV", "production"]], sel: id, mode: "triggers", cam: [1.2, 300, 200] };
    const v2 = await loadViewer(html, { wasm: false, hash: "#" + glpv.state.encode(st) });
    try {
      assert.equal(v2.glpv.sim.source, "merge_request_event");
      assert.deepEqual(j(v2.glpv.sim.vars), [["DEPLOY_ENV", "production"]]);
      assert.equal(v2.glpv.selectedJob, id);
      assert.ok(!v2.glpv.panel.classList.contains("hidden"));
      assert.equal(v2.glpv.edgeMode, "triggers");
      assert.equal(v2.glpv.view.scale, 1.2);
      assert.ok(Math.abs((700 - v2.glpv.view.tx) / 1.2 - 300) < 1);
      assert.ok(Math.abs((450 - v2.glpv.view.ty) / 1.2 - 200) < 1);
      assert.equal(v2.glpv.state.lastHash(), v2.window.location.hash);
      assert.equal(v2.errors.length, 0, v2.errors.join("\n"));
    } finally {
      v2.close();
    }
  });
});

/* ---------------- stack collapse ---------------- */

describe("stack collapse (fixtures, js)", () => {
  let v, glpv, window, G, html;
  const j = (x) => JSON.parse(JSON.stringify(x));
  before(async () => {
    html = readSample(FIXTURES).html;
    v = await loadViewer(html, { wasm: false });
    ({ glpv, window } = v);
    G = glpv.G;
  });
  after(() => v && v.close());
  const clean = () => assert.equal(v.errors.length, 0, v.errors.join("\n"));
  const fanoutGroup = () =>
    [...glpv.stacks.groups().values()].find((g) => g.members[0].project.path === "fx/fanout");

  test("near-identical leaf children form a group; the toggle folds them into one card and back", () => {
    const g = fanoutGroup();
    assert.ok(g, "fx/fanout group");
    assert.equal(g.members.length, 4);
    assert.deepEqual(j(g.names), ["component-alpha", "component-beta", "component-gamma", "component-delta"]);
    const toggle = window.document.querySelector(".stack-toggle input");
    assert.ok(toggle && !toggle.parentElement.hidden, "toggle shown when groups exist");
    const cards0 = glpv.scene.cards.length, pills0 = glpv.scene.pills.length;
    const view0 = j(glpv.view);

    glpv.stacks.setEnabled(true);
    assert.equal(toggle.checked, true);
    assert.equal(glpv.scene.cards.length, cards0 - 3, "three member cards folded away");
    assert.equal(glpv.scene.pills.length, pills0 - 3 * 2, "their pills too");
    const stackCard = glpv.scene.cards.find((c) => c.stack);
    assert.ok(stackCard, "one stack card");
    assert.equal(stackCard.stack.members.length, 4);
    assert.ok(glpv.scene.labels.some((l) => l.lines.includes("×4")), "the bus label still says ×4");
    const memberIds = new Set(g.members.map((m) => m.id));
    const into = glpv.scene.edges.filter((e) => e.toPipeline && memberIds.has(e.toPipeline));
    assert.equal(into.length, 4, "every bridge still has a branch, routed into the stack card");
    assert.deepEqual(j(glpv.view), view0, "camera preserved across the relayout");
    assert.equal(glpv.state.current().stk, 1);
    assert.equal(glpv.search.index().filter((e) => e.kind === "pipeline").length, glpv.scene.cards.length);

    // selecting a hidden member's job expands the group first
    const hiddenJob = g.members[2].jobs[0].id;
    assert.ok(glpv.stacks.hidden().has(g.members[2].id));
    glpv.selectJob(hiddenJob);
    assert.ok(glpv.stacks.expanded().has(g.key));
    assert.equal(glpv.scene.cards.length, cards0);
    assert.equal(glpv.selectedJob, hiddenJob);
    assert.ok(!glpv.panel.classList.contains("hidden"));

    glpv.stacks.setEnabled(false);
    assert.equal(toggle.checked, false);
    assert.equal(glpv.scene.cards.length, cards0);
    assert.equal(glpv.stacks.expanded().size, 0);
    assert.equal(glpv.state.current().stk, undefined);
    clean();
  });

  test("clicking a stack card expands it in place", () => {
    glpv.selectJob(null);
    glpv.stacks.setEnabled(true);
    const cd = glpv.scene.cards.find((c) => c.stack);
    const cardsFolded = glpv.scene.cards.length;
    const cx = (cd.x + cd.w / 2) * glpv.view.scale + glpv.view.tx;
    const cy = (cd.y + 10) * glpv.view.scale + glpv.view.ty; // the header, off any pill
    const ev = (type) => new window.PointerEvent(type, { clientX: cx, clientY: cy, pointerId: 1, bubbles: true });
    glpv.viewport.dispatchEvent(ev("pointerdown"));
    glpv.viewport.dispatchEvent(ev("pointerup"));
    assert.ok(glpv.stacks.expanded().has(cd.stack.key), "expanded by click");
    assert.equal(glpv.scene.cards.length, cardsFolded + 3);
    glpv.stacks.setEnabled(false);
    clean();
  });

  test("stk in a link restores the folded board", async () => {
    const cards0 = glpv.scene.cards.length;
    const v2 = await loadViewer(html, { wasm: false, hash: "#" + glpv.state.encode({ v: 1, stk: 1 }) });
    try {
      assert.equal(v2.glpv.scene.cards.length, cards0 - 3);
      assert.equal(v2.window.document.querySelector(".stack-toggle input").checked, true);
      assert.equal(v2.errors.length, 0, v2.errors.join("\n"));
    } finally {
      v2.close();
    }
  });
});
