/* glpv interactive viewer.
 *
 * Reads the embedded graph JSON, lays the pipelines out as project lanes →
 * pipeline cards → stage columns → job pills, draws needs/trigger edges on a
 * WebGL2 (or Canvas2D) board, and re-evaluates every job's rules live as the
 * user changes the simulated pipeline source, ref and variables.
 *
 * The rules evaluator lives in eval.js (embedded ahead of this file in the
 * same script scope): the canonical Rust engine compiled to WebAssembly is
 * used when available, the JS mirror otherwise.
 */
"use strict";

// Errors are collected for headless smoke tests (window.__glpv.errors).
const __glpvErrors = [];
addEventListener("error", (e) => {
  __glpvErrors.push(String((e.error && e.error.stack) || e.message || e));
});
addEventListener("unhandledrejection", (e) => {
  __glpvErrors.push("unhandledrejection: " + String(e.reason));
});
{
  const orig = console.error;
  console.error = (...a) => {
    __glpvErrors.push(a.map(String).join(" "));
    orig.apply(console, a);
  };
}

const G = JSON.parse(document.getElementById("glpv-graph").textContent);

/* ================= small helpers ================= */

function h(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}
function truncate(s, n) {
  return s.length > n ? s.slice(0, n) + "…" : s;
}

/* ================= simulation state ================= */

const DEFAULT_SOURCE = (G.scenarios[0] && G.scenarios[0].source) || "push";
const sim = {
  source: DEFAULT_SOURCE,
  ref: "",
  tag: false,
  vars: [], // [key, value]; the value "(unset)" simulates an unset variable
  assumeChanges: null, // simulation-wide rules:changes assumption
  assumeExists: null, // simulation-wide rules:exists assumption
};

/* ================= graph-wide evaluation ================= */

const pipeOfJob = new Map();
for (const p of G.pipelines) for (const j of p.jobs) pipeOfJob.set(j.id, p);

// Parallel/matrix expansions carry only a stub; the first expansion of each
// base holds the shared payload (rules, YAML, provenance, variables).
const baseJobOf = baseJobMap(G);
function payloadJob(j) {
  return baseJobOf.get(j.id) || j;
}

/* ================= evaluation engines =================
 * Primary: the canonical Rust evaluator compiled to WebAssembly (embedded
 * base64 island). Fallback: the JS mirror above, used until the module is
 * instantiated or when WebAssembly is unavailable. */

let wasmEval = null; // (simJsonString) => parsed result | null

/** Resolves to true once the wasm evaluator is live, false when unavailable. */
function startWasm() {
  const island = document.getElementById("glpv-eval-wasm");
  if (
    !island ||
    typeof WebAssembly === "undefined" ||
    typeof TextEncoder === "undefined"
  )
    return Promise.resolve(false);
  let bytes;
  try {
    const bin = atob(island.textContent.trim());
    bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  } catch (e) {
    return Promise.resolve(false);
  }
  return WebAssembly.instantiate(bytes, {})
    .then(({ instance }) => {
      const ex = instance.exports;
      const enc = new TextEncoder();
      const dec = new TextDecoder();
      // memory.buffer is re-fetched on every use: it detaches when wasm grows.
      const put = (s) => {
        const b = enc.encode(s);
        const p = ex.glpv_alloc(b.length);
        new Uint8Array(ex.memory.buffer, p, b.length).set(b);
        return [p, b.length];
      };
      const [gp, gl_] = put(document.getElementById("glpv-graph").textContent);
      const ok = ex.glpv_init(gp, gl_);
      ex.glpv_dealloc(gp, gl_);
      if (ok !== 0) return false;
      wasmEval = (simJson) => {
        try {
          const [p, l] = put(simJson);
          const rp = ex.glpv_eval(p, l);
          ex.glpv_dealloc(p, l);
          if (!rp) return null;
          const rl = ex.glpv_result_len();
          const out = dec.decode(new Uint8Array(ex.memory.buffer, rp, rl));
          ex.glpv_dealloc(rp, rl);
          return JSON.parse(out);
        } catch (e) {
          wasmEval = null; // one bad call disables it; JS fallback takes over
          return null;
        }
      };
      applyEval(); // re-run under the canonical evaluator
      return true;
    })
    .catch(() => false);
}

// The wasm trace is the Rust JSON (snake_case, optional keys); the JS mirror
// always emits every key. Normalise so the panel sees one shape.
const adaptTrace = (ts) =>
  (ts || []).map((t) => ({
    index: t.index,
    result: t.result,
    clause: t.clause,
    when: t.when ?? null,
    varsUsed: t.vars_used || [],
    note: t.note ?? null,
  }));

function evaluateAll() {
  let jobEval = new Map();
  let filled = false;
  if (wasmEval) {
    const res = wasmEval(JSON.stringify(wasmSimOf(sim, selectedJob)));
    if (res && res.pipelines) {
      filled = true;
      for (const p of G.pipelines) {
        const byBase = res.pipelines[p.id] || {};
        for (const j of p.jobs) {
          const e = byBase[j.base_name || j.name];
          if (e)
            jobEval.set(j.id, {
              outcome: e[0],
              blockedBy: e[1] || undefined,
              trace: null,
            });
        }
      }
      if (res.trace && selectedJob && jobEval.has(selectedJob)) {
        jobEval.get(selectedJob).trace = adaptTrace(res.trace.trace);
      }
    }
  }
  if (!filled) jobEval = evaluateGraph(G, sim, baseJobOf);

  // Pipeline reachability through trigger edges.
  const rank = { off: 0, unknown: 1, gate: 2, on: 3 };
  const status = new Map();
  for (const p of G.pipelines)
    status.set(p.id, p.kind === "root" || p.kind === "detached" ? "on" : "off");
  for (let pass = 0, changed = true; changed && pass < 10; pass++) {
    changed = false;
    for (const e of G.trigger_edges) {
      const src = pipeOfJob.get(e.from_job);
      if (!src) continue;
      const sSt = status.get(src.id);
      if (sSt === "off") continue;
      const ev = jobEval.get(e.from_job);
      if (!ev) continue;
      let t;
      if (ev.outcome === "runs" || ev.outcome === "delayed") t = sSt === "on" ? "on" : sSt;
      else if (ev.outcome === "manual") t = sSt === "on" ? "gate" : sSt === "gate" ? "gate" : "unknown";
      else if (ev.outcome === "unknown") t = "unknown";
      else t = "off";
      if (rank[t] > rank[status.get(e.to_pipeline)]) {
        status.set(e.to_pipeline, t);
        changed = true;
      }
    }
  }
  return { jobEval, status };
}

function jobById(id) {
  const p = pipeOfJob.get(id);
  return p ? p.jobs.find((j) => j.id === id) : null;
}

/* ================= palette (canvas needs concrete colors) ================= */

