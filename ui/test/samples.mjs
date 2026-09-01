// The two sample scans the tests run against, produced by prepare.mjs (or by
// the CI `samples` job): the test fixtures crawled with --all, and the demo
// set crawled from its entry project.
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");

export const SAMPLES = [
  { name: "fixtures", dir: "target/ui-test/out-fixtures" },
  { name: "demo", dir: "target/ui-test/out-demo" },
];

export function samplePaths(sample) {
  const dir = path.join(ROOT, sample.dir);
  return { html: path.join(dir, "index.html"), json: path.join(dir, "graph.json") };
}

/** Returns { html, jsonText, graph } or throws with the command to run. */
export function readSample(sample) {
  const p = samplePaths(sample);
  if (!existsSync(p.html) || !existsSync(p.json)) {
    throw new Error(
      `sample "${sample.name}" is missing under ${sample.dir}; run \`npm run prepare-samples\` ` +
        "(or unset GLPV_SKIP_PREPARE)",
    );
  }
  const jsonText = readFileSync(p.json, "utf8");
  return { html: readFileSync(p.html, "utf8"), jsonText, graph: JSON.parse(jsonText) };
}
