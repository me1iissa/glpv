// Drives the embedded wasm evaluator (ui/eval-wasm.b64) from node, exactly
// the way the viewer's startWasm() does — except that a failing call throws
// instead of silently falling back to the JS mirror.
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
export const B64_PATH = path.join(here, "..", "eval-wasm.b64");

/** Instantiate the module over one graph (JSON text). */
export async function loadEvaluator(graphJsonText) {
  const b64 = readFileSync(B64_PATH, "utf8").trim();
  if (!b64) throw new Error("ui/eval-wasm.b64 is empty; run scripts/build-wasm.sh");
  const { instance } = await WebAssembly.instantiate(Buffer.from(b64, "base64"), {});
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
  const [gp, gl] = put(graphJsonText);
  const ok = ex.glpv_init(gp, gl);
  ex.glpv_dealloc(gp, gl);
  if (ok !== 0) throw new Error("glpv_init rejected the graph (code " + ok + ")");
  return {
    exports: Object.keys(ex).sort(),
    /** sim: the `Sim` payload (see wasmSimOf in ui/eval.js) → parsed `Out`. */
    eval(sim) {
      const [p, l] = put(JSON.stringify(sim));
      const rp = ex.glpv_eval(p, l);
      ex.glpv_dealloc(p, l);
      if (!rp) throw new Error("glpv_eval returned null for " + JSON.stringify(sim));
      const rl = ex.glpv_result_len();
      const out = dec.decode(new Uint8Array(ex.memory.buffer, rp, rl));
      ex.glpv_dealloc(rp, rl);
      return JSON.parse(out);
    },
  };
}