function parseColor(str) {
  str = (str || "").trim();
  let m;
  if ((m = /^#([0-9a-f]{3})$/i.exec(str)))
    return [...m[1]].map((c) => parseInt(c + c, 16) / 255).concat([1]);
  if ((m = /^#([0-9a-f]{6})([0-9a-f]{2})?$/i.exec(str))) {
    const v = m[1];
    const a = m[2] ? parseInt(m[2], 16) / 255 : 1;
    return [
      parseInt(v.slice(0, 2), 16) / 255,
      parseInt(v.slice(2, 4), 16) / 255,
      parseInt(v.slice(4, 6), 16) / 255,
      a,
    ];
  }
  if ((m = /^rgba?\(([^)]+)\)$/.exec(str))) {
    const parts = m[1].split(/[\s,/]+/).filter(Boolean).map(parseFloat);
    return [parts[0] / 255, parts[1] / 255, parts[2] / 255, parts.length > 3 ? parts[3] : 1];
  }
  return null;
}
function mix(a, b, t) {
  return [
    a[0] + (b[0] - a[0]) * t,
    a[1] + (b[1] - a[1]) * t,
    a[2] + (b[2] - a[2]) * t,
    a[3] + (b[3] - a[3]) * t,
  ];
}
function withA(c, a) {
  return [c[0], c[1], c[2], a];
}
function css(c, aMul) {
  const a = c[3] * (aMul === undefined ? 1 : aMul);
  return `rgba(${Math.round(c[0] * 255)},${Math.round(c[1] * 255)},${Math.round(c[2] * 255)},${a})`;
}

let PAL = null;
function readPalette() {
  const cs =
    typeof getComputedStyle === "function"
      ? getComputedStyle(document.documentElement)
      : null;
  const g = (name, fallback) =>
    (cs && parseColor(cs.getPropertyValue(name))) || parseColor(fallback);
  PAL = {
    ground: g("--ground", "#fafafc"),
    panel: g("--panel", "#f1f2f7"),
    card: g("--card", "#ffffff"),
    ink: g("--ink", "#22252e"),
    muted: g("--muted", "#6a6f7e"),
    line: g("--line", "#e0e2ea"),
    accent: g("--accent", "#6d4fc4"),
    accentSoft: g("--accent-soft", "#ece7fa"),
    ok: g("--ok", "#1a7f37"),
    warn: g("--warn", "#9a6700"),
    err: g("--err", "#cf222e"),
    needs: g("--edge-needs", "#0969da"),
    trig: g("--edge-trigger", "#8250df"),
  };
}

/* ================= scene: layout without a DOM ================= */

const KIND_LABEL = {
  root: "pipeline",
  multi_project: "downstream",
  child: "child",
  dynamic_child: "dynamic child",
  unresolved: "unresolved",
  detached: "detached",
};
const REDUCED_MOTION =
  typeof matchMedia !== "undefined" &&
  matchMedia("(prefers-reduced-motion: reduce)").matches;

const F = {
  pill: "12px system-ui, sans-serif",
  badge: "600 10px system-ui, sans-serif",
  head: "600 12px system-ui, sans-serif",
  small: "11px system-ui, sans-serif",
  stage: "600 10px system-ui, sans-serif",
  band: "600 13px system-ui, sans-serif",
  label: "10px system-ui, sans-serif",
};

const measureCtx = (() => {
  try {
    return document.createElement("canvas").getContext("2d");
  } catch (e) {
    return null;
  }
})();
function textW(s, font) {
  if (!measureCtx) return String(s).length * 6.4;
  measureCtx.font = font;
  return measureCtx.measureText(String(s)).width;
}
function fitText(s, font, maxW) {
  s = String(s);
  if (textW(s, font) <= maxW) return s;
  let lo = 1,
    hi = s.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (textW(s.slice(0, mid) + "…", font) <= maxW) lo = mid;
    else hi = mid - 1;
  }
  return s.slice(0, lo) + "…";
}
function wrapText(s, font, maxW, maxLines) {
  const words = String(s).split(/\s+/);
  const lines = [];
  let cur = "";
  for (const w of words) {
    const next = cur ? cur + " " + w : w;
    if (textW(next, font) <= maxW || !cur) cur = next;
    else {
      lines.push(cur);
      cur = w;
      if (lines.length === maxLines - 1) break;
    }
  }
  if (cur) lines.push(fitText(cur === "" ? "" : words.slice(lines.join(" ").split(/\s+/).filter(Boolean).length).join(" ") || cur, font, maxW));
  return lines.slice(0, maxLines);
}

const PILL_H = 26;
const PILL_GAP = 7;
const SUBCOL_WRAP = 30; // pills per sub-column before a stage wraps
const SUBCOL_GAP = 18; // wide enough for a routing lane
const STAGE_GAP = 44; // wide enough to route edges through
const CARD_PAD = 12;
const HEAD_H = 34;
const STAGE_TITLE_H = 20;
const COL_GAP = 120;
const BAND_GAP = 36;
const CELL_GAP = 26;
const BAND_PAD = 16;
const LABEL_H = 34;

const scene = {
  size: { w: 1200, h: 800 },
  bands: [],
  cards: [],
  stageTitles: [],
  pills: [],
  pillByJob: new Map(),
  edges: [],
  labels: [],
};

function buildScene() {
  // --- per-card content layout (local coordinates) ---
  const locals = new Map(); // pid -> {w, h, pills:[], titles:[]}
  for (const p of G.pipelines) {
    if (p.unresolved) {
      const title =
        (p.kind === "dynamic_child" ? "dynamic child pipeline" : "unresolved") +
        " — " +
        p.unresolved.reason.replaceAll("_", " ");
      const lines = wrapText(p.unresolved.detail || "", F.small, 296, 3);
      locals.set(p.id, {
        w: 320,
        h: HEAD_H + CARD_PAD * 2 + 18 + lines.length * 15 + 6,
        pills: [],
        titles: [],
        unres: { title, lines },
      });
      continue;
    }
    let x = CARD_PAD;
    let maxH = 0;
    const pills = [];
    const titles = [];
    let stageIdx = -1;
    let lastBridgeStage = -1;
    let maxSpan = 0;
    let sameStageNeeds = false;
    let anyNeeds = false;
    let wrappedStage = false;
    const stageIdxByName = new Map();
    for (const st of p.stages) {
      // one pill per base job; expansions collapse into a ×N badge
      const seen = new Map();
      for (const j of p.jobs) {
        if (j.stage !== st) continue;
        const base = payloadJob(j);
        const e = seen.get(base.id);
        if (e) e.count++;
        else seen.set(base.id, { job: base, count: 1 });
      }
      if (!seen.size) continue;
      const items = [...seen.values()];
      let pillW = 70;
      for (const it of items) {
        const name = it.job.base_name || it.job.name;
        const extras =
          (it.count > 1 ? textW("×" + it.count, F.badge) + 6 : 0) +
          (it.job.trigger ? 13 : 0) +
          (it.job.when === "manual" ? 13 : 0);
        pillW = Math.max(pillW, Math.min(240, textW(name, F.pill) + 18 + extras));
      }
      const nCols = Math.ceil(items.length / SUBCOL_WRAP);
      if (nCols > 1) wrappedStage = true;
      const perCol = Math.ceil(items.length / nCols);
      const stageW = nCols * pillW + (nCols - 1) * SUBCOL_GAP;
      stageIdx++;
      stageIdxByName.set(st, stageIdx);
      titles.push({ x, y: HEAD_H, text: st, w: stageW });
      items.forEach((it, i) => {
        const cx = x + Math.floor(i / perCol) * (pillW + SUBCOL_GAP);
        const cy = HEAD_H + STAGE_TITLE_H + (i % perCol) * (PILL_H + PILL_GAP);
        const name = it.job.base_name || it.job.name;
        const badge = it.count > 1 ? "×" + it.count : "";
        const icons = (it.job.trigger ? "▶" : "") + (it.job.when === "manual" ? "✋" : "");
        const extras = (badge ? textW(badge, F.badge) + 6 : 0) + (icons ? icons.length * 13 : 0);
        if (it.job.trigger) lastBridgeStage = Math.max(lastBridgeStage, stageIdx);
        pills.push({
          id: it.job.id,
          name,
          text: fitText(name, F.pill, pillW - 16 - extras),
          badge,
          icons,
          count: it.count,
          x: cx,
          y: cy,
          w: pillW,
          h: PILL_H,
          stageIdx,
          trigger: !!it.job.trigger,
          outcome: null,
          dim: false,
        });
        maxH = Math.max(maxH, cy + PILL_H - HEAD_H);
      });
      x += stageW + STAGE_GAP;
    }
    // A bottom corridor is reserved when edges must travel past columns:
    // long-span needs, or a trigger bridge that is not in the last stage.
    for (const j of p.jobs) {
      const jStage = stageIdxByName.get(j.stage);
      if (jStage === undefined) continue;
      for (const n of j.needs || []) {
        if (n.kind !== "normal") continue;
        const t = p.jobs.find((x) => x.name === n.job);
        if (!t) continue;
        const tStage = stageIdxByName.get(t.stage);
        if (tStage !== undefined) {
          anyNeeds = true;
          maxSpan = Math.max(maxSpan, jStage - tStage);
          if (tStage === jStage) sameStageNeeds = true;
        }
      }
    }
    const needsCorridor =
      maxSpan > 1 ||
      sameStageNeeds ||
      (anyNeeds && wrappedStage) ||
      lastBridgeStage >= 0;
    locals.set(p.id, {
      w: Math.max(220, x - STAGE_GAP + CARD_PAD),
      h: HEAD_H + maxH + CARD_PAD + (needsCorridor ? 18 : 0),
      pills,
      titles,
      corridor: needsCorridor,
    });
  }

  // --- board layout: columns by trigger depth, rows by project ---
  const projects = [];
  const byProject = new Map();
  for (const p of G.pipelines) {
    const key = p.project.host + "/" + p.project.path;
    if (!byProject.has(key)) {
      byProject.set(key, []);
      projects.push(key);
    }
    byProject.get(key).push(p);
  }
  const maxDepth = Math.max(...G.pipelines.map((p) => p.depth));

  // A cell = every pipeline of one project at one trigger depth. Big cells
  // (gitlab's 50+ children) wrap into a grid of sub-columns instead of one
  // endless stack; the gaps between sub-columns are reserved for edge routing.
  const CELL_COL_GAP = 34;
  const cellsOf = new Map(); // "project|depth" -> layout
  for (const key of projects) {
    for (const p of byProject.get(key)) {
      const ck = key + "|" + p.depth;
      if (!cellsOf.has(ck)) cellsOf.set(ck, { list: [] });
      cellsOf.get(ck).list.push(p);
    }
  }
  for (const cell of cellsOf.values()) {
    const n = cell.list.length;
    const nCols = Math.max(1, Math.min(4, Math.ceil(n / 10)));
    const perCol = Math.ceil(n / nCols);
    cell.nCols = nCols;
    cell.perCol = perCol;
    cell.colWs = [];
    cell.colHs = [];
    for (let c = 0; c < nCols; c++) {
      const part = cell.list.slice(c * perCol, (c + 1) * perCol);
      cell.colWs.push(Math.max(...part.map((p) => locals.get(p.id).w)));
      cell.colHs.push(
        part.reduce((a, p) => a + locals.get(p.id).h, 0) + (part.length - 1) * CELL_GAP
      );
    }
    cell.w = cell.colWs.reduce((a, b) => a + b, 0) + (nCols - 1) * CELL_COL_GAP;
    cell.h = Math.max(...cell.colHs);
  }

  const colW = [];
  for (let d = 0; d <= maxDepth; d++) {
    let w = 160;
    for (const [ck, cell] of cellsOf)
      if (ck.endsWith("|" + d)) w = Math.max(w, cell.w);
    colW.push(w);
  }
  const colX = [];
  let x = 30;
  for (let d = 0; d <= maxDepth; d++) {
    colX.push(x);
    x += colW[d] + COL_GAP;
  }

  let y = 46;
  let maxRight = 0;
  const cardPos = new Map();
  const cardCell = new Map(); // pipeline id -> {colIdx, cellTop, colX0}
  for (const key of projects) {
    const list = byProject.get(key);
    const cells = new Map();
    for (const p of list) {
      if (!cells.has(p.depth)) cells.set(p.depth, []);
      cells.get(p.depth).push(p);
    }
    let bandH = 0;
    for (const [d] of cells) bandH = Math.max(bandH, cellsOf.get(key + "|" + d).h);
    const contentTop = y + LABEL_H;
    for (const [d, cell] of cells) {
      const c = cellsOf.get(key + "|" + d);
      const cellTop = contentTop + (bandH - c.h) / 2;
      let cx = colX[d];
      for (let ci = 0; ci < c.nCols; ci++) {
        const part = cell.slice(ci * c.perCol, (ci + 1) * c.perCol);
        let cy = cellTop + (c.h - c.colHs[ci]) / 2;
        for (const p of part) {
          cardPos.set(p.id, { x: cx, y: cy });
          cardCell.set(p.id, { colIdx: ci, cellTop, colX0: cx });
          cy += locals.get(p.id).h + CELL_GAP;
        }
        cx += c.colWs[ci] + CELL_COL_GAP;
      }
    }
    const depths = [...cells.keys()];
    const dMin = Math.min(...depths);
    const dMax = Math.max(...depths);
    const left = colX[dMin] - BAND_PAD;
    const right = colX[dMax] + colW[dMax] + BAND_PAD;
    scene.bands.push({
      x: left,
      y,
      w: right - left,
      h: bandH + LABEL_H + BAND_PAD,
      label: key,
    });
    maxRight = Math.max(maxRight, right);
    y += bandH + LABEL_H + BAND_PAD + BAND_GAP;
  }
  scene.size = { w: maxRight + 40, h: y + 10 };
  scene.colX = colX;

  // --- flatten to world coordinates ---
  for (const p of G.pipelines) {
    const loc = locals.get(p.id);
    const pos = cardPos.get(p.id);
    const cardIdx = scene.cards.length;
    const cc = cardCell.get(p.id) || { colIdx: 0, cellTop: pos.y, colX0: pos.x };
    const card = {
      p,
      x: pos.x,
      y: pos.y,
      w: loc.w,
      h: loc.h,
      cellColIdx: cc.colIdx,
      cellTop: cc.cellTop,
      unres: loc.unres || null,
      status: "on",
      dim: false,
      stageBounds: (loc.titles || []).map((t) => ({
        x0: pos.x + t.x,
        x1: pos.x + t.x + t.w,
      })),
      corridorY: loc.corridor ? pos.y + loc.h - 7 : null,
    };
    scene.cards.push(card);
    for (const t of loc.titles)
      scene.stageTitles.push({ x: pos.x + t.x, y: pos.y + t.y, text: t.text, w: t.w });
    for (const pl of loc.pills) {
      pl.x += pos.x;
      pl.y += pos.y;
      pl.cardIdx = cardIdx;
      pl.idx = scene.pills.length;
      scene.pillByJob.set(pl.id, scene.pills.length);
      scene.pills.push(pl);
    }
  }

  // The grid indexes pills only and must exist before edges are routed:
  // the label allocator consults it to keep labels off pills.
  buildGrid();
  buildEdges();
}

/* ---- edges: tessellated béziers with arc length for dashes & pulses ---- */

function cubicPts(x1, y1, cx1, cy1, cx2, cy2, x2, y2, n, out) {
  for (let i = 0; i <= n; i++) {
    const t = i / n;
    const u = 1 - t;
    out.push({
      x: u * u * u * x1 + 3 * u * u * t * cx1 + 3 * u * t * t * cx2 + t * t * t * x2,
      y: u * u * u * y1 + 3 * u * u * t * cy1 + 3 * u * t * t * cy2 + t * t * t * y2,
    });
  }
  return out;
}
function withDist(pts) {
  let d = 0;
  pts[0].d = 0;
  for (let i = 1; i < pts.length; i++) {
    d += Math.hypot(pts[i].x - pts[i - 1].x, pts[i].y - pts[i - 1].y);
    pts[i].d = d;
  }
  return pts;
}
function needPts(x1, y1, x2, y2) {
  const dx = Math.max(40, Math.abs(x2 - x1) / 2);
  return withDist(cubicPts(x1, y1, x1 + dx, y1, x2 - dx, y2, x2, y2, 20, []));
}

function pillRect(id) {
  const i = scene.pillByJob.get(id);
  return i === undefined ? null : scene.pills[i];
}

function hashStr(str) {
  let h = 2166136261;
  for (let i = 0; i < str.length; i++) {
    h ^= str.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

/** Insert quarter-arcs at the interior corners of an orthogonal polyline. */
function roundedPath(points, r) {
  const out = [{ x: points[0].x, y: points[0].y }];
  for (let i = 1; i < points.length - 1; i++) {
    const a = points[i - 1], b = points[i], c = points[i + 1];
    const d1 = Math.hypot(b.x - a.x, b.y - a.y);
    const d2 = Math.hypot(c.x - b.x, c.y - b.y);
    const rr = Math.min(r, d1 / 2, d2 / 2);
    if (rr < 0.5) {
      out.push({ x: b.x, y: b.y });
      continue;
    }
    const u1 = { x: (b.x - a.x) / d1, y: (b.y - a.y) / d1 };
    const u2 = { x: (c.x - b.x) / d2, y: (c.y - b.y) / d2 };
    const p1 = { x: b.x - u1.x * rr, y: b.y - u1.y * rr };
    const p2 = { x: b.x + u2.x * rr, y: b.y + u2.y * rr };
    // quadratic corner sampled at 4 points
    for (let t = 0; t <= 1; t += 1 / 3) {
      const u = 1 - t;
      out.push({
        x: u * u * p1.x + 2 * u * t * b.x + t * t * p2.x,
        y: u * u * p1.y + 2 * u * t * b.y + t * t * p2.y,
      });
    }
  }
  out.push({ x: points[points.length - 1].x, y: points[points.length - 1].y });
  return withDist(out);
}

/** A vertical routing lane inside gutter g of a card (g = right of stage g). */
function gutterLaneX(card, g, srcId) {
  const b = card.stageBounds;
  const g0 = b[g].x1;
  const g1 = g + 1 < b.length ? b[g + 1].x0 : card.x + card.w - 2;
  const lanes = Math.max(2, Math.floor((g1 - g0 - 6) / 3));
  return g0 + 4 + (hashStr(srcId) % lanes) * 3;
}
function corridorLaneY(card, srcId) {
  return (card.corridorY || card.y + card.h - 7) - (hashStr(srcId) % 3) * 3;
}
/** Is this pill in the last sub-column of its (possibly wrapped) stage? */
function lastSubcol(card, pl) {
  return pl.x + pl.w + SUBCOL_GAP >= card.stageBounds[pl.stageIdx].x1 - 1;
}
/** Vertical lane in the gap immediately right of the pill's sub-column. */
function rightGapX(card, pl, srcId) {
  if (lastSubcol(card, pl)) return gutterLaneX(card, pl.stageIdx, srcId);
  return pl.x + pl.w + 4 + (hashStr(srcId) % 4) * 3;
}
/** Vertical lane in the gap immediately left of the pill's sub-column. */
function leftGapX(card, pl, srcId) {
  const b = card.stageBounds[pl.stageIdx];
  if (pl.x - SUBCOL_GAP <= b.x0 + 1) {
    // first sub-column: use the stage gutter to its left (or the card edge)
    return pl.stageIdx > 0
      ? gutterLaneX(card, pl.stageIdx - 1, srcId)
      : Math.max(card.x + 3, pl.x - 5 - (hashStr(srcId) % 3) * 3);
  }
  return pl.x - 5 - (hashStr(srcId) % 4) * 3;
}

/** Route one intra-card needs edge so it never crosses a pill: drop in the
 * gap beside the source's sub-column, travel along the bottom corridor, rise
 * in the gap beside the target's sub-column. Direct gutter hop when source
 * and target sit on the two sides of the same gutter. */
function routeNeeds(card, a, b) {
  const si = a.stageIdx, ti = b.stageIdx;
  const sy = a.y + a.h / 2, ty = b.y + b.h / 2;
  const sx = a.x + a.w;
  const sGap = rightGapX(card, a, a.id);
  if (ti === si && Math.abs(b.x + b.w - (a.x + a.w)) < 2) {
    // same sub-column: around its right gap into the target's right edge
    return roundedPath(
      [{ x: sx, y: sy }, { x: sGap, y: sy }, { x: sGap, y: ty }, { x: b.x + b.w, y: ty }],
      6
    );
  }
  if (ti === si + 1 && lastSubcol(card, a) && b.x - SUBCOL_GAP <= card.stageBounds[ti].x0 + 1) {
    // both sides of one gutter: the classic short hop
    return roundedPath(
      [{ x: sx, y: sy }, { x: sGap, y: sy }, { x: sGap, y: ty }, { x: b.x, y: ty }],
      6
    );
  }
  // general case: corridor route
  const cy = corridorLaneY(card, a.id);
  if (ti === si) {
    // same stage, different sub-column: enter the target's right edge
    const tGap = b.x + b.w + 4 + (hashStr(a.id) % 4) * 3;
    return roundedPath(
      [{ x: sx, y: sy }, { x: sGap, y: sy }, { x: sGap, y: cy }, { x: tGap, y: cy }, { x: tGap, y: ty }, { x: b.x + b.w, y: ty }],
      6
    );
  }
  const tGap = leftGapX(card, b, a.id);
  return roundedPath(
    [{ x: sx, y: sy }, { x: sGap, y: sy }, { x: sGap, y: cy }, { x: tGap, y: cy }, { x: tGap, y: ty }, { x: b.x, y: ty }],
    6
  );
}

/** The path a bridge pill takes to leave its card at the right edge. */
function exitPath(card, pl) {
  const sy = pl.y + pl.h / 2;
  const sx = pl.x + pl.w;
  const lastStage = card.stageBounds.length - 1;
  if ((pl.stageIdx >= lastStage && lastSubcol(card, pl)) || card.corridorY === null) {
    return [{ x: sx, y: sy }, { x: card.x + card.w, y: sy }];
  }
  const gx = rightGapX(card, pl, pl.id);
  const cy = corridorLaneY(card, pl.id);
  return [{ x: sx, y: sy }, { x: gx, y: sy }, { x: gx, y: cy }, { x: card.x + card.w, y: cy }];
}

/** Waypoints from a channel x into a target card's left edge. Cards in inner
 * cell columns are reached over the top of the cell and down the gutter left
 * of their column, so the path never crosses sibling cards. */
function branchWaypoints(chX, toCard, ty, srcKey) {
  if (toCard.cellColIdx === 0 || toCard.x - chX < 60)
    return [{ x: chX, y: ty }, { x: toCard.x, y: ty }];
  const gx = toCard.x - 8 - (hashStr(srcKey) % 7) * 3;
  const connY = toCard.cellTop - 10 - (hashStr(srcKey) % 4) * 4;
  return [
    { x: chX, y: connY },
    { x: gx, y: connY },
    { x: gx, y: ty },
    { x: toCard.x, y: ty },
  ];
}

/** A vertical channel lane in the empty gap left of the target depth column. */
function channelLaneX(depth, srcId) {
  const cx = scene.colX[Math.min(depth, scene.colX.length - 1)];
  return cx - 30 - (hashStr(srcId) % 12) * 4;
}

function buildEdges() {
  const seenPairs = new Set();
  for (const p of G.pipelines) {
    const card = scene.cards.find((c) => c.p.id === p.id);
    for (const j of p.jobs) {
      const jBase = payloadJob(j);
      for (const n of j.needs || []) {
        if (n.kind !== "normal") continue;
        const target = jobById(p.id + "/" + n.job);
        if (!target) continue;
        const tBase = payloadJob(target);
        if (tBase.id === jBase.id) continue;
        const key = p.id + "|" + tBase.id + "|" + jBase.id;
        if (seenPairs.has(key)) continue;
        seenPairs.add(key);
        const a = pillRect(tBase.id);
        const b = pillRect(jBase.id);
        if (!a || !b || !card) continue;
        scene.edges.push({
          pts: routeNeeds(card, a, b),
          cls: "needs",
          kind: "needs",
          opt: !!n.optional,
          cond: false,
          dim: false,
          fromJob: tBase.id,
          toJob: jBase.id,
          fromPill: a.idx,
          toPill: b.idx,
          labelIdx: null,
          noArrow: false,
        });
      }
    }
  }

  // Trigger edges: bundle every bridge's fan-out into one trunk + bus in the
  // channel left of the target column, with a single ×N label.
  const placedLabels = [];
  const groups = new Map(); // source pill idx -> [edge]
  const cycles = [];
  for (const e of G.trigger_edges) {
    const bridge = jobById(e.from_job);
    const from = bridge ? pillRect(payloadJob(bridge).id) : null;
    const toCard = scene.cards.find((c) => c.p.id === e.to_pipeline);
    if (!from || !toCard) continue;
    if (e.cycle || toCard.x < from.x) {
      cycles.push({ e, bridge, from, toCard });
      continue;
    }
    if (!groups.has(from.idx)) groups.set(from.idx, []);
    groups.get(from.idx).push({ e, bridge, toCard });
  }

  const placeLabel = (px, py, lines, srcPill) => {
    const width = Math.max(...lines.map((l) => textW(l, F.label))) + 12;
    const height = lines.length * 12 + 6;
    const clampX = (x) => Math.min(Math.max(x, width / 2 + 4), scene.size.w - width / 2 - 4);
    const clampY = (yy) => Math.min(Math.max(yy, 14), scene.size.h - height);
    const hitsPill = (x0, y0, x1, y1) => {
      for (let cx = Math.floor(x0 / GRID_CELL); cx <= Math.floor(x1 / GRID_CELL); cx++)
        for (let cy = Math.floor(y0 / GRID_CELL); cy <= Math.floor(y1 / GRID_CELL); cy++)
          for (const i of grid.get(cx + "," + cy) || []) {
            const p = scene.pills[i];
            if (x0 < p.x + p.w + 2 && x1 > p.x - 2 && y0 < p.y + p.h + 2 && y1 > p.y - 2)
              return true;
          }
      return false;
    };
    const collides = (x, yy) => {
      if (
        placedLabels.some(
          (b) =>
            Math.abs(x - b.x) < (width + b.w) / 2 + 4 &&
            yy < b.y + b.h + 3 &&
            yy + height > b.y - 3
        )
      )
        return true;
      const x0 = x - width / 2, x1 = x + width / 2;
      if (hitsPill(x0, yy, x1, yy + height)) return true;
      // card headers carry text; keep labels off them
      for (const cd of scene.cards) {
        if (x0 < cd.x + cd.w && x1 > cd.x && yy < cd.y + HEAD_H && yy + height > cd.y)
          return true;
      }
      return false;
    };
    // Search outward: vertical slots sized to this label, then sideways.
    const stepY = height + 6;
    let bx = clampX(px);
    let by = clampY(py);
    let found = false;
    outer: for (const dx of [0, -50, 50, -100, 100, -150, 150]) {
      const x = clampX(px + dx);
      for (let k = 0; k <= 60; k++) {
        const dy = (k % 2 ? 1 : -1) * Math.ceil(k / 2) * stepY;
        const yy = clampY(py + dy);
        if (!collides(x, yy)) {
          bx = x;
          by = yy;
          found = true;
          break outer;
        }
      }
    }
    placedLabels.push({ x: bx, y: by, w: width, h: height });
    scene.labels.push({
      x: bx, y: by, w: width, h: height, lines, dim: false,
      srcPill: srcPill === undefined ? null : srcPill,
      edgeIdxs: [],
    });
    return scene.labels.length - 1;
  };
  const linkLabel = (li, ei) => {
    if (li !== null && li !== undefined) scene.labels[li].edgeIdxs.push(ei);
  };

  // Merge bridges that share a card, a label and a target column into one
  // bundle: per-source feeders -> a vertical bus -> per-target branches, with
  // a single xN label. gitlab's 37 per-gem bridges collapse into one bus.
  const bundles = new Map();
  for (const [pillIdx, list] of groups) {
    const from = scene.pills[pillIdx];
    for (const g of list) {
      const style = triggerStyle(g.bridge, g.e);
      const key =
        from.cardIdx + "|" + (style.label || "") + "|" + g.toCard.p.depth;
      if (!bundles.has(key))
        bundles.set(key, { sources: new Map(), targets: [], style, depth: g.toCard.p.depth });
      const b = bundles.get(key);
      b.sources.set(pillIdx, from);
      b.targets.push({ ...g, fromPill: pillIdx, ty: g.toCard.y + Math.min(26, g.toCard.h / 2) });
    }
  }

  for (const [key, b] of bundles) {
    const chX = channelLaneX(b.depth, key);
    const exits = new Map(); // pillIdx -> {pts, ey}
    for (const [pi, from] of b.sources) {
      const card = scene.cards[from.cardIdx];
      const exit = exitPath(card, from);
      exits.set(pi, { exit, ey: exit[exit.length - 1].y, ex: exit[exit.length - 1].x });
    }
    const bundled = b.targets.length >= 3 && [...exits.values()].every((x) => chX > x.ex + 12);
    if (!bundled) {
      for (const g of b.targets.sort((a, c) => a.ty - c.ty)) {
        const { exit, ey, ex } = exits.get(g.fromPill);
        const st = triggerStyle(g.bridge, g.e);
        const pts =
          chX > ex + 12
            ? roundedPath(
                exit.concat(
                  [{ x: chX, y: ey }],
                  branchWaypoints(chX, g.toCard, g.ty, g.e.to_pipeline)
                ),
                8
              )
            : withDist(
                cubicPts(ex, ey, ex + 60, ey, g.toCard.x - 60, g.ty, g.toCard.x, g.ty, 20, [])
              );
        const lines = ["\u25b6 " + truncate(scene.pills[g.fromPill].name, 26)].concat(
          st.label ? st.label.split(" · ") : []
        );
        const labelIdx = placeLabel(
          (ex + Math.max(chX, ex + 80)) / 2, ey - 12, lines, g.fromPill
        );
        scene.edges.push({
          pts, cls: "trig", kind: "trig", opt: false, cond: st.cond, dim: false,
          fromJob: g.e.from_job, toJob: null, toPipeline: g.e.to_pipeline,
          fromPill: g.fromPill, toPill: null, labelIdx, noArrow: false,
        });
        linkLabel(labelIdx, scene.edges.length - 1);
      }
      continue;
    }

    const branchPts = b.targets.map((g) =>
      branchWaypoints(chX, g.toCard, g.ty, g.e.to_pipeline)
    );
    const attachYs = branchPts.map((wp) => wp[0].y);
    const eys = [...exits.values()].map((x) => x.ey);
    const yMin = Math.min(...attachYs, ...eys);
    const yMax = Math.max(...attachYs, ...eys);
    const allPills = [...b.sources.keys()];
    // feeders: each bridge joins the bus at its exit height
    const feederIdxs = [];
    for (const [pi, x] of exits) {
      scene.edges.push({
        pts: roundedPath(x.exit.concat([{ x: chX, y: x.ey }]), 8),
        cls: "trig", kind: "trig", opt: false, cond: b.style.cond, dim: false,
        fromJob: scene.pills[pi].id, toJob: null, toPipeline: null,
        fromPill: pi, toPill: null, labelIdx: null, noArrow: true,
      });
      feederIdxs.push(scene.edges.length - 1);
    }
    // the bus itself, carrying the single xN label named after its bridges
    const names = [...b.sources.values()].map((pl) => pl.name);
    let prefix = names[0];
    for (const n of names) {
      let k = 0;
      while (k < prefix.length && k < n.length && prefix[k] === n[k]) k++;
      prefix = prefix.slice(0, k);
    }
    prefix = prefix.trim();
    const nameLine =
      names.length === 1
        ? "\u25b6 " + truncate(names[0], 26)
        : prefix.length >= 4
          ? "\u25b6 " + truncate(prefix, 22) + "\u2026"
          : "\u25b6 " + names.length + " trigger jobs";
    const lines = [nameLine].concat(
      (b.style.label ? b.style.label + " · " : "").split(" · ").filter(Boolean)
    );
    lines.push("×" + b.targets.length);
    const midEx = [...exits.values()][0].ex;
    const labelIdx = placeLabel(
      (midEx + chX) / 2, Math.min(...eys) - 12, lines, b.targets[0].fromPill
    );
    for (const fi of feederIdxs) linkLabel(labelIdx, fi);
    scene.edges.push({
      pts: withDist([{ x: chX, y: yMin, d: 0 }, { x: chX, y: yMax }]),
      cls: "trig", kind: "trig", opt: false, cond: b.style.cond, dim: false,
      fromJob: b.targets[0].e.from_job, toJob: null, toPipeline: null,
      fromPill: b.targets[0].fromPill, busPills: allPills, toPill: null,
      labelIdx, noArrow: true,
    });
    linkLabel(labelIdx, scene.edges.length - 1);
    // branches into each downstream card
    b.targets.forEach((g, ti) => {
      scene.edges.push({
        pts: roundedPath(branchPts[ti], 8),
        cls: "trig", kind: "trig", opt: false, cond: b.style.cond, dim: false,
        fromJob: g.e.from_job, toJob: null, toPipeline: g.e.to_pipeline,
        fromPill: g.fromPill, toPill: null, labelIdx: null, noArrow: false,
      });
      linkLabel(labelIdx, scene.edges.length - 1);
    });
  }

  // Backward edges (cycles): swing above the cards, as before.
  for (const { e, bridge, from, toCard } of cycles) {
    const fromX = from.x + from.w;
    const fromY = from.y + from.h / 2;
    const toX = toCard.x + toCard.w;
    const toY = toCard.y + Math.min(26, toCard.h / 2);
    const lift = Math.max(14, Math.min(from.y, toCard.y) - 44);
    const mx = (fromX + toX) / 2;
    const p1 = cubicPts(fromX, fromY, fromX + 90, fromY, fromX + 90, lift, mx, lift, 14, []);
    cubicPts(mx, lift, 2 * mx - fromX - 90, lift, toX + 90, toY, toX, toY, 14, p1);
    const pts = withDist(p1);
    const style = triggerStyle(bridge, e);
    const total = pts[pts.length - 1].d;
    let pi = 0;
    while (pi < pts.length - 1 && pts[pi].d < total * 0.5) pi++;
    const lines = ["\u25b6 " + truncate(from.name, 26)].concat(
      style.label ? style.label.split(" · ") : []
    );
    const labelIdx = placeLabel(pts[pi].x, pts[pi].y - 12, lines, from.idx);
    scene.edges.push({
      pts, cls: "trig", kind: "cycle", opt: false, cond: style.cond, dim: false,
      fromJob: e.from_job, toJob: null, toPipeline: e.to_pipeline,
      fromPill: from.idx, toPill: null, labelIdx, noArrow: false,
    });
    linkLabel(labelIdx, scene.edges.length - 1);
  }

  buildAdjacency();
}

/* ---- adjacency + lineage tracing ---- */

const adj = { out: new Map(), inn: new Map(), trig: new Map(), byTarget: new Map() };
const pipeById = new Map(G.pipelines.map((p) => [p.id, p]));
/** First (lowest-order) include edge into each file, per pipeline. */
const incomingInclude = new Map();
const incomingIncludeCount = new Map();
for (const e of G.include_edges) {
  const k = e.pipeline + "|" + e.to;
  incomingIncludeCount.set(k, (incomingIncludeCount.get(k) || 0) + 1);
  const cur = incomingInclude.get(k);
  if (!cur || e.order < cur.order) incomingInclude.set(k, e);
}

function buildAdjacency() {
  scene.edges.forEach((e, i) => {
    if (e.cls === "needs") {
      if (!adj.out.has(e.fromPill)) adj.out.set(e.fromPill, []);
      adj.out.get(e.fromPill).push(i);
      if (!adj.inn.has(e.toPill)) adj.inn.set(e.toPill, []);
      adj.inn.get(e.toPill).push(i);
    } else if (e.fromPill !== undefined && e.fromPill !== null) {
      for (const pi of e.busPills || [e.fromPill]) {
        if (!adj.trig.has(pi)) adj.trig.set(pi, []);
        adj.trig.get(pi).push(i);
      }
      if (e.toPipeline) {
        if (!adj.byTarget.has(e.toPipeline)) adj.byTarget.set(e.toPipeline, []);
        adj.byTarget.get(e.toPipeline).push(i);
      }
    }
  });
}

/** Transitive closure both directions, plus the trigger fans it feeds. */
function traceLineage(pillIdx) {
  const pills = new Set([pillIdx]);
  const edges = new Set();
  for (const [dir, map] of [["down", adj.out], ["up", adj.inn]]) {
    const stack = [pillIdx];
    while (stack.length) {
      const p = stack.pop();
      for (const ei of map.get(p) || []) {
        edges.add(ei);
        const nxt = dir === "down" ? scene.edges[ei].toPill : scene.edges[ei].fromPill;
        if (!pills.has(nxt)) {
          pills.add(nxt);
          stack.push(nxt);
        }
      }
    }
  }
  for (const p of pills) for (const ei of adj.trig.get(p) || []) edges.add(ei);

  // Walk UP through trigger edges: light the bridge that invokes this job's
  // pipeline, that bridge's own upstream needs, and so on to the root — the
  // "how does this job get invoked" path.
  const seenPids = new Set();
  let pidStack = [pipeOfJob.get(scene.pills[pillIdx].id).id];
  let guard = 0;
  while (pidStack.length && guard++ < 40) {
    const pid = pidStack.pop();
    if (seenPids.has(pid)) continue;
    seenPids.add(pid);
    for (const ei of adj.byTarget.get(pid) || []) {
      const e = scene.edges[ei];
      edges.add(ei);
      const bp = e.fromPill;
      if (bp === undefined || bp === null || pills.has(bp)) continue;
      pills.add(bp);
      // the bridge's feeder + bus segments
      for (const ti of adj.trig.get(bp) || []) {
        const te = scene.edges[ti];
        if (!te.toPipeline || te.toPipeline === pid) edges.add(ti);
      }
      // the bridge's own upstream needs closure
      const st = [bp];
      while (st.length) {
        const q = st.pop();
        for (const ni of adj.inn.get(q) || []) {
          edges.add(ni);
          const nxt = scene.edges[ni].fromPill;
          if (!pills.has(nxt)) {
            pills.add(nxt);
            st.push(nxt);
          }
        }
      }
      pidStack.push(pipeOfJob.get(scene.pills[bp].id).id);
    }
  }
  return { pills, edges };
}

/** Direct neighbourhood only (used on hover). */
function directEdges(pillIdx) {
  const set = new Set();
  for (const ei of adj.out.get(pillIdx) || []) set.add(ei);
  for (const ei of adj.inn.get(pillIdx) || []) set.add(ei);
  for (const ei of adj.trig.get(pillIdx) || []) set.add(ei);
  return set;
}

function triggerStyle(bridge, e) {
  if (bridge) bridge = payloadJob(bridge);
  const parts = [];
  if (e.strategy) parts.push(e.strategy);
  let cond = false;
  if (bridge) {
    const m = bridge.rules.mode;
    if (m === "manual" || bridge.when === "manual") { cond = true; parts.push("manual"); }
    else if (m === "conditional" || m === "legacy") {
      cond = true;
      const c = (bridge.rules.rules || []).find((r) => r.if);
      parts.push(c ? "if " + truncate(c.if, 30) : "conditional");
    } else if (m === "never") { cond = true; parts.push("never"); }
  }
  if (e.cycle) parts.push("cycle");
  return { cond, label: parts.join(" · ") || "trigger" };
}

/* ---- spatial grid for picking ---- */

const GRID_CELL = 200;
const grid = new Map();
function buildGrid() {
  scene.pills.forEach((pl, i) => {
    const x0 = Math.floor(pl.x / GRID_CELL);
    const x1 = Math.floor((pl.x + pl.w) / GRID_CELL);
    const y0 = Math.floor(pl.y / GRID_CELL);
    const y1 = Math.floor((pl.y + pl.h) / GRID_CELL);
    for (let cx = x0; cx <= x1; cx++)
      for (let cy = y0; cy <= y1; cy++) {
        const k = cx + "," + cy;
        if (!grid.has(k)) grid.set(k, []);
        grid.get(k).push(i);
      }
  });
}
function pickLabel(wx, wy) {
  for (let i = scene.labels.length - 1; i >= 0; i--) {
    const l = scene.labels[i];
    if (wx >= l.x - l.w / 2 && wx <= l.x + l.w / 2 && wy >= l.y - 10 && wy <= l.y - 10 + l.h)
      return i;
  }
  return -1;
}
function pickPill(wx, wy) {
  const k = Math.floor(wx / GRID_CELL) + "," + Math.floor(wy / GRID_CELL);
  const cell = grid.get(k);
  if (!cell) return -1;
  for (let i = cell.length - 1; i >= 0; i--) {
    const pl = scene.pills[cell[i]];
    if (wx >= pl.x && wx <= pl.x + pl.w && wy >= pl.y && wy <= pl.y + pl.h)
      return cell[i];
  }
  return -1;
}

/* ================= renderers ================= */

const viewport = h("div", "viewport");
const glCanvas = document.createElement("canvas");
glCanvas.id = "gl-layer";
const txtCanvas = document.createElement("canvas");
txtCanvas.id = "txt-layer";
viewport.appendChild(glCanvas);
viewport.appendChild(txtCanvas);

let view = { scale: 1, tx: 0, ty: 0 };
let vw = 800;
let vh = 600;
let dpr = (typeof devicePixelRatio === "number" && devicePixelRatio) || 1;
let mode = "none"; // webgl2 | canvas2d | none
let gl = null;
let ctx2d = null; // scene context in canvas2d mode
let txtCtx = null;
let hoverIdx = -1;
let hoverLabel = -1;

/* ---- edge visibility state ---- */

let edgeMode = "focus"; // focus | all | triggers
let selLineage = null;  // {pills:Set, edges:Set} for the selected job
let hoverLit = null;    // Set of edge indices around the hovered pill

const EDGE_NORMAL = 0, EDGE_DIM = 1, EDGE_HIDDEN = 2, EDGE_LIT = 3;
function edgeStateOf(e, idx) {
  const lit = (hoverLit && hoverLit.has(idx)) || (selLineage && selLineage.edges.has(idx));
  if (e.cls === "needs") {
    if (edgeMode === "triggers") return EDGE_HIDDEN;
    if (edgeMode === "focus" && !lit) return EDGE_HIDDEN;
  }
  if (lit) return EDGE_LIT;
  if (e.dim) return EDGE_DIM;
  if (selLineage && e.cls === "needs") return EDGE_DIM;
  return EDGE_NORMAL;
}

/* ---- pill / card / edge styling (shared by both renderers) ---- */

function pillStyle(pl) {
  const P = PAL;
  let fill = P.card;
  let stroke = pl.trigger ? P.trig : P.line;
  let strokeW = 1.2;
  let hatch = false;
  let alpha = 1;
  switch (pl.outcome) {
    case "runs":
      stroke = P.ok;
      fill = mix(P.card, P.ok, 0.07);
      break;
    case "manual":
    case "delayed":
      stroke = P.warn;
      fill = mix(P.card, P.warn, 0.07);
      break;
    case "skipped":
      alpha = 0.35;
      break;
    case "blocked":
      stroke = P.err;
      alpha = 0.35;
      break;
    case "unknown":
      stroke = P.muted;
      hatch = true;
      break;
  }
  if (pl.dim) alpha = Math.min(alpha, 0.3);
  if (selLineage && !selLineage.pills.has(pl.idx)) alpha = Math.min(alpha, 0.35);
  if (selectedJob === pl.id) {
    stroke = P.accent;
    strokeW = 2.4;
    alpha = 1;
  } else if (hoverIdx >= 0 && scene.pills[hoverIdx] === pl) {
    strokeW = 2;
  }
  return { fill, stroke, strokeW, hatch, alpha, radius: 7 };
}

function cardStyle(c) {
  const P = PAL;
  let fill = P.card;
  let stroke = P.line;
  let alpha = 1;
  if (c.unres) {
    stroke = c.p.kind === "dynamic_child" ? P.warn : P.err;
    fill = mix(P.card, stroke, 0.05);
  } else if (c.status === "gate") stroke = P.warn;
  if (c.dim) alpha = 0.4;
  return { fill, stroke, strokeW: 1.3, hatch: false, alpha, radius: 10 };
}

function bandStyle() {
  return {
    fill: withA(PAL.panel, 0.55),
    stroke: withA(PAL.line, 0.9),
    strokeW: 1,
    hatch: false,
    alpha: 1,
    radius: 12,
  };
}

function edgeStyle(e) {
  const P = PAL;
  if (e.kind === "needs")
    return { color: withA(P.needs, 0.75), width: 1.6, dash: e.opt ? 7 : 0 };
  if (e.kind === "cycle") return { color: withA(P.err, 0.85), width: 2, dash: 6 };
  return { color: withA(P.trig, 0.85), width: 2.2, dash: e.cond ? 8 : 0 };
}

const STATUS_DOT = { on: "ok", gate: "warn", unknown: "warn", off: "muted" };

/* ---- WebGL2 backend ---- */

const RECT_VS = `#version 300 es
layout(location=0) in vec2 corner;
layout(location=1) in vec4 rect;
layout(location=2) in vec4 fill;
layout(location=3) in vec4 stroke;
layout(location=4) in vec4 params; // radius, strokeW, hatch, alpha
uniform vec3 uView; uniform vec2 uRes;
out vec2 vLocal; out vec2 vSize; out vec4 vFill; out vec4 vStroke; out vec4 vParams; out vec2 vWorld;
void main(){
  vec2 world = rect.xy + corner * rect.zw;
  vec2 screen = world * uView.x + uView.yz;
  vec2 clip = screen / uRes * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
  vLocal = corner * rect.zw; vSize = rect.zw;
  vFill = fill; vStroke = stroke; vParams = params; vWorld = world;
}`;
const RECT_FS = `#version 300 es
precision highp float;
in vec2 vLocal; in vec2 vSize; in vec4 vFill; in vec4 vStroke; in vec4 vParams; in vec2 vWorld;
uniform vec3 uView;
out vec4 outColor;
float sdBox(vec2 p, vec2 b, float r){
  vec2 q = abs(p) - b + r;
  return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
void main(){
  float aa = 1.2 / uView.x;
  vec2 half_ = vSize * 0.5;
  float d = sdBox(vLocal - half_, half_, min(vParams.x, min(half_.x, half_.y)));
  vec4 fill = vFill;
  if (vParams.z > 0.5) {
    float s = mod(vWorld.x + vWorld.y, 14.0);
    if (s < 7.0) fill = mix(fill, vStroke, 0.16);
  }
  float inside = 1.0 - smoothstep(-aa, aa, d);
  float body = 1.0 - smoothstep(-aa, aa, d + vParams.y);
  vec4 col = mix(vStroke, fill, body);
  outColor = vec4(col.rgb, col.a * inside * vParams.w);
}`;
const EDGE_VS = `#version 300 es
layout(location=0) in vec2 pos;
layout(location=1) in vec2 norm;   // pre-scaled to half-width in px
layout(location=2) in float dist;
layout(location=3) in vec4 color;
layout(location=4) in vec4 style;  // dash period px, state, class, arrow
uniform vec3 uView; uniform vec2 uRes;
out float vDist; out vec4 vColor; out vec4 vStyle;
void main(){
  float boost = style.y > 2.5 ? 1.35 : 1.0; // lit edges draw wider
  vec2 world = pos + norm * boost / uView.x;
  vec2 screen = world * uView.x + uView.yz;
  vec2 clip = screen / uRes * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
  vDist = dist; vColor = color; vStyle = style;
}`;
const EDGE_FS = `#version 300 es
precision highp float;
in float vDist; in vec4 vColor; in vec4 vStyle;
uniform float uTime; uniform float uPulse; uniform float uEdgeMode; uniform float uScale;
out vec4 outColor;
void main(){
  float st = vStyle.y;
  if (st > 1.5 && st < 2.5) discard; // hidden
  float alpha = vColor.a;
  if (vStyle.x > 0.0) alpha *= 0.25 + 0.75 * step(fract(vDist / vStyle.x), 0.6);
  if (uPulse > 0.5 && (st < 0.5 || st > 2.5)) {
    float p = fract(vDist / 170.0 - uTime * 0.65);
    float g = smoothstep(0.0, 0.12, p) * (1.0 - smoothstep(0.12, 0.34, p));
    alpha = min(1.0, alpha + g * 0.6);
  }
  if (st > 0.5 && st < 1.5) alpha *= 0.12;       // dim
  if (st > 2.5) alpha = min(1.0, alpha * 1.4);    // lit
  // "all" mode: un-highlighted needs edges are faint texture, and the whole
  // needs mesh fades out at overview zoom where it cannot be read anyway
  if (vStyle.z > 0.5 && uEdgeMode > 0.5) {
    if (st < 0.5) alpha *= 0.5;
    alpha *= smoothstep(0.05, 0.16, uScale);
  }
  outColor = vec4(vColor.rgb, alpha);
}`;

const glState = {
  rectProg: null,
  edgeProg: null,
  rectVaos: {}, // bands | cards | pills
  rectBufs: {},
  rectCounts: {},
  edgeVao: null,
  edgeBufs: null,
  edgeIndexCount: 0,
  edgeStyleArr: null,
  edgeVertRanges: [], // per edge: [firstVert, vertCount]
};

function compile(glc, type, src) {
  const s = glc.createShader(type);
  glc.shaderSource(s, src);
  glc.compileShader(s);
  if (!glc.getShaderParameter(s, glc.COMPILE_STATUS))
    throw new Error(glc.getShaderInfoLog(s) || "shader error");
  return s;
}
function program(glc, vs, fs) {
  const p = glc.createProgram();
  glc.attachShader(p, compile(glc, glc.VERTEX_SHADER, vs));
  glc.attachShader(p, compile(glc, glc.FRAGMENT_SHADER, fs));
  glc.linkProgram(p);
  if (!glc.getProgramParameter(p, glc.LINK_STATUS))
    throw new Error(glc.getProgramInfoLog(p) || "link error");
  return p;
}

function initGL() {
  gl = glCanvas.getContext("webgl2", { alpha: false, antialias: true });
  if (!gl) return false;
  glState.rectProg = program(gl, RECT_VS, RECT_FS);
  glState.edgeProg = program(gl, EDGE_VS, EDGE_FS);

  const quad = new Float32Array([0, 0, 1, 0, 0, 1, 1, 1]);
  const quadBuf = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, quadBuf);
  gl.bufferData(gl.ARRAY_BUFFER, quad, gl.STATIC_DRAW);

  for (const group of ["bands", "cards", "pills"]) {
    const vao = gl.createVertexArray();
    gl.bindVertexArray(vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, quadBuf);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    const inst = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, inst);
    const stride = 16 * 4;
    const attrs = [
      [1, 4, 0],
      [2, 4, 4],
      [3, 4, 8],
      [4, 4, 12],
    ];
    for (const [loc, size, off] of attrs) {
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, size, gl.FLOAT, false, stride, off * 4);
      gl.vertexAttribDivisor(loc, 1);
    }
    glState.rectVaos[group] = vao;
    glState.rectBufs[group] = inst;
    glState.rectCounts[group] = 0;
  }

  buildEdgeGeometry();
  glState.locs = {
    rView: gl.getUniformLocation(glState.rectProg, "uView"),
    rRes: gl.getUniformLocation(glState.rectProg, "uRes"),
    eView: gl.getUniformLocation(glState.edgeProg, "uView"),
    eRes: gl.getUniformLocation(glState.edgeProg, "uRes"),
    eTime: gl.getUniformLocation(glState.edgeProg, "uTime"),
    ePulse: gl.getUniformLocation(glState.edgeProg, "uPulse"),
    eMode: gl.getUniformLocation(glState.edgeProg, "uEdgeMode"),
    eScale: gl.getUniformLocation(glState.edgeProg, "uScale"),
  };
  gl.enable(gl.BLEND);
  gl.blendFuncSeparate(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA, gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
  return true;
}

function buildEdgeGeometry() {
  const pos = [];
  const norm = [];
  const dist = [];
  const color = [];
  const style = [];
  const idx = [];
  glState.edgeVertRanges = [];
  for (const e of scene.edges) {
    const st = edgeStyle(e);
    const firstVert = pos.length / 2;
    const pts = e.pts;
    for (let i = 0; i < pts.length; i++) {
      const prev = pts[Math.max(0, i - 1)];
      const next = pts[Math.min(pts.length - 1, i + 1)];
      let nx = -(next.y - prev.y);
      let ny = next.x - prev.x;
      const len = Math.hypot(nx, ny) || 1;
      nx = (nx / len) * (st.width / 2);
      ny = (ny / len) * (st.width / 2);
      pos.push(pts[i].x, pts[i].y, pts[i].x, pts[i].y);
      norm.push(nx, ny, -nx, -ny);
      dist.push(pts[i].d, pts[i].d);
      color.push(...st.color, ...st.color);
      const cls = e.cls === "needs" ? 1 : 0;
      style.push(st.dash, 0, cls, 0, st.dash, 0, cls, 0);
      if (i > 0) {
        const b = firstVert + i * 2;
        idx.push(b - 2, b - 1, b, b, b - 1, b + 1);
      }
    }
    if (!e.noArrow) {
      // arrowhead at the end
      const last = pts[pts.length - 1];
      const back = pts[pts.length - 2] || last;
      let dx = last.x - back.x;
      let dy = last.y - back.y;
      const dl = Math.hypot(dx, dy) || 1;
      dx /= dl;
      dy /= dl;
      const hw = st.width * 1.9 + 2.5;
      const hl = st.width * 2.6 + 5;
      const bx = last.x - dx * hl;
      const by = last.y - dy * hl;
      const av = pos.length / 2;
      pos.push(last.x, last.y, bx - dy * hw, by + dx * hw, bx + dy * hw, by - dx * hw);
      norm.push(0, 0, 0, 0, 0, 0);
      dist.push(last.d, last.d, last.d);
      color.push(...st.color, ...st.color, ...st.color);
      const cls = e.cls === "needs" ? 1 : 0;
      style.push(0, 0, cls, 1, 0, 0, cls, 1, 0, 0, cls, 1);
      idx.push(av, av + 1, av + 2);
    }
    glState.edgeVertRanges.push([firstVert, pos.length / 2 - firstVert]);
  }
  const vao = gl.createVertexArray();
  gl.bindVertexArray(vao);
  const mk = (data, loc, size) => {
    const b = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, b);
    gl.bufferData(gl.ARRAY_BUFFER, data, gl.STATIC_DRAW);
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, size, gl.FLOAT, false, 0, 0);
    return b;
  };
  glState.edgeBufs = {
    pos: mk(new Float32Array(pos), 0, 2),
    norm: mk(new Float32Array(norm), 1, 2),
    dist: mk(new Float32Array(dist), 2, 1),
    color: mk(new Float32Array(color), 3, 4),
    style: mk(new Float32Array(style), 4, 4),
  };
  glState.edgeStyleArr = new Float32Array(style);
  const ib = gl.createBuffer();
  gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, ib);
  gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, new Uint32Array(idx), gl.STATIC_DRAW);
  glState.edgeIndexCount = idx.length;
  glState.edgeVao = vao;
}

