// Builds the sample graphs the UI tests consume (npm `pretest`): materialise
// the fixture and demo repositories, then scan them to HTML + JSON under
// target/ui-test/. Set GLPV_SKIP_PREPARE=1 when the samples already exist
// (CI downloads them as artifacts of the `samples` job).
import { spawnSync } from "node:child_process";
import { ROOT } from "./samples.mjs";

if (process.env.GLPV_SKIP_PREPARE === "1") {
  console.log("prepare: skipped (GLPV_SKIP_PREPARE=1)");
  process.exit(0);
}

const steps = [
  ["run", "-q", "-p", "glpv-cli", "--example", "build_fixtures", "--",
    "tests/fixtures/projects", "target/ui-test/fixtures"],
  ["run", "-q", "-p", "glpv-cli", "--example", "build_fixtures", "--",
    "demo/projects", "target/ui-test/demo"],
  ["run", "-q", "-p", "glpv-cli", "--", "scan", "--projects", "target/ui-test/fixtures", "--all",
    "--changed-file", "src/main.rs", "--changed-file", "docs/sub/x.md",
    "-o", "target/ui-test/out-fixtures", "--format", "html,json"],
  ["run", "-q", "-p", "glpv-cli", "--", "scan", "--projects", "target/ui-test/demo",
    "--entry", "pipelines-demo/shop", "-o", "target/ui-test/out-demo", "--format", "html,json"],
];

for (const args of steps) {
  console.log("prepare: cargo " + args.join(" "));
  const r = spawnSync("cargo", args, { cwd: ROOT, stdio: "inherit" });
  if (r.status !== 0) {
    console.error(`prepare: \`cargo ${args.join(" ")}\` failed with status ${r.status}`);
    process.exit(r.status ?? 1);
  }
}
