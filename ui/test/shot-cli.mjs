#!/usr/bin/env node
// Render a scanned index.html headlessly and write a PNG of the board — the
// same jsdom + canvas path the viewer tests use (GLPV_SHOTS). For docs.
//
//   node ui/test/shot-cli.mjs <index.html> <out.png> [--width 1600] [--height 1000]
//                             [--select <job name>] [--hash <#state>]
import { readFileSync, renameSync, mkdirSync } from "node:fs";
import path from "node:path";
import { loadViewer, hasCanvas } from "./harness.mjs";

// shots.mjs reads GLPV_SHOTS when it loads: set it first, import it after.
process.env.GLPV_SHOTS = "1";
const { shot } = await import("./shots.mjs");

const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : dflt;
};
const [htmlPath, outPath] = args.filter((a, i) => !a.startsWith("--") && !(i > 0 && args[i - 1].startsWith("--")));
if (!htmlPath || !outPath) {
  console.error("usage: shot-cli.mjs <index.html> <out.png> [--width N] [--height N] [--select job] [--hash #state]");
  process.exit(2);
}
if (!hasCanvas) {
  console.error("the optional `canvas` package is required for screenshots (npm install canvas)");
  process.exit(1);
}
const width = Number(opt("--width", 1600));
const height = Number(opt("--height", 1000));
const v = await loadViewer(readFileSync(htmlPath, "utf8"), { wasm: true, width, height, hash: opt("--hash", "") });
const select = opt("--select", null);
if (select) {
  const job = v.glpv.G.pipelines.flatMap((p) => p.jobs).find((j) => j.name === select);
  if (!job) {
    console.error(`no job named ${select}`);
    process.exit(1);
  }
  v.glpv.selectJob(job.id);
  v.glpv.flyTo(v.glpv.scene.pillByJob.get(job.id));
}
// shots.mjs writes under target/ui-test/shots/<name>.png; move to the requested path
const tmp = await shot(v.glpv, "shot-cli-" + process.pid);
if (!tmp) {
  console.error("screenshot not produced");
  process.exit(1);
}
mkdirSync(path.dirname(outPath), { recursive: true });
renameSync(tmp, outPath);
if (v.errors.length) {
  console.error("viewer errors:\n" + v.errors.join("\n"));
  process.exit(1);
}
console.log(`${outPath}: ${width}x${height}, ${v.glpv.scene.pills.length} jobs, ${v.glpv.scene.cards.length} pipelines`);
v.close();