function rectInstances(list, styleFn) {
  const arr = new Float32Array(list.length * 16);
  list.forEach((item, i) => {
    const s = styleFn(item);
    arr.set(
      [
        item.x, item.y, item.w, item.h,
        s.fill[0], s.fill[1], s.fill[2], s.fill[3],
        s.stroke[0], s.stroke[1], s.stroke[2], s.stroke[3],
        s.radius, s.strokeW, s.hatch ? 1 : 0, s.alpha,
      ],
      i * 16
    );
  });
  return arr;
}

function uploadRects() {
  if (!gl) return;
  const groups = [
    ["bands", scene.bands, bandStyle],
    ["cards", scene.cards, cardStyle],
    ["pills", scene.pills, pillStyle],
  ];
  for (const [name, list, fn] of groups) {
    gl.bindBuffer(gl.ARRAY_BUFFER, glState.rectBufs[name]);
    gl.bufferData(gl.ARRAY_BUFFER, rectInstances(list, fn), gl.DYNAMIC_DRAW);
    glState.rectCounts[name] = list.length;
  }
}

function uploadEdgeState() {
  if (!gl || !glState.edgeStyleArr) return;
  const arr = glState.edgeStyleArr;
  scene.edges.forEach((e, i) => {
    const st = edgeStyle(e);
    const state = edgeStateOf(e, i);
    const cls = e.cls === "needs" ? 1 : 0;
    const [first, count] = glState.edgeVertRanges[i];
    const arrowFrom = e.noArrow ? count : count - 3;
    for (let v = 0; v < count; v++) {
      const o = (first + v) * 4;
      arr[o] = v >= arrowFrom ? 0 : st.dash;
      arr[o + 1] = state;
      arr[o + 2] = cls;
      arr[o + 3] = v >= arrowFrom ? 1 : 0;
    }
  });
  gl.bindBuffer(gl.ARRAY_BUFFER, glState.edgeBufs.style);
  gl.bufferData(gl.ARRAY_BUFFER, arr, gl.DYNAMIC_DRAW);
}

