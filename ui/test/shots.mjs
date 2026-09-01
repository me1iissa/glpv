// Opt-in debugging aid: GLPV_SHOTS=1 npm test writes a PNG of the board after
// each interaction step to target/ui-test/shots/. Needs the optional `canvas`
// package (the viewer's Canvas2D path renders the gl-layer and txt-layer
// canvases; they are composited here).
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { ROOT } from "./samples.mjs";
import { hasCanvas } from "./harness.mjs";

const require = createRequire(import.meta.url);
export const SHOTS = process.env.GLPV_SHOTS === "1" && hasCanvas;
const DIR = path.join(ROOT, "target", "ui-test", "shots");

export async function shot(glpv, name) {
  if (!SHOTS) return null;
  const { createCanvas, loadImage } = require("canvas");
  const [glLayer, txtLayer] = glpv.canvases;
  const w = glLayer.width || 1, hgt = glLayer.height || 1;
  const out = createCanvas(w, hgt);
  const ctx = out.getContext("2d");
  ctx.fillStyle = "#ffffff";
  ctx.fillRect(0, 0, w, hgt);
  for (const layer of [glLayer, txtLayer]) {
    try {
      const img = await loadImage(layer.toDataURL("image/png"));
      ctx.drawImage(img, 0, 0);
    } catch (e) {
      // a layer with no context draws nothing
    }
  }
  mkdirSync(DIR, { recursive: true });
  const file = path.join(DIR, name.replace(/[^A-Za-z0-9_.-]+/g, "_") + ".png");
  writeFileSync(file, out.toBuffer("image/png"));
  return file;
}
