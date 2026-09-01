// jsdom harness for the viewer: loads a scanned index.html fragment into a
// window with just enough platform stubbed for app.js to boot (no WebGL2;
// Canvas2D when the optional `canvas` package is installed), and exposes
// window.__glpv plus every error the page produced.
import { JSDOM, VirtualConsole } from "jsdom";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

export const hasCanvas = (() => {
  try {
    require("canvas");
    return true;
  } catch (e) {
    return false;
  }
})();

const CANVAS_NOT_IMPLEMENTED = /Not implemented: HTMLCanvasElement/;

/**
 * @param {string} fragmentHtml the CLI's index.html (a body fragment)
 * @param {{wasm?: boolean, width?: number, height?: number}} opts
 * @returns {Promise<{window, glpv, errors: string[], close(): void}>}
 */
export async function loadViewer(fragmentHtml, { wasm = true, width = 1400, height = 900 } = {}) {
  const errors = [];
  const virtualConsole = new VirtualConsole();
  virtualConsole.on("jsdomError", (e) => {
    const text = String((e && e.stack) || (e && e.message) || e);
    if (!hasCanvas && CANVAS_NOT_IMPLEMENTED.test(text)) return;
    errors.push("jsdomError: " + text);
  });
  virtualConsole.on("error", (...a) => errors.push("console.error: " + a.map(String).join(" ")));

  const html = `<!doctype html><html><head><meta charset="utf-8"></head><body>${fragmentHtml}</body></html>`;
  const dom = new JSDOM(html, {
    runScripts: "dangerously",
    pretendToBeVisual: true,
    url: "http://glpv.test/",
    virtualConsole,
    beforeParse(window) {
      window.addEventListener("error", (e) => {
        errors.push("window.error: " + String((e.error && e.error.stack) || e.message || e));
      });
      if (wasm) {
        window.WebAssembly = WebAssembly;
        window.TextEncoder = TextEncoder;
        window.TextDecoder = TextDecoder;
      } else {
        delete window.WebAssembly;
      }
      const proto = window.Element.prototype;
      if (!proto.setPointerCapture) proto.setPointerCapture = () => {};
      if (!proto.releasePointerCapture) proto.releasePointerCapture = () => {};
      // jsdom does no layout: every element reports the viewport size.
      proto.getBoundingClientRect = () => ({
        x: 0, y: 0, left: 0, top: 0, width, height, right: width, bottom: height,
        toJSON() { return {}; },
      });
    },
  });
  const window = dom.window;
  const glpv = window.__glpv;
  if (!glpv) {
    throw new Error("viewer did not boot (window.__glpv missing):\n" + errors.join("\n"));
  }
  if (wasm) {
    const ok = await glpv.wasmReady;
    if (ok !== true || !glpv.wasmActive()) {
      throw new Error("wasm evaluator did not come up:\n" + errors.join("\n"));
    }
  }
  return { window, glpv, errors, close: () => window.close() };
}

/** Rectangle overlap for scene labels: x is the centre, y the top edge. */
export function labelsOverlap(a, b) {
  return Math.abs(a.x - b.x) < (a.w + b.w) / 2 && a.y < b.y + b.h && a.y + a.h > b.y;
}

/** Overlap between a label (centre-x) and a pill (left-x). */
export function labelHitsPill(l, p) {
  const lx = l.x - l.w / 2;
  return lx < p.x + p.w && lx + l.w > p.x && l.y < p.y + p.h && l.y + l.h > p.y;
}