function uploadEdgeColors() {
  if (!gl) return;
  const color = [];
  scene.edges.forEach((e, i) => {
    const st = edgeStyle(e);
    const count = glState.edgeVertRanges[i][1];
    for (let v = 0; v < count; v++) color.push(...st.color);
  });
  gl.bindBuffer(gl.ARRAY_BUFFER, glState.edgeBufs.color);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(color), gl.DYNAMIC_DRAW);
}

let pulseTime = 0;
function drawGL() {
  gl.viewport(0, 0, glCanvas.width, glCanvas.height);
  const P = PAL.ground;
  gl.clearColor(P[0], P[1], P[2], 1);
  gl.clear(gl.COLOR_BUFFER_BIT);

  gl.useProgram(glState.rectProg);
  gl.uniform3f(glState.locs.rView, view.scale, view.tx, view.ty);
  gl.uniform2f(glState.locs.rRes, vw, vh);
  for (const group of ["bands", "cards", "pills"]) {
    gl.bindVertexArray(glState.rectVaos[group]);
    gl.drawArraysInstanced(gl.TRIANGLE_STRIP, 0, 4, glState.rectCounts[group]);
  }

  gl.useProgram(glState.edgeProg);
  gl.uniform3f(glState.locs.eView, view.scale, view.tx, view.ty);
  gl.uniform2f(glState.locs.eRes, vw, vh);
  gl.uniform1f(glState.locs.eTime, pulseTime);
  gl.uniform1f(glState.locs.ePulse, pulseOn() ? 1 : 0);
  gl.uniform1f(glState.locs.eMode, edgeMode === "all" ? 1 : 0);
  gl.uniform1f(glState.locs.eScale, view.scale);
  gl.bindVertexArray(glState.edgeVao);
  gl.drawElements(gl.TRIANGLES, glState.edgeIndexCount, gl.UNSIGNED_INT, 0);
  gl.bindVertexArray(null);
}

/* ---- Canvas2D fallback backend ---- */

function roundRectPath(c, x, y, w, h, r) {
  r = Math.min(r, w / 2, h / 2);
  c.beginPath();
  c.moveTo(x + r, y);
  c.arcTo(x + w, y, x + w, y + h, r);
  c.arcTo(x + w, y + h, x, y + h, r);
  c.arcTo(x, y + h, x, y, r);
  c.arcTo(x, y, x + w, y, r);
  c.closePath();
}

function draw2D() {
  const c = ctx2d;
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  c.fillStyle = css(PAL.ground);
  c.fillRect(0, 0, vw, vh);
  c.setTransform(dpr * view.scale, 0, 0, dpr * view.scale, dpr * view.tx, dpr * view.ty);
  const vis = worldViewport(60);
  const drawRect = (item, s) => {
    if (!rectVisible(item, vis)) return;
    c.globalAlpha = s.alpha;
    roundRectPath(c, item.x, item.y, item.w, item.h, s.radius);
    c.fillStyle = css(s.fill);
    c.fill();
    c.lineWidth = s.strokeW;
    c.strokeStyle = css(s.stroke);
    c.stroke();
  };
  for (const b of scene.bands) drawRect(b, bandStyle(b));
  for (const cd of scene.cards) drawRect(cd, cardStyle(cd));
  for (const pl of scene.pills) drawRect(pl, pillStyle(pl));
  c.globalAlpha = 1;
  scene.edges.forEach((e, ei) => {
    const pts = e.pts;
    if (!rectVisible(edgeBounds(e), vis)) return;
    const state = edgeStateOf(e, ei);
    if (state === EDGE_HIDDEN) return;
    const st = edgeStyle(e);
    let a = st.color[3];
    if (state === EDGE_DIM) a *= 0.12;
    if (state === EDGE_LIT) a = Math.min(1, a * 1.4);
    if (e.cls === "needs" && edgeMode === "all") {
      if (state === EDGE_NORMAL) a *= 0.5;
      a *= Math.min(1, Math.max(0, (view.scale - 0.05) / 0.11));
    }
    if (a <= 0.01) return;
    c.globalAlpha = a;
    c.strokeStyle = css(withA(st.color, 1));
    c.lineWidth = st.width;
    c.lineWidth = state === EDGE_LIT ? st.width * 1.35 : st.width;
    c.setLineDash(st.dash ? [st.dash * 0.6, st.dash * 0.4] : []);
    c.beginPath();
    c.moveTo(pts[0].x, pts[0].y);
    for (let i = 1; i < pts.length; i++) c.lineTo(pts[i].x, pts[i].y);
    c.stroke();
    c.setLineDash([]);
    if (e.noArrow) return;
    const last = pts[pts.length - 1];
    const back = pts[pts.length - 2] || last;
    let dx = last.x - back.x, dy = last.y - back.y;
    const dl = Math.hypot(dx, dy) || 1;
    dx /= dl; dy /= dl;
    const hw = st.width * 1.9 + 2.5, hl = st.width * 2.6 + 5;
    c.beginPath();
    c.moveTo(last.x, last.y);
    c.lineTo(last.x - dx * hl - dy * hw, last.y - dy * hl + dx * hw);
    c.lineTo(last.x - dx * hl + dy * hw, last.y - dy * hl - dx * hw);
    c.closePath();
    c.fillStyle = css(withA(st.color, 1));
    c.fill();
  });
  c.globalAlpha = 1;
}

/* ---- shared text overlay ---- */

function worldViewport(pad) {
  return {
    x0: -view.tx / view.scale - pad,
    y0: -view.ty / view.scale - pad,
    x1: (-view.tx + vw) / view.scale + pad,
    y1: (-view.ty + vh) / view.scale + pad,
  };
}
function rectVisible(r, vis) {
  return r.x + r.w >= vis.x0 && r.x <= vis.x1 && r.y + r.h >= vis.y0 && r.y <= vis.y1;
}
function edgeBounds(e) {
  if (!e.bounds) {
    let x0 = 1e9, y0 = 1e9, x1 = -1e9, y1 = -1e9;
    for (const p of e.pts) {
      x0 = Math.min(x0, p.x); y0 = Math.min(y0, p.y);
      x1 = Math.max(x1, p.x); y1 = Math.max(y1, p.y);
    }
    e.bounds = { x: x0, y: y0, w: x1 - x0, h: y1 - y0 };
  }
  return e.bounds;
}

function drawText() {
  if (!txtCtx) return;
  const c = txtCtx;
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  c.clearRect(0, 0, vw, vh);
  c.setTransform(dpr * view.scale, 0, 0, dpr * view.scale, dpr * view.tx, dpr * view.ty);
  const s = view.scale;
  const vis = worldViewport(20);
  c.textBaseline = "alphabetic";

  for (const b of scene.bands) {
    if (!rectVisible(b, vis)) continue;
    c.font = F.band;
    c.fillStyle = css(PAL.muted);
    c.fillText(b.label, b.x + 14, b.y + 22);
  }

  for (const cd of scene.cards) {
    if (!rectVisible(cd, vis)) continue;
    c.globalAlpha = cd.dim ? 0.45 : 1;
    if (cd.w * s >= 56) {
      const kind = KIND_LABEL[cd.p.kind] || cd.p.kind;
      let x = cd.x + CARD_PAD;
      const chipW = textW(kind, F.badge) + 12;
      roundRectPath(c, x, cd.y + 9, chipW, 17, 5);
      c.fillStyle = css(PAL.accentSoft);
      c.fill();
      c.font = F.badge;
      c.fillStyle = css(PAL.accent);
      c.fillText(kind, x + 6, cd.y + 21);
      x += chipW + 8;
      if (s > 0.35) {
        c.font = F.small;
        c.fillStyle = css(PAL.ink);
        const refText = "@ " + (cd.p.git_ref || "worktree");
        c.fillText(refText, x, cd.y + 21);
        x += textW(refText, F.small) + 8;
        c.fillStyle = css(PAL.muted);
        c.fillText(
          fitText(cd.p.config_path, F.small, Math.max(0, cd.x + cd.w - 26 - x)),
          x,
          cd.y + 21
        );
      }
      const dotC = PAL[STATUS_DOT[cd.status] || "muted"];
      c.beginPath();
      c.arc(cd.x + cd.w - 15, cd.y + 17, 4.5, 0, Math.PI * 2);
      c.fillStyle = css(dotC);
      c.fill();
    }
    if (cd.unres && s > 0.3) {
      c.font = F.badge;
      c.fillStyle = css(cd.p.kind === "dynamic_child" ? PAL.warn : PAL.err);
      c.fillText(fitText(cd.unres.title, F.badge, cd.w - CARD_PAD * 2), cd.x + CARD_PAD, cd.y + HEAD_H + 16);
      c.font = F.small;
      c.fillStyle = css(PAL.muted);
      cd.unres.lines.forEach((line, i) => {
        c.fillText(
          fitText(line, F.small, cd.w - CARD_PAD * 2),
          cd.x + CARD_PAD,
          cd.y + HEAD_H + 33 + i * 15
        );
      });
    }
    c.globalAlpha = 1;
  }

  if (s > 0.35) {
    c.font = F.stage;
    c.fillStyle = css(PAL.muted);
    for (const t of scene.stageTitles) {
      if (t.x > vis.x1 || t.x + t.w < vis.x0 || t.y > vis.y1 || t.y + 20 < vis.y0) continue;
      c.fillText(fitText(t.text.toUpperCase(), F.stage, t.w), t.x + 2, t.y + 14);
    }
  }

  if (PILL_H * s >= 8) {
    for (const pl of scene.pills) {
      if (!rectVisible(pl, vis)) continue;
      const dim =
        pl.dim ||
        pl.outcome === "skipped" ||
        pl.outcome === "blocked" ||
        (selLineage && !selLineage.pills.has(pl.idx));
      c.globalAlpha = dim ? 0.55 : 1;
      c.font = F.pill;
      c.fillStyle = css(dim ? PAL.muted : PAL.ink);
      c.fillText(pl.text, pl.x + 8, pl.y + 17);
      let rx = pl.x + pl.w - 7;
      if (pl.badge) {
        c.font = F.badge;
        const bw = textW(pl.badge, F.badge);
        c.fillStyle = css(PAL.accent);
        c.fillText(pl.badge, rx - bw, pl.y + 17);
        rx -= bw + 5;
      }
      if (pl.icons) {
        c.font = F.badge;
        c.fillStyle = css(PAL.muted);
        c.fillText(pl.icons, rx - pl.icons.length * 12, pl.y + 17);
      }
      c.globalAlpha = 1;
    }
  }

  if (s > 0.4) {
    for (const l of scene.labels) {
      if (l.x - l.w / 2 > vis.x1 || l.x + l.w / 2 < vis.x0 || l.y > vis.y1 || l.y + l.h < vis.y0)
        continue;
      c.globalAlpha = l.dim ? 0.25 : 1;
      roundRectPath(c, l.x - l.w / 2, l.y - 10, l.w, l.h, 4);
      c.fillStyle = css(PAL.card, 0.97);
      c.fill();
      if (hoverLabel >= 0 && scene.labels[hoverLabel] === l) {
        c.lineWidth = 1.4;
        c.strokeStyle = css(PAL.accent);
        c.stroke();
      }
      c.textAlign = "center";
      l.lines.forEach((line, i) => {
        const isName = i === 0 && l.srcPill !== null;
        c.font = isName ? F.badge : F.label;
        c.fillStyle = css(isName ? PAL.accent : PAL.muted);
        c.fillText(line, l.x, l.y + i * 12);
      });
      c.textAlign = "left";
      c.globalAlpha = 1;
    }
  }
}

/* ---- draw orchestration ---- */

function draw() {
  if (mode === "webgl2") drawGL();
  else if (mode === "canvas2d") draw2D();
  drawText();
  drawMini();
}

function pulseOn() {
  return mode === "webgl2" && !REDUCED_MOTION && scene.edges.length <= 20000;
}
let rafId = null;
function pulseLoop(t) {
  rafId = null;
  if (!pulseOn() || document.hidden) return;
  pulseTime = t / 1000;
  drawGL(); // text layer is static during the pulse
  rafId = requestAnimationFrame(pulseLoop);
}
function startPulse() {
  if (pulseOn() && rafId === null && typeof requestAnimationFrame === "function")
    rafId = requestAnimationFrame(pulseLoop);
}
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) startPulse();
});

function initRenderer() {
  try {
    if (initGL()) {
      mode = "webgl2";
      return;
    }
  } catch (e) {
    gl = null;
  }
  try {
    ctx2d = glCanvas.getContext("2d");
  } catch (e) {
    ctx2d = null;
  }
  mode = ctx2d ? "canvas2d" : "none";
  if (mode === "none") {
    const note = h(
      "div",
      "render-note",
      "This browser exposes neither WebGL2 nor 2D canvas; the board cannot be drawn. The side data (diagnostics, simulation counts) still works."
    );
    viewport.appendChild(note);
  }
}

function syncBuffers() {
  if (mode === "webgl2") {
    uploadRects();
    uploadEdgeState();
    uploadEdgeColors();
  }
}

/* ================= apply evaluation ================= */

let lastEval = null;

function applyEval() {
  const res = evaluateAll();
  lastEval = res;
  for (const c of scene.cards) {
    c.status = res.status.get(c.p.id);
    c.dim = c.status === "off" && c.p.kind !== "root";
  }
  for (const pl of scene.pills) {
    const ev = res.jobEval.get(pl.id);
    pl.outcome = ev ? ev.outcome : null;
    pl.dim = scene.cards[pl.cardIdx].dim;
  }
  for (const e of scene.edges) {
    let dim = false;
    const srcP = pipeOfJob.get(e.fromJob);
    const srcEv = res.jobEval.get(e.fromJob);
    const srcOff = srcP && res.status.get(srcP.id) === "off";
    const srcDead = srcEv && (srcEv.outcome === "skipped" || srcEv.outcome === "blocked");
    if (srcOff || srcDead) dim = true;
    if (e.toJob) {
      const tEv = res.jobEval.get(e.toJob);
      if (tEv && (tEv.outcome === "skipped" || tEv.outcome === "blocked")) dim = true;
    }
    if (e.toPipeline && res.status.get(e.toPipeline) === "off" && !srcOff) {
      dim = dim || (srcEv && srcEv.outcome !== "manual");
    }
    e.dim = dim;
    if (e.labelIdx != null) scene.labels[e.labelIdx].dim = dim;
  }
  syncBuffers();
  draw();
  updateCounts(res);
  if (selectedJob) renderPanel(selectedJob);
}

/* ================= variable name suggestions ================= */

/** Every variable name the scan ingested: rules:if references, YAML variable
 * keys (pipeline + job) and $VARS in trigger locations. */
function scannedVarNames() {
  const names = new Set();
  const fromExpr = (e) => {
    for (const m of String(e || "").matchAll(/\$([A-Za-z_][A-Za-z0-9_]*)/g)) names.add(m[1]);
  };
  for (const p of G.pipelines) {
    for (const k of Object.keys(p.variables || {})) names.add(k);
    for (const c of (p.workflow_rules && p.workflow_rules.rules) || []) fromExpr(c.if);
    for (const j of p.jobs) {
      for (const k of Object.keys(j.variables || {})) names.add(k);
      for (const c of j.rules.rules || []) fromExpr(c.if);
      if (j.trigger && j.trigger.kind) {
        fromExpr(j.trigger.kind.project);
        fromExpr(j.trigger.kind.branch);
      }
    }
  }
  const custom = [...names].filter((n) => !n.startsWith("CI_")).sort();
  const predefined = [...names].filter((n) => n.startsWith("CI_")).sort();
  return custom.concat(predefined);
}

/* ================= top bars ================= */

const topbar = h("div", "topbar");
const simbar = h("div", "simbar");
const counts = h("span", "chip", "");
const diagChip = h("span", "chip click");

function buildTopbar() {
  const root = G.pipelines.find((p) => p.kind === "root");
  const brand = h("span", "brand", "$ glpv");
  const small = h("small", "", root ? root.project.host + "/" + root.project.path + " @ " + (root.git_ref || "worktree") : "");
  brand.appendChild(small);
  topbar.appendChild(brand);
  const jobs = G.pipelines.reduce((n, p) => n + p.jobs.length, 0);
  topbar.appendChild(h("span", "chip", G.pipelines.length + " pipelines · " + jobs + " jobs"));
  topbar.appendChild(counts);
  diagChip.textContent = G.diagnostics.length + " diagnostics";
  diagChip.addEventListener("click", showDiagnostics);
  topbar.appendChild(diagChip);

  const legend = h("span", "legend-inline");
  const li = (swCls, label) => {
    const s = h("span");
    s.appendChild(h("span", "sw " + swCls));
    s.appendChild(document.createTextNode(" " + label));
    legend.appendChild(s);
  };
  li("n", "needs");
  li("", "trigger");
  li("d", "conditional / manual trigger");
  li("c", "cycle");
  topbar.appendChild(legend);

  const modeSel = document.createElement("select");
  for (const [v, label] of [
    ["focus", "needs: hover / selection"],
    ["all", "needs: all"],
    ["triggers", "needs: hidden"],
  ]) {
    const o = document.createElement("option");
    o.value = v;
    o.textContent = label;
    modeSel.appendChild(o);
  }
  modeSel.value = edgeMode;
  modeSel.title =
    "How needs edges are drawn. Hover a job to see its direct dependencies; click it to trace the full chain.";
  modeSel.addEventListener("change", () => {
    edgeMode = modeSel.value;
    if (mode === "webgl2") uploadEdgeState();
    draw();
  });
  topbar.appendChild(modeSel);

  topbar.appendChild(h("span", "spacer"));
  const zoom = h("span", "zoombar");
  for (const [label, act] of [["−", "out"], ["+", "in"], ["fit", "fit"]]) {
    const b = h("button", "", label);
    b.addEventListener("click", () => zoomAction(act));
    zoom.appendChild(b);
  }
  topbar.appendChild(zoom);
}

function updateCounts(res) {
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
  counts.innerHTML = "";
  counts.append("simulated: ");
  const b = (n, label) => {
    const e = h("b", "", n + " " + label);
    counts.appendChild(e);
    counts.append("  ");
  };
  b(c.runs, "run");
  if (c.manual) b(c.manual, "manual");
  if (c.delayed) b(c.delayed, "delayed");
  b(c.skipped + c.blocked, "skipped");
  if (c.unknown) b(c.unknown, "unknown");
}

function buildSimbar() {
  simbar.appendChild(h("span", "lbl", "SIMULATE"));

  const srcSel = document.createElement("select");
  for (const s of SOURCES) {
    const o = document.createElement("option");
    o.value = s;
    o.textContent = "source: " + s;
    if (s === sim.source) o.selected = true;
    srcSel.appendChild(o);
  }
  srcSel.addEventListener("change", () => { sim.source = srcSel.value; applyEval(); });
  simbar.appendChild(srcSel);

  const root = G.pipelines.find((p) => p.kind === "root");
  const refIn = h("input", "ref");
  refIn.type = "text";
  refIn.placeholder = "ref: " + ((root && (root.git_ref || root.default_branch)) || "main");
  refIn.addEventListener("input", () => { sim.ref = refIn.value.trim(); applyEval(); });
  simbar.appendChild(refIn);

  const tagLbl = h("label");
  const tagCb = h("input");
  tagCb.type = "checkbox";
  tagCb.addEventListener("change", () => { sim.tag = tagCb.checked; applyEval(); });
  tagLbl.appendChild(tagCb);
  tagLbl.appendChild(document.createTextNode(" tag"));
  simbar.appendChild(tagLbl);

  const datalist = h("datalist");
  datalist.id = "glpv-vars";
  for (const name of scannedVarNames()) {
    const o = document.createElement("option");
    o.value = name;
    datalist.appendChild(o);
  }
  simbar.appendChild(datalist);

  const varsWrap = h("span");
  simbar.appendChild(varsWrap);
  const addBtn = h("button", "", "+ variable");
  const renderVars = () => {
    varsWrap.innerHTML = "";
    sim.vars.forEach((pair, i) => {
      const row = h("span", "var-row");
      const k = h("input"); k.type = "text"; k.placeholder = "NAME"; k.value = pair[0];
      k.setAttribute("list", "glpv-vars");
      const v = h("input"); v.type = "text"; v.placeholder = "value"; v.value = pair[1];
      k.addEventListener("input", () => { pair[0] = k.value.trim(); applyEval(); });
      v.addEventListener("input", () => { pair[1] = v.value; applyEval(); });
      const del = h("button", "del", "×");
      del.addEventListener("click", () => { sim.vars.splice(i, 1); renderVars(); applyEval(); });
      row.append(k, document.createTextNode("="), v, del);
      varsWrap.appendChild(row);
    });
  };
  addBtn.addEventListener("click", () => { sim.vars.push(["", ""]); renderVars(); });
  simbar.appendChild(addBtn);
  addSimVarFn = (name, value) => {
    const existing = sim.vars.find((v2) => v2[0] === name);
    if (existing) {
      if (value !== undefined) existing[1] = value;
    } else {
      sim.vars.push([name, value === undefined ? "" : value]);
    }
    renderVars();
    applyEval();
  };

  const mkAssume = (name, get, set) => {
    const sel = document.createElement("select");
    for (const [v, label] of [
      ["null", name + ": undecided"],
      ["true", name + ": match"],
      ["false", name + ": no match"],
    ]) {
      const o = document.createElement("option");
      o.value = v;
      o.textContent = label;
      sel.appendChild(o);
    }
    sel.title =
      "simulation-wide assumption for rules:" + name +
      " — the crawl cannot see the diff/tree, so these clauses are undecided unless you pick a side";
    sel.addEventListener("change", () => {
      set(sel.value === "null" ? null : sel.value === "true");
      applyEval();
    });
    sel.dataset.assume = name;
    simbar.appendChild(sel);
    return sel;
  };
  const chSel = mkAssume("changes", () => sim.assumeChanges, (v) => { sim.assumeChanges = v; });
  const exSel = mkAssume("exists", () => sim.assumeExists, (v) => { sim.assumeExists = v; });

  const reset = h("button", "", "reset");
  reset.addEventListener("click", () => {
    sim.source = DEFAULT_SOURCE;
    sim.ref = ""; sim.tag = false; sim.vars = [];
    sim.assumeChanges = null; sim.assumeExists = null;
    srcSel.value = sim.source; refIn.value = ""; tagCb.checked = false;
    chSel.value = "null"; exSel.value = "null";
    renderVars(); applyEval();
  });
  simbar.appendChild(reset);
  refreshSimBarFn = () => {
    renderVars();
    srcSel.value = sim.source;
    chSel.value = sim.assumeChanges === null ? "null" : String(sim.assumeChanges);
    exSel.value = sim.assumeExists === null ? "null" : String(sim.assumeExists);
  };
  simbar.appendChild(h("span", "sim-note",
    "the whole graph re-evaluates as you type — downstream pipelines grey out when their trigger no longer fires"));
}

/* ================= outcome explorer =================
 * Answers "what are all the possible outcomes?" for a job. The gate chain
 * (workflow rules, each bridge's rules, the job's rules) is evaluated as a
 * lazy decision tree: when a gate is undecided, it branches on the first
 * still-unassigned input that this gate reads — an unknown variable taking
 * the candidate values the rules compare against (plus unset / anything
 * else), or a changes:/exists: clause taking match / no-match. Inputs of
 * later gates only unfold on paths that reach them, so even dozens of
 * unknowns stay tractable, and every leaf is a definitive verdict. */

const SENT_UNSET = "\u0000unset";
const SENT_OTHER = "\u0000other";

function invocationHops(p) {
  const hops = [];
  let cur = p;
  let guard = 0;
  while (cur && cur.parent && guard++ < 12) {
    const pp = pipeById.get(cur.parent[0]);
    if (!pp) break;
    hops.unshift({ pp, bridgeName: cur.parent[1] });
    cur = pp;
  }
  return hops;
}

function gateChainFor(job, p) {
  const hops = invocationHops(p);
  const pipes = hops.map((hp) => hp.pp).concat([p]);
  const gates = [];
  pipes.forEach((cp, i) => {
    if (cp.workflow_rules)
      gates.push({
        kind: "workflow",
        label: cp.project.path + " workflow:rules",
        pipeline: cp,
        rules: cp.workflow_rules,
        when: "on_success",
        isWorkflow: true,
      });
    if (i < hops.length) {
      const bj = jobById(cp.id + "/" + hops[i].bridgeName);
      if (bj) {
        const base = payloadJob(bj);
        gates.push({
          kind: "trigger",
          label: "\u25b6 " + (base.base_name || base.name),
          pipeline: cp,
          rules: base.rules,
          when: base.when,
          job: base,
        });
      }
    }
  });
  gates.push({
    kind: "job",
    label: job.base_name || job.name,
    pipeline: p,
    rules: job.rules,
    when: job.when,
    job,
  });
  return gates;
}

function gateBaseTable(g) {
  if (g.job) return jobVarTable(g.pipeline, g.job, sim);
  const pv = pipelineVarTable(g.pipeline, sim);
  for (const [k, v] of sim.vars) applySimVar(pv, k, v);
  return pv;
}

function simpleReLiteral(reSrc) {
  const m = /^\/\^?([A-Za-z0-9_.-]+)\$?\/$/.exec(reSrc);
  return m ? m[1] : null;
}

/** Per-gate, per-clause branch inputs. A clause question is "does this rule
 * match?"; when a matching clause is a pure conjunction of == comparisons,
 * the implied variable values are pinned so later gates stay consistent. */
function collectClauseInputs(gates) {
  const tables = gates.map(gateBaseTable);
  const inputsByKey = new Map();
  gates.forEach((g, gi) => {
    (g.rules.rules || []).forEach((cl, ci) => {
      const key = "cl:" + gi + ":" + ci;
      inputsByKey.set(key, {
        key,
        kind: "clause",
        gi,
        ci,
        gate: g,
        clause: cl,
        label: truncate(clauseText(cl), 90) + "  (in " + g.label + ")",
        when: cl.when || g.when,
        pins: clausePins(cl, tables),
        values: [true, false],
      });
    });
  });
  return { inputsByKey, tables };
}

/** Variable assignments implied by this clause being TRUE, when derivable:
 * conjunction-only expressions contribute their `$X == "lit"` / `$X == null`
 * terms. `||` or `!` anywhere makes the implication unsafe — no pins then. */
function clausePins(cl, tables) {
  if (!cl.if) return [];
  let toks;
  try {
    toks = lex(String(cl.if));
  } catch (e2) {
    return [];
  }
  if (toks.some((t) => t.k === "||" || t.k === "!")) return [];
  const unknown = (n) =>
    tables.some((tb) => {
      const st2 = tb.get(n);
      return !st2 || st2.k === "unknown";
    });
  const pins = [];
  for (let i2 = 0; i2 < toks.length; i2++) {
    if (toks[i2].k !== "==") continue;
    const a = toks[i2 - 1];
    const b = toks[i2 + 1];
    if (a && a.k === "var" && b && b.k === "str" && unknown(a.v)) pins.push({ name: a.v, value: b.v });
    else if (a && a.k === "str" && b && b.k === "var" && unknown(b.v)) pins.push({ name: b.v, value: a.v });
    else if (a && a.k === "var" && b && b.k === "null" && unknown(a.v)) pins.push({ name: a.v, value: null });
  }
  return pins;
}

function evalGateWorld(g, baseTable, facts, assign, gi) {
  const vars = new Map(baseTable);
  for (const [key, v] of assign) {
    if (!key.startsWith("var:")) continue;
    const n = key.slice(4);
    if (v === null) vars.set(n, { k: "unset" });
    else vars.set(n, { k: "known", v });
  }
  const atoms = {
    clause: (cl, ci) => {
      const v = assign.get("cl:" + gi + ":" + ci);
      return v === undefined ? null : v;
    },
    changes: () => sim.assumeChanges,
    exists: () => sim.assumeExists,
  };
  return evaluateRules(g.rules, vars, g.when, facts, atoms);
}

function solveOutcomes(gates, ctx) {
  const facts = gates.map((g) => factsOf(g.pipeline, sim));
  let nodes = 0;
  const treeSig = (t) =>
    t.leaf
      ? "L:" + t.leaf.cls + "|" + t.leaf.reason
      : "N:" + t.input.key + "(" + t.branches.map((b) => treeSig(b.tree)).join(";") + ")";

  const rec = (gi, assign, depth) => {
    if (gi >= gates.length) return { leaf: { cls: "invoked", sev: "runs", reason: "" } };
    if (nodes++ > 6000 || depth > 80)
      return { leaf: { cls: "undecided", sev: "unknown", reason: "search capped; pin some variables in the sim bar and explore again" } };
    const g = gates[gi];
    const evx = evalGateWorld(g, ctx.tables[gi], facts[gi], assign, gi);
    const o = g.isWorkflow && evx.outcome === "skipped" ? "blocked" : evx.outcome;
    if (o === "blocked")
      return { leaf: { cls: "not invoked", sev: "skipped", reason: g.label + " blocks the pipeline" } };
    if (o === "skipped")
      return {
        leaf:
          g.kind === "job"
            ? { cls: "not invoked", sev: "skipped", reason: "the job's own rules skip it" }
            : { cls: "not invoked", sev: "skipped", reason: g.label + " does not fire" },
      };
    if (o === "manual")
      return {
        leaf:
          g.kind === "job"
            ? { cls: "manual", sev: "manual", reason: "the job itself is a manual action" }
            : { cls: "manual", sev: "manual", reason: g.label + " must be started manually first" },
      };
    if (o !== "unknown") return rec(gi + 1, assign, depth); // runs / delayed: pass

    // branch on the first clause whose result is undecided (from the trace)
    const und = (evx.trace || []).find(
      (t) => t.result === "unknown" && ctx.inputsByKey.has("cl:" + gi + ":" + t.index) && !assign.has("cl:" + gi + ":" + t.index)
    );
    if (!und)
      return {
        leaf: {
          cls: "undecided",
          sev: "unknown",
          reason: "depends on something the explorer cannot enumerate (" + g.label + ")",
        },
      };
    const inp = ctx.inputsByKey.get("cl:" + gi + ":" + und.index);
    const branches = [];
    for (const v of inp.values) {
      assign.set(inp.key, v);
      const pinned = [];
      if (v === true) {
        for (const pin of inp.pins) {
          const pk = "var:" + pin.name;
          if (!assign.has(pk)) {
            assign.set(pk, pin.value);
            pinned.push(pk);
          }
        }
      }
      branches.push({ value: v, tree: rec(gi, assign, depth + 1) });
      for (const pk of pinned) assign.delete(pk);
      assign.delete(inp.key);
    }
    const merged = new Map();
    for (const b of branches) {
      const sg = treeSig(b.tree);
      if (!merged.has(sg)) merged.set(sg, { values: [], tree: b.tree });
      merged.get(sg).values.push(b.value);
    }
    const out = [...merged.values()];
    if (out.length === 1) return out[0].tree;
    return { input: inp, branches: out };
  };
  return rec(0, new Map(), 0);
}

function valueLabel(inp, v) {
  return v ? "matches" : "doesn't match";
}

function renderOutcomeTree(node) {
  if (node.leaf) {
    const sp = h("span");
    sp.appendChild(h("span", "badge " + node.leaf.sev, node.leaf.cls));
    if (node.leaf.reason) sp.append(" " + node.leaf.reason);
    return sp;
  }
  const wrap = h("div");
  wrap.appendChild(h("div", "otree-var", node.input.label));
  const ul = h("ul", "otree");
  for (const b of node.branches) {
    const li = h("li");
    li.appendChild(
      h("span", "otree-val", b.values.map((v) => valueLabel(node.input, v)).join(" or "))
    );
    li.append(" \u2192 ");
    li.appendChild(renderOutcomeTree(b.tree));
    ul.appendChild(li);
  }
  wrap.appendChild(ul);
  return wrap;
}

function buildOutcomeExplorer(job, p) {
  const gates = gateChainFor(job, p);
  const ctx = collectClauseInputs(gates);
  const tree = solveOutcomes(gates, ctx);
  const box = h("div", "otree-box");
  box.appendChild(h("h3", "", "All possible outcomes"));

  // outcome-first: for every distinct verdict, the shortest path that
  // reaches it, with runs of "rule doesn't match" folded per gate
  const classes = new Map();
  const walk = (t, path) => {
    if (t.leaf) {
      const sig = t.leaf.cls + "|" + t.leaf.reason;
      const cur = classes.get(sig);
      if (!cur) classes.set(sig, { verdict: t.leaf, count: 1, best: path.slice() });
      else {
        cur.count++;
        if (path.length < cur.best.length) cur.best = path.slice();
      }
      return;
    }
    for (const b of t.branches) {
      path.push({ input: t.input, values: b.values });
      walk(b.tree, path);
      path.pop();
    }
  };
  walk(tree, []);

  const rank = { runs: 0, manual: 1, skipped: 2, unknown: 3 };
  const sorted = [...classes.values()].sort(
    (a, b) => (rank[a.verdict.sev] ?? 9) - (rank[b.verdict.sev] ?? 9)
  );
  box.appendChild(
    h("div", "note", sorted.length + " distinct outcome(s), each with the shortest way to get there.")
  );

  const outcomeTitle = (v) => {
    if (v.sev === "runs") return "This job runs";
    if (v.sev === "manual") return "Runs only after a manual action";
    if (v.sev === "unknown") return "Still undecided";
    const r = v.reason || "";
    if (r.includes("own rules skip")) return "Skipped by its own rules";
    if (r.includes("blocks the pipeline"))
      return "Pipeline blocked \u2014 " + r.replace(" blocks the pipeline", "");
    if (r.includes("does not fire"))
      return "Never reached \u2014 " + r.replace(" does not fire", " doesn't fire");
    return "Not invoked";
  };
  const gateName = (g) =>
    g.kind === "workflow" ? g.pipeline.project.path : g.label;
  const gateChip = (g) =>
    g.kind === "workflow" ? "workflow" : g.kind === "trigger" ? "trigger" : "this job";
  const ruleVerb = (inp) => {
    const w2 = inp.when || "on_success";
    if (w2 === "never") return "a stop rule fires:";
    if (w2 === "manual") return "a manual rule fires:";
    return inp.gate.kind === "trigger" ? "the trigger fires when" : "a rule matches when";
  };

  const renderSteps = (steps) => {
    const ol = h("ol", "oc-steps");
    let falseRun = null;
    const flush = () => {
      if (!falseRun) return;
      const li = h("li", "oc-step");
      li.appendChild(h("span", "via", gateChip(falseRun.g)));
      li.append(" " + gateName(falseRun.g) + ": ");
      li.appendChild(
        h("span", "oc-quiet",
          falseRun.n === 1 ? "its undecided rule doesn't match" : "none of its " + falseRun.n + " undecided rules match")
      );
      ol.appendChild(li);
      falseRun = null;
    };
    for (const step of steps) {
      if (step.values.length !== 1) continue;
      const inp = step.input;
      if (step.values[0] === false) {
        if (falseRun && falseRun.g === inp.gate) falseRun.n++;
        else {
          flush();
          falseRun = { g: inp.gate, n: 1 };
        }
        continue;
      }
      flush();
      const li = h("li", "oc-step");
      li.appendChild(h("span", "via", gateChip(inp.gate)));
      li.append(" " + gateName(inp.gate) + " \u2014 " + ruleVerb(inp));
      li.appendChild(condBox(inp.clause));
      for (const pin of inp.pins) {
        if (pin.value === null || !addSimVarFn) continue;
        const btn = h("button", "loc-link", "+ set $" + pin.name);
        btn.title = 'set $' + pin.name + ' = "' + pin.value + '" in the simulation';
        btn.addEventListener("click", (e2) => {
          e2.stopPropagation();
          addSimVarFn(pin.name, pin.value);
        });
        li.appendChild(btn);
      }
      ol.appendChild(li);
    }
    flush();
    if (!ol.children.length) {
      const li = h("li", "oc-step");
      li.appendChild(h("span", "oc-quiet", "unconditional under the current simulation"));
      ol.appendChild(li);
    }
    return ol;
  };

  const ul = h("div", "oc-list");
  for (const c of sorted) {
    const card = h("div", "oc " + c.verdict.sev);
    const head = h("div", "oc-head");
    head.appendChild(h("span", "badge " + c.verdict.sev, c.verdict.cls));
    head.appendChild(h("span", "oc-title", outcomeTitle(c.verdict)));
    if (c.verdict.sev === "runs") {
      const ap = h("button", "loc-link", "\u26a1 apply");
      ap.title = "find and apply a full scenario that makes this job run";
      ap.addEventListener("click", (e2) => {
        e2.stopPropagation();
        const sc = findEnablingScenario(gates);
        if (sc) applyScenario(sc);
      });
      head.appendChild(ap);
    }
    if (c.count > 1)
      head.appendChild(h("span", "note-inline", c.count + " ways"));
    card.appendChild(head);
    card.appendChild(renderSteps(c.best));
    ul.appendChild(card);
  }
  box.appendChild(ul);

  const det = document.createElement("details");
  const sum = document.createElement("summary");
  sum.textContent = "full decision tree";
  det.appendChild(sum);
  det.appendChild(renderOutcomeTree(tree));
  box.appendChild(det);
  box.appendChild(
    h("div", "note",
      "each undecided rule on the invocation path is explored both ways (matches / doesn't match); a matching rule that pins variable values keeps later gates consistent.")
  );
  return box;
}

/* ================= scenario finder =================
 * Searches for a concrete simulation (variable values + changes/exists
 * assumptions) under which the job is invoked, guided by the evaluator's
 * trace: it only branches on the input currently blocking a gate. */

function tokenText(t) {
  switch (t.k) {
    case "var": return "$" + t.v;
    case "str": return '"' + t.v + '"';
    case "re": return t.v;
    case "bool": return String(t.v);
    case "null": return "null";
    case "(": return "(";
    case ")": return ")";
    default: return t.k;
  }
}

/** Split an expression at top-level `&&`; null when `||` sits at top level. */
function splitTopAnd(expr) {
  let toks;
  try {
    toks = lex(String(expr));
  } catch (e2) {
    return null;
  }
  let depth = 0;
  const groups = [[]];
  for (const t of toks) {
    if (t.k === "(") depth++;
    else if (t.k === ")") depth--;
    if (t.k === "&&" && depth === 0) {
      groups.push([]);
      continue;
    }
    if (t.k === "||" && depth === 0) return null;
    groups[groups.length - 1].push(t);
  }
  return groups.map((g) =>
    g.map(tokenText).join(" ").replaceAll("( ", "(").replaceAll(" )", ")")
  );
}

/** Split at top level by the given operator; null when not splittable. */
function splitTopBy(expr, op) {
  let toks;
  try {
    toks = lex(String(expr));
  } catch (e2) {
    return null;
  }
  let depth = 0;
  const groups = [[]];
  for (const t of toks) {
    if (t.k === "(") depth++;
    else if (t.k === ")") depth--;
    if (t.k === op && depth === 0) {
      groups.push([]);
      continue;
    }
    groups[groups.length - 1].push(t);
  }
  return groups.length > 1 ? groups : null;
}

/** Syntax-highlighted spans for a token list. */
function tokensToSpans(toks) {
  const out = h("span");
  toks.forEach((t, i) => {
    if (i) out.append(" ");
    if (t.k === "var") out.appendChild(h("span", "cv", "$" + t.v));
    else if (t.k === "str") out.appendChild(h("span", "cs", '"' + t.v + '"'));
    else if (t.k === "re") out.appendChild(h("span", "cs", t.v));
    else out.appendChild(h("span", "co", tokenText(t)));
  });
  return out;
}

/** A clause's condition as readable rows, split at top-level or / and. */
function condBox(clause) {
  const box = h("div", "cond");
  if (clause.if) {
    const orParts = splitTopBy(clause.if, "||");
    const andParts = orParts ? null : splitTopBy(clause.if, "&&");
    const parts = orParts || andParts;
    const word = orParts ? "or" : "and";
    if (parts) {
      parts.forEach((toksPart, i) => {
        const row = h("div", "cond-row");
        row.appendChild(h("span", "cond-join", i ? word : ""));
        row.appendChild(tokensToSpans(toksPart));
        box.appendChild(row);
      });
    } else {
      let toks = null;
      try {
        toks = lex(String(clause.if));
      } catch (e2) {
        /* raw fallback below */
      }
      const row = h("div", "cond-row");
      row.appendChild(h("span", "cond-join", ""));
      if (toks) row.appendChild(tokensToSpans(toks));
      else row.append(String(clause.if));
      box.appendChild(row);
    }
  }
  if (clause.changes) {
    const row = h("div", "cond-row cond-sep");
    row.appendChild(h("span", "cond-join", clause.if ? "and" : ""));
    row.append("changed files match ");
    row.appendChild(h("span", "cs", clause.changes.join(", ")));
    box.appendChild(row);
  }
  if (clause.exists) {
    const row = h("div", "cond-row cond-sep");
    row.appendChild(h("span", "cond-join", clause.if || clause.changes ? "and" : ""));
    row.append("repo contains ");
    row.appendChild(h("span", "cs", clause.exists.join(", ")));
    box.appendChild(row);
  }
  return box;
}

/** Candidate values per variable, from every comparison in the gate chain. */
function varCandidatesFor(gates) {
  const cand = new Map();
  const bare = new Set();
  for (const g of gates) {
    for (const cl of g.rules.rules || []) {
      if (!cl.if) continue;
      let toks;
      try {
        toks = lex(String(cl.if));
      } catch (e2) {
        continue;
      }
      const isCmp = (x) => x && (x.k === "==" || x.k === "!=" || x.k === "=~" || x.k === "!~");
      toks.forEach((t, i2) => {
        if (t.k !== "var") return;
        if (!cand.has(t.v)) cand.set(t.v, new Set());
        const addLit = (tok) => {
          if (!tok) return;
          if (tok.k === "str") cand.get(t.v).add(tok.v);
          else if (tok.k === "re") {
            const m = /^\/\^?([A-Za-z0-9_.-]+)\$?\/$/.exec(tok.v);
            if (m) cand.get(t.v).add(m[1]);
          }
        };
        if (isCmp(toks[i2 + 1])) addLit(toks[i2 + 2]);
        if (isCmp(toks[i2 - 1])) addLit(toks[i2 - 2]);
        if (!isCmp(toks[i2 - 1]) && !isCmp(toks[i2 + 1])) bare.add(t.v);
      });
    }
  }
  const out = new Map();
  for (const [n, set] of cand) {
    // unset first: scenarios read cleaner when a variable can simply stay unset
    const vals = [null, ...[...set].slice(0, 6)];
    if (bare.has(n)) vals.push("true");
    vals.push("(other)"); // any value not compared against
    out.set(n, vals);
  }
  return out;
}

function evalGateSim(g, table, facts, assign, atomAssign) {
  const vars = new Map(table);
  for (const [n, v] of assign)
    vars.set(n, v === null ? { k: "unset" } : { k: "known", v });
  const atoms = {
    changes: () =>
      atomAssign.has("changes") ? atomAssign.get("changes") : sim.assumeChanges,
    exists: () =>
      atomAssign.has("exists") ? atomAssign.get("exists") : sim.assumeExists,
  };
  return evaluateRules(g.rules, vars, g.when, facts, atoms);
}

function findEnablingScenario(gates) {
  const cand = varCandidatesFor(gates);
  // the pipeline source is itself a lever: try the current one first, then
  // every other source (facts and predefined variables follow it)
  const savedSource = sim.source;
  try {
    for (const src of [sim.source, ...SOURCES.filter((x) => x !== sim.source)]) {
      sim.source = src;
      const found = searchScenario(gates, cand);
      if (found) {
        found.source = src === savedSource ? null : src;
        return found;
      }
    }
    return null;
  } finally {
    sim.source = savedSource;
  }
}

function searchScenario(gates, cand) {
  const tables = gates.map(gateBaseTable);
  const facts = gates.map((g) => factsOf(g.pipeline, sim));
  let nodes = 0;
  const rec = (assign, atomAssign) => {
    if (nodes++ > 8000) return null;
    for (let gi = 0; gi < gates.length; gi++) {
      const g = gates[gi];
      const evx = evalGateSim(g, tables[gi], facts[gi], assign, atomAssign);
      const o = g.isWorkflow && evx.outcome === "skipped" ? "blocked" : evx.outcome;
      if (o === "blocked" || o === "skipped" || o === "manual") return null;
      if (o !== "unknown") continue; // runs / delayed: next gate
      const t = (evx.trace || []).find((x) => x.result === "unknown");
      if (!t) return null;
      const clause = (g.rules.rules || [])[t.index];
      const uv = (t.varsUsed || []).find(
        ([n2, st2]) => st2 === "unknown" && !assign.has(n2)
      );
      if (uv && cand.has(uv[0])) {
        for (const v of cand.get(uv[0])) {
          assign.set(uv[0], v);
          const r = rec(assign, atomAssign);
          if (r) return r;
          assign.delete(uv[0]);
        }
        return null;
      }
      for (const key of ["changes", "exists"]) {
        if (clause && clause[key] && !atomAssign.has(key)) {
          for (const v of [true, false]) {
            atomAssign.set(key, v);
            const r = rec(assign, atomAssign);
            if (r) return r;
            atomAssign.delete(key);
          }
          return null;
        }
      }
      return null; // legacy or otherwise undecidable
    }
    return { assign: new Map(assign), atomAssign: new Map(atomAssign) };
  };
  const found = rec(new Map(), new Map());
  if (!found) return null;
  // minimise: drop every assignment the invocation does not actually need
  const stillRuns = (assign, atomAssign) => {
    for (let gi = 0; gi < gates.length; gi++) {
      const evx = evalGateSim(gates[gi], tables[gi], facts[gi], assign, atomAssign);
      const o = gates[gi].isWorkflow && evx.outcome === "skipped" ? "blocked" : evx.outcome;
      if (o !== "runs" && o !== "delayed") return false;
    }
    return true;
  };
  for (const key of [...found.assign.keys()]) {
    const v = found.assign.get(key);
    found.assign.delete(key);
    if (!stillRuns(found.assign, found.atomAssign)) found.assign.set(key, v);
  }
  for (const key of [...found.atomAssign.keys()]) {
    const v = found.atomAssign.get(key);
    found.atomAssign.delete(key);
    if (!stillRuns(found.assign, found.atomAssign)) found.atomAssign.set(key, v);
  }
  return found;
}

let refreshSimBarFn = null; // set by buildSimbar

function applyScenario(sc) {
  if (sc.source) sim.source = sc.source;
  for (const [n, v] of sc.assign) {
    const val = v === null ? "(unset)" : v;
    const row = sim.vars.find((r) => r[0] === n);
    if (row) row[1] = val;
    else sim.vars.push([n, val]);
  }
  if (sc.atomAssign.has("changes")) sim.assumeChanges = sc.atomAssign.get("changes");
  if (sc.atomAssign.has("exists")) sim.assumeExists = sc.atomAssign.get("exists");
  if (refreshSimBarFn) refreshSimBarFn();
  applyEval();
}

/* ================= side panel ================= */

const panel = h("div", "panel hidden");
let selectedJob = null;
let addSimVarFn = null; // set by buildSimbar; adds a variable row to the sim bar

/** Outcome counts over the transitive `needs` closure feeding a pill. */
function needsClosureCounts(pillIdx) {
  if (pillIdx === undefined || pillIdx < 0 || pillIdx === null) return null;
  const seen = new Set([pillIdx]);
  const st = [pillIdx];
  const counts = { total: 0, runs: 0, unknown: 0, skipped: 0, manual: 0 };
  while (st.length) {
    const q = st.pop();
    for (const ei of adj.inn.get(q) || []) {
      const np = scene.edges[ei].fromPill;
      if (seen.has(np)) continue;
      seen.add(np);
      st.push(np);
      counts.total++;
      const evx = lastEval && lastEval.jobEval.get(scene.pills[np].id);
      const o = evx ? evx.outcome : "unknown";
      if (o === "runs" || o === "delayed") counts.runs++;
      else if (o === "manual") counts.manual++;
      else if (o === "unknown") counts.unknown++;
      else counts.skipped++;
    }
  }
  return counts;
}

function selectJob(id) {
  if (selectedJob === id || id === null) {
    selectedJob = null;
    selLineage = null;
    panel.classList.add("hidden");
    applyEval();
    return;
  }
  selectedJob = id;
  const idx = scene.pillByJob.get(id);
  selLineage = idx === undefined ? null : traceLineage(idx);
  applyEval(); // re-evaluates with trace_job = the selection, then renders the panel
  panel.classList.remove("hidden");
}

function renderPanel(id) {
  let job = jobById(id);
  const p = pipeOfJob.get(id);
  if (!job || !p) return;
  job = payloadJob(job);
  const pillIdx = scene.pillByJob.get(job.id);
  const count = pillIdx === undefined ? 1 : scene.pills[pillIdx].count;
  const shownName = (job.base_name || job.name) + (count > 1 ? "  ×" + count : "");
  const ev = lastEval && lastEval.jobEval.get(id);
  panel.innerHTML = "";

  const close = h("button", "close", "✕");
  close.addEventListener("click", () => selectJob(id));
  panel.appendChild(close);
  panel.appendChild(h("h2", "", shownName));
  panel.appendChild(h("div", "sub",
    p.project.path + " · stage " + job.stage + " · when: " + job.when +
    (count > 1 ? " · " + count + " parallel expansions" : "")));

  if (ev) {
    const badge = h("span", "badge " + ev.outcome, ev.outcome + (ev.blockedBy ? " (" + ev.blockedBy + ")" : ""));
    panel.appendChild(badge);
  }

  /* ---- how this job is invoked: trigger chain + include chain ---- */
  {
    panel.appendChild(h("h3", "", "How it's invoked"));
    const ul = h("ul", "prov-list");

    // pipeline chain: root ... -> bridge job -> this pipeline
    const hops = [];
    let cur = p;
    let guard = 0;
    while (cur && cur.parent && guard++ < 12) {
      const pp = pipeById.get(cur.parent[0]);
      if (!pp) break;
      hops.unshift({ pp, bridgeName: cur.parent[1] });
      cur = pp;
    }
    for (const hop of hops) {
      const li = h("li");
      li.appendChild(h("span", "via", KIND_LABEL[hop.pp.kind] || hop.pp.kind));
      li.append(hop.pp.project.path + " @ " + (hop.pp.git_ref || "worktree") + " ");
      const b = h("button", "loc-link", "▶ " + hop.bridgeName);
      b.title = "jump to the trigger job that starts this pipeline";
      b.addEventListener("click", (ev2) => {
        ev2.stopPropagation();
        const bj = jobById(hop.pp.id + "/" + hop.bridgeName);
        if (bj) {
          const base = payloadJob(bj);
          flyTo(scene.pillByJob.get(base.id));
          selectJob(base.id);
        }
      });
      li.appendChild(b);
      ul.appendChild(li);
    }
    if (hops.length) {
      const li = h("li");
      li.appendChild(h("span", "via", KIND_LABEL[p.kind] || p.kind));
      li.append("this pipeline — " + p.project.path + " · " + p.config_path);
      ul.appendChild(li);
    }

    // include chain: entry file -> ... -> the file that defines this job
    const defFile = job.provenance.defined_at.file;
    const incEdges = [];
    let f = defFile;
    let g2 = 0;
    while (f !== p.entry_source && g2++ < 60) {
      const e = incomingInclude.get(p.id + "|" + f);
      if (!e) break;
      incEdges.unshift(e);
      f = e.from;
    }
    if (!incEdges.length && defFile === p.entry_source) {
      const li = h("li");
      li.appendChild(h("span", "via", "entry"));
      li.append("defined directly in " + ((sourceMeta(defFile) || {}).path || "the entry file"));
      ul.appendChild(li);
    }
    for (const e of incEdges) {
      const li = h("li");
      li.appendChild(h("span", "via", "include"));
      li.append(truncate(e.location, 44) + " ");
      li.appendChild(makeLocLink(e.span));
      ul.appendChild(li);
    }
    if (incEdges.length) {
      const alt = incomingIncludeCount.get(p.id + "|" + defFile) || 1;
      const li = h("li");
      li.appendChild(h("span", "via", "defines"));
      li.append((sourceMeta(defFile) || { path: "file " + defFile }).path + " ");
      li.appendChild(makeLocLink(job.provenance.defined_at));
      if (alt > 1) li.appendChild(h("div", "note", "+" + (alt - 1) + " other include path(s) also pull this file in"));
      ul.appendChild(li);
    }
    if (ul.children.length) panel.appendChild(ul);
    else panel.removeChild(panel.lastChild); // no chain info: drop the heading

    /* ---- invocation simulation: every gate on the way, evaluated live ---- */
    panel.appendChild(h("h3", "", "Invocation simulation"));
    const gates = gateChainFor(job, p);
    const gTables = gates.map(gateBaseTable);
    const gFacts = gates.map((g2) => factsOf(g2.pipeline, sim));
    const emptyA = new Map();
    const steps = gates.map((g2, gi) => {
      const evx = evalGateSim(g2, gTables[gi], gFacts[gi], emptyA, emptyA);
      const o = g2.isWorkflow && evx.outcome === "skipped" ? "blocked" : evx.outcome;
      const decider =
        (evx.trace || []).find((x) => x.result === "matched") ||
        (evx.trace || []).find((x) => x.result === "unknown") ||
        null;
      return {
        g: g2, gi, evx, outcome: o, decider,
        clause: decider ? (g2.rules.rules || [])[decider.index] : null,
      };
    });

    let verdict = "invoked \u2713";
    let vcls = "runs";
    for (const st2 of steps) {
      if (st2.outcome === "blocked" || st2.outcome === "skipped") {
        verdict =
          st2.g.kind === "job"
            ? "not invoked \u2014 the job's rules skip it"
            : "not invoked \u2014 " + st2.g.label + (st2.g.isWorkflow ? " blocks the pipeline" : " does not fire");
        vcls = "skipped";
        break;
      }
      if (st2.outcome === "manual") {
        verdict =
          st2.g.kind === "job"
            ? "manual \u2014 the job must be played"
            : "requires manual start of " + st2.g.label;
        vcls = "manual";
        break;
      }
      if (st2.outcome === "unknown") {
        verdict = "undecided \u2014 see the gates below";
        vcls = "unknown";
        break;
      }
    }

    const vRow = h("div", "verdict-row");
    vRow.appendChild(h("span", "badge " + vcls, verdict));
    if (vcls !== "runs") {
      const en = h("button", "explore", "\u26a1 Enable in simulation");
      en.title =
        "find variable values (and changes/exists assumptions) that make this job run, and apply them to the whole simulation";
      en.addEventListener("click", () => {
        const sc = findEnablingScenario(gates);
        if (sc) applyScenario(sc);
        else
          en.replaceWith(
            h("span", "note",
              "no automatic scenario invokes this job (every pipeline source tried) \u2014 each path is blocked or needs a manual action")
          );
      });
      vRow.appendChild(en);
    }
    panel.appendChild(vRow);

    const KIND_TAG = { workflow: "workflow", trigger: "trigger", job: "this job" };
    const stateIco = (r2) => (r2 === "true" ? "\u2713" : r2 === "false" ? "\u2717" : "?");
    const stateCls = (r2) => (r2 === "true" ? "t-ok" : r2 === "false" ? "t-no" : "t-un");
    const sul = h("ul", "trace");
    for (const st2 of steps) {
      const li = h("li");
      li.appendChild(h("span", "badge " + st2.outcome, st2.outcome));
      li.append(" " + KIND_TAG[st2.g.kind] + ": " + st2.g.label);
      if (st2.clause) {
        const when = (st2.decider && st2.decider.when) || st2.g.when || "on_success";
        li.appendChild(
          h("div", "note",
            (st2.outcome === "unknown" ? "hinges on this rule" : "decided by this rule") +
            " \u2192 when: " + when)
        );
        const wrap = h("div", "terms");
        const addTerm = (r2, text, vals) => {
          const row = h("div", "term " + stateCls(r2));
          row.appendChild(h("span", "t-ico", stateIco(r2)));
          row.appendChild(h("span", "t-txt", text));
          if (vals) row.appendChild(h("span", "t-vals", vals));
          wrap.appendChild(row);
        };
        const terms = st2.clause.if ? splitTopAnd(st2.clause.if) : null;
        const varsLine = (r2) =>
          r2.varsUsed.map(([n2, sv]) => "$" + n2 + " = " + stateText(sv)).join("   ");
        if (terms && terms.length > 1) {
          for (const term of terms) {
            const r2 = evalIf(term, gTables[st2.gi]);
            addTerm(r2.result, term, varsLine(r2));
          }
        } else if (st2.clause.if) {
          const r2 = evalIf(st2.clause.if, gTables[st2.gi]);
          addTerm(r2.result, truncate(String(st2.clause.if), 110), varsLine(r2));
        }
        if (st2.clause.changes) {
          const a = sim.assumeChanges;
          addTerm(
            a === null ? "unknown" : a ? "true" : "false",
            "changes: [" + truncate(st2.clause.changes.join(", "), 70) + "]",
            a === null ? "pick an assumption in the sim bar" : "assumed " + (a ? "matching" : "not matching")
          );
        }
        if (st2.clause.exists) {
          const a = sim.assumeExists;
          addTerm(
            a === null ? "unknown" : a ? "true" : "false",
            "exists: [" + truncate(st2.clause.exists.join(", "), 70) + "]",
            a === null ? "pick an assumption in the sim bar" : "assumed " + (a ? "matching" : "not matching")
          );
        }
        li.appendChild(wrap);
      }
      if (st2.g.job) {
        const n2 = needsClosureCounts(scene.pillByJob.get(st2.g.job.id));
        if (n2 && n2.total)
          li.appendChild(
            h("div", "note",
              "needs chain: " + n2.total + " job(s) \u2014 " + n2.runs + " run" +
              (n2.manual ? ", " + n2.manual + " manual" : "") +
              (n2.unknown ? ", " + n2.unknown + " unknown" : "") +
              (n2.skipped ? ", " + n2.skipped + " skipped" : ""))
          );
      }
      if (st2.g.kind === "job") li.appendChild(h("div", "note", "full rule trace below"));
      sul.appendChild(li);
    }
    panel.appendChild(sul);

    // every variable these gates read, with its current simulated state
    const varMap = new Map();
    for (const st2 of steps)
      for (const t of st2.evx.trace || []) {
        if (t.result === "not_reached") continue;
        for (const [n2, sv] of t.varsUsed || []) if (!varMap.has(n2)) varMap.set(n2, sv);
      }
    if (varMap.size) {
      const undecided = [...varMap].filter(([, sv]) => sv === "unknown" || sv === "unset");
      const known = [...varMap].filter(([, sv]) => sv !== "unknown" && sv !== "unset");
      panel.appendChild(h("h3", "", "Variables on this path"));
      const tbl = h("table", "kv");
      for (const [n2, sv] of undecided.concat(known)) {
        const tr = h("tr");
        tr.appendChild(h("td", "", "$" + n2));
        const td = h("td", "", sv + " ");
        if ((sv === "unknown" || sv === "unset") && addSimVarFn) {
          const b2 = h("button", "loc-link", "+ set");
          b2.title = "add this variable to the simulation bar";
          b2.addEventListener("click", (ev3) => {
            ev3.stopPropagation();
            addSimVarFn(n2);
          });
          td.appendChild(b2);
        }
        tr.appendChild(td);
        tbl.appendChild(tr);
      }
      panel.appendChild(tbl);
    }

    const exBtn = h("button", "explore", "Explore all possible outcomes \u2192");
    exBtn.addEventListener("click", () => {
      exBtn.replaceWith(buildOutcomeExplorer(job, p));
    });
    panel.appendChild(exBtn);
  }

  if (ev && ev.trace && ev.trace.length) {
    panel.appendChild(h("h3", "", "Rule trace (current simulation)"));
    const ul = h("ul", "trace");
    for (const t of ev.trace) {
      const li = h("li");
      const b = h("span", "badge " + t.result, t.result.replaceAll("_", " "));
      li.appendChild(b);
      li.append(" → when: " + (t.when || "on_success"));
      li.appendChild(h("div", "cl", t.clause));
      if (t.varsUsed && t.varsUsed.length) {
        li.appendChild(h("div", "vars", t.varsUsed.map(([n, s]) => "$" + n + " = " + s).join("   ")));
      }
      if (t.note) li.appendChild(h("div", "note", t.note));
      ul.appendChild(li);
    }
    panel.appendChild(ul);
  }

  if (job.trigger) {
    panel.appendChild(h("h3", "", "Trigger"));
    const tbl = h("table", "kv");
    const row = (k, v) => {
      const tr = h("tr");
      tr.appendChild(h("td", "", k));
      tr.appendChild(h("td", "", v));
      tbl.appendChild(tr);
    };
    const k = job.trigger.kind;
    if (k.type === "multi_project") {
      row("project", k.project + (k.project_resolved ? " → " + k.project_resolved.path : ""));
      row("branch", k.branch_resolved || k.branch || "(default branch)");
    } else if (k.type === "dynamic_child") {
      row("artifact", k.artifact);
      row("generated by", k.job);
    } else {
      row("child configs", String((k.includes || []).length));
    }
    if (job.trigger.strategy) row("strategy", job.trigger.strategy);
    panel.appendChild(tbl);
  }

  if (job.needs && job.needs.length) {
    panel.appendChild(h("h3", "", "Needs"));
    const ul = h("ul", "prov-list");
    for (const n of job.needs) {
      const li = h("li");
      li.append(n.job + (n.optional ? " (optional)" : ""));
      if (n.project) li.append(" ← artifacts from " + n.project);
      ul.appendChild(li);
    }
    panel.appendChild(ul);
  }

  const vars = Object.entries(job.variables || {});
  if (vars.length) {
    panel.appendChild(h("h3", "", "Job variables"));
    const tbl = h("table", "kv");
    for (const [k, v] of vars) {
      const tr = h("tr");
      tr.appendChild(h("td", "", k));
      tr.appendChild(h("td", "", v));
      tbl.appendChild(tr);
    }
    panel.appendChild(tbl);
  }

  panel.appendChild(h("h3", "", "Defined at"));
  const prov = h("ul", "prov-list");
  const defLi = h("li");
  defLi.appendChild(makeLocLink(job.provenance.defined_at));
  prov.appendChild(defLi);
  for (const c of job.provenance.contributors || []) {
    const li = h("li");
    const kind = c.kind.type || c.kind;
    li.appendChild(h("span", "via", kind));
    if (c.kind.name) li.append(c.kind.name + " ");
    li.appendChild(makeLocLink(c.span));
    prov.appendChild(li);
  }
  panel.appendChild(prov);

  panel.appendChild(h("h3", "", "Effective configuration"));
  panel.appendChild(h("pre", "yaml", job.merged_yaml || ""));
}

function sourceMeta(fileIdx) {
  return G.sources.find((s) => s.file === fileIdx);
}
function makeLocLink(span) {
  const meta = sourceMeta(span.file);
  const label = (meta ? meta.path : "file " + span.file) + ":" + span.start[0];
  const b = h("button", "loc-link", label);
  b.addEventListener("click", (e) => {
    e.stopPropagation();
    openSource(span.file, span.start[0]);
  });
  return b;
}

/* ================= overlays: source viewer + diagnostics ================= */

function overlay(headContent, bodyEl) {
  const ov = h("div", "overlay");
  const card = h("div", "overlay-card");
  const head = h("div", "overlay-head");
  head.append(headContent);
  head.appendChild(h("span", "spacer"));
  const close = h("button", "close", "✕");
  close.addEventListener("click", () => ov.remove());
  head.appendChild(close);
  card.appendChild(head);
  const body = h("div", "overlay-body");
  body.appendChild(bodyEl);
  card.appendChild(body);
  ov.appendChild(card);
  ov.addEventListener("click", (e) => { if (e.target === ov) ov.remove(); });
  document.body.appendChild(ov);
  const esc = (e) => {
    if (e.key === "Escape") { ov.remove(); document.removeEventListener("keydown", esc); }
  };
  document.addEventListener("keydown", esc);
  return ov;
}

function openSource(fileIdx, line) {
  const meta = sourceMeta(fileIdx);
  if (!meta) return;
  const title = (meta.project ? meta.project.path + " · " : "") + meta.path +
    (meta.sha ? " @ " + meta.sha.slice(0, 8) : "");
  if (!meta.text) {
    overlay(title, h("div", "diag-list", "source text not embedded (scan ran with --no-embed-sources)"));
    return;
  }
  const tbl = h("table", "src");
  const lines = meta.text.split("\n");
  let hlRow = null;
  lines.forEach((text, i) => {
    const tr = h("tr", i + 1 === line ? "hl" : "");
    tr.appendChild(h("td", "ln", String(i + 1)));
    tr.appendChild(h("td", "", text));
    if (i + 1 === line) hlRow = tr;
    tbl.appendChild(tr);
  });
  overlay(title, tbl);
  if (hlRow) hlRow.scrollIntoView({ block: "center" });
}

function showDiagnostics() {
  const ul = h("ul", "diag-list");
  for (const d of G.diagnostics) {
    const li = h("li");
    li.appendChild(h("span", "sev " + d.severity, d.severity.toUpperCase()));
    li.append("[" + d.code + "] " + d.message);
    if (d.span) {
      li.append(" ");
      li.appendChild(makeLocLink(d.span));
    }
    if (d.hint) li.appendChild(h("div", "note", "hint: " + d.hint));
    ul.appendChild(li);
  }
  if (!G.diagnostics.length) ul.appendChild(h("li", "", "no diagnostics"));
  overlay("diagnostics", ul);
}

/* ================= camera & interaction ================= */

function fitView() {
  const sw = scene.size.w, sh = scene.size.h;
  view.scale = Math.min(1, (vw - 20) / sw, (vh - 20) / sh);
  view.scale = Math.max(view.scale, 0.03);
  view.tx = Math.max(10, (vw - sw * view.scale) / 2);
  view.ty = 10;
  draw();
}
function flyTo(pillIdx) {
  if (pillIdx === undefined || pillIdx < 0) return;
  const pl = scene.pills[pillIdx];
  view.scale = Math.max(view.scale, 0.7);
  view.tx = vw * 0.38 - (pl.x + pl.w / 2) * view.scale;
  view.ty = vh / 2 - (pl.y + pl.h / 2) * view.scale;
  draw();
}
function zoomAt(cx, cy, f) {
  const r = viewport.getBoundingClientRect();
  const px = cx - r.left, py = cy - r.top;
  const ns = Math.min(Math.max(view.scale * f, 0.03), 6);
  view.tx = px - ((px - view.tx) * ns) / view.scale;
  view.ty = py - ((py - view.ty) * ns) / view.scale;
  view.scale = ns;
  draw();
}
function zoomAction(act) {
  const r = viewport.getBoundingClientRect();
  const cx = r.left + r.width / 2, cy = r.top + r.height / 2;
  if (act === "in") zoomAt(cx, cy, 1.25);
  else if (act === "out") zoomAt(cx, cy, 1 / 1.25);
  else fitView();
}
function toWorld(clientX, clientY) {
  const r = viewport.getBoundingClientRect();
  return {
    x: (clientX - r.left - view.tx) / view.scale,
    y: (clientY - r.top - view.ty) / view.scale,
  };
}

viewport.addEventListener("wheel", (e) => {
  e.preventDefault();
  zoomAt(e.clientX, e.clientY, e.deltaY < 0 ? 1.15 : 1 / 1.15);
}, { passive: false });

let dragState = null;
viewport.addEventListener("pointerdown", (e) => {
  dragState = { x: e.clientX, y: e.clientY, tx: view.tx, ty: view.ty, moved: false };
  if (viewport.setPointerCapture) viewport.setPointerCapture(e.pointerId);
});
viewport.addEventListener("pointermove", (e) => {
  if (dragState) {
    const dx = e.clientX - dragState.x, dy = e.clientY - dragState.y;
    if (Math.abs(dx) + Math.abs(dy) > 4) dragState.moved = true;
    if (dragState.moved) {
      view.tx = dragState.tx + dx;
      view.ty = dragState.ty + dy;
      viewport.classList.add("dragging");
      draw();
    }
    return;
  }
  const w = toWorld(e.clientX, e.clientY);
  const idx = pickPill(w.x, w.y);
  const lIdx = idx < 0 && view.scale > 0.4 ? pickLabel(w.x, w.y) : -1;
  if (idx !== hoverIdx || lIdx !== hoverLabel) {
    hoverIdx = idx;
    hoverLabel = lIdx;
    hoverLit =
      idx >= 0
        ? directEdges(idx)
        : lIdx >= 0
          ? new Set(scene.labels[lIdx].edgeIdxs)
          : null;
    viewport.style.cursor = idx >= 0 || lIdx >= 0 ? "pointer" : "grab";
    if (mode === "webgl2") {
      uploadRects();
      uploadEdgeState();
    }
    draw();
  }
});
function endDrag(e) {
  if (!dragState) return;
  const wasClick = !dragState.moved;
  dragState = null;
  viewport.classList.remove("dragging");
  if (wasClick && e.type === "pointerup") {
    const w = toWorld(e.clientX, e.clientY);
    const idx = pickPill(w.x, w.y);
    const lIdx = idx < 0 && view.scale > 0.4 ? pickLabel(w.x, w.y) : -1;
    if (idx >= 0) selectJob(scene.pills[idx].id);
    else if (lIdx >= 0 && scene.labels[lIdx].srcPill !== null) {
      // a trigger label selects the bridge job it belongs to
      const sp = scene.labels[lIdx].srcPill;
      flyTo(sp);
      selectJob(scene.pills[sp].id);
    } else if (selectedJob) selectJob(null); // click on empty space clears the trace
  }
}
viewport.addEventListener("pointerup", endDrag);
viewport.addEventListener("pointercancel", endDrag);
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && selectedJob && !document.querySelector(".overlay"))
    selectJob(null);
});

/* ---- minimap: overview + click/drag navigation ---- */

const miniCanvas = document.createElement("canvas");
miniCanvas.id = "minimap";
viewport.appendChild(miniCanvas);
let miniK = 0.01;
const MINI_W = 200;

function drawMini() {
  let mctx2;
  try {
    mctx2 = miniCanvas.getContext("2d");
  } catch (e) {
    return;
  }
  if (!mctx2) return;
  miniK = MINI_W / scene.size.w;
  const mh = Math.max(40, Math.min(160, Math.round(scene.size.h * miniK)));
  miniK = Math.min(miniK, mh / scene.size.h);
  if (miniCanvas.width !== MINI_W * dpr || miniCanvas.height !== mh * dpr) {
    miniCanvas.width = MINI_W * dpr;
    miniCanvas.height = mh * dpr;
    miniCanvas.style.width = MINI_W + "px";
    miniCanvas.style.height = mh + "px";
  }
  const c = mctx2;
  c.setTransform(dpr, 0, 0, dpr, 0, 0);
  c.fillStyle = css(PAL.panel, 0.92);
  c.fillRect(0, 0, MINI_W, mh);
  c.scale(miniK, miniK);
  for (const b of scene.bands) {
    c.fillStyle = css(PAL.line, 0.5);
    c.fillRect(b.x, b.y, b.w, b.h);
  }
  for (const cd of scene.cards) {
    c.fillStyle = css(cd.dim ? PAL.muted : PAL.accent, cd.dim ? 0.35 : 0.75);
    c.fillRect(cd.x, cd.y, Math.max(cd.w, 3 / miniK), Math.max(cd.h, 3 / miniK));
  }
  // current viewport
  c.strokeStyle = css(PAL.ink, 0.9);
  c.lineWidth = 1.5 / miniK;
  const vx = -view.tx / view.scale;
  const vy = -view.ty / view.scale;
  c.strokeRect(vx, vy, vw / view.scale, vh / view.scale);
}

function miniJump(e) {
  const r = miniCanvas.getBoundingClientRect();
  const wx = (e.clientX - r.left) / miniK;
  const wy = (e.clientY - r.top) / miniK;
  view.tx = vw / 2 - wx * view.scale;
  view.ty = vh / 2 - wy * view.scale;
  draw();
}
let miniDrag = false;
miniCanvas.addEventListener("pointerdown", (e) => {
  e.stopPropagation();
  miniDrag = true;
  miniCanvas.setPointerCapture(e.pointerId);
  miniJump(e);
});
miniCanvas.addEventListener("pointermove", (e) => {
  if (miniDrag) {
    e.stopPropagation();
    miniJump(e);
  }
});
miniCanvas.addEventListener("pointerup", (e) => {
  e.stopPropagation();
  miniDrag = false;
});

function resize() {
  const r = viewport.getBoundingClientRect();
  vw = Math.max(50, r.width);
  vh = Math.max(50, r.height);
  dpr = (typeof devicePixelRatio === "number" && devicePixelRatio) || 1;
  for (const cnv of [glCanvas, txtCanvas]) {
    cnv.width = Math.round(vw * dpr);
    cnv.height = Math.round(vh * dpr);
  }
  txtCtx = (() => {
    try {
      return txtCanvas.getContext("2d");
    } catch (e) {
      return null;
    }
  })();
  draw();
}

function watchTheme() {
  const onTheme = () => {
    readPalette();
    syncBuffers();
    draw();
  };
  if (typeof matchMedia === "function") {
    const mq = matchMedia("(prefers-color-scheme: dark)");
    if (mq.addEventListener) mq.addEventListener("change", onTheme);
  }
  if (typeof MutationObserver === "function") {
    new MutationObserver(onTheme).observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"],
    });
  }
}

/* ================= init ================= */

const app = document.getElementById("app");
buildTopbar();
buildSimbar();
app.appendChild(topbar);
app.appendChild(simbar);
const main = h("div", "main");
main.appendChild(viewport);
main.appendChild(panel);
app.appendChild(main);

readPalette();
buildScene();
initRenderer();
resize();
applyEval();
fitView();
watchTheme();
const wasmReady = startWasm();
startPulse();

if (typeof ResizeObserver === "function") new ResizeObserver(resize).observe(viewport);
else if (typeof addEventListener === "function") addEventListener("resize", resize);

// Test surface for the headless harness (ui/test/viewer.test.mjs). Live
// getters where state is reassigned; everything else is the object itself.
window.__glpv = window.__glpv || {};
// Live getters must be defined, not assigned (Object.assign would copy their
// current values).
Object.defineProperties(window.__glpv, {
  mode: { get: () => mode, enumerable: true },
  selectedJob: { get: () => selectedJob, enumerable: true },
  edgeMode: { get: () => edgeMode, enumerable: true },
});
Object.assign(window.__glpv, {
  G, scene, sim, view, panel, viewport, counts, simbar,
  canvases: [glCanvas, txtCanvas],
  lastEval: () => lastEval,
  lineage: () => selLineage,
  wasmActive: () => !!wasmEval,
  wasmReady,
  disableWasm() { wasmEval = null; applyEval(); },
  jobById, payloadJob, pipeOfJob, selectJob, flyTo, applyEval, evaluateAll,
  renderPanel, gateChainFor, collectClauseInputs, solveOutcomes,
  findEnablingScenario, applyScenario, buildOutcomeExplorer,
  refreshSimBar: () => refreshSimBarFn && refreshSimBarFn(),
  draw, resize,
  errors: __glpvErrors,
});
