/* glpv evaluator mirror.
 *
 * A faithful JS port of glpv-core/src/rules/ (expr.rs, mod.rs) plus the
 * simulated CI variable tables mirrored from glpv-wasm. The viewer HTML embeds
 * this file ahead of app.js in one shared script scope; ui/test/parity.test.mjs
 * `require()`s it and checks it against the Rust engine (the expression cases
 * in tests/parity/expr-cases.json, and the embedded wasm build over whole
 * graphs). It must therefore stay free of DOM and viewer state: everything it
 * needs arrives as a parameter.
 *
 * Regex subset. GitLab evaluates `=~` with Ruby's engine; the canonical Rust
 * engine uses the `regex` crate (RE2 semantics, no lookaround, no
 * backreferences). The viewer follows the Rust engine: re2ToJs() rejects what
 * the regex crate rejects and translates \A \z \a \x{..} \pL (?P<..> and POSIX
 * classes to JS syntax. Documented residue (never exercised by the parity
 * cases): the `x` flag, flags set mid-pattern such as (?i:...) or (?-i), bare
 * script names in \p{..}, `.` versus "\r", and class set operations such as
 * [a-z&&[^aeiou]].
 */
"use strict";

const SOURCES = [
  "push", "merge_request_event", "schedule", "web", "api", "trigger",
  "pipeline", "parent_pipeline", "chat", "webide", "external",
  "external_pull_request_event",
];

/* ================= rules:if evaluator (port of expr.rs) ================= */

const UNKNOWN = { t: "unknown" };
const NIL = { t: "nil" };
const MAX_TOKENS = 100;

function lex(expr) {
  const out = [];
  let i = 0;
  const err = (m) => { throw new Error(m); };
  while (i < expr.length) {
    // Checked before the whitespace skip, like the Rust lexer: the limit
    // trips only when input remains after the 101st token.
    if (out.length > MAX_TOKENS) err("more than " + MAX_TOKENS + " tokens");
    const c = expr[i];
    if (c === " " || c === "\t" || c === "\n" || c === "\r") { i++; continue; }
    if (c === "(") { out.push({ k: "(" }); i++; continue; }
    if (c === ")") { out.push({ k: ")" }); i++; continue; }
    if (c === "$") {
      let j = i + 1;
      while (j < expr.length && /[A-Za-z0-9_]/.test(expr[j])) j++;
      if (j === i + 1) err("`$` with no variable name");
      out.push({ k: "var", v: expr.slice(i + 1, j) });
      i = j; continue;
    }
    if (c === '"' || c === "'") {
      // Non-greedy, no escape processing — exactly GitLab's lexer.
      const end = expr.indexOf(c, i + 1);
      if (end < 0) err("unterminated string");
      out.push({ k: "str", v: expr.slice(i + 1, end) });
      i = end + 1; continue;
    }
    if (c === "/") {
      let j = i + 1, prevBs = false, close = -1;
      while (j < expr.length) {
        if (expr[j] === "/" && !prevBs) { close = j; break; }
        prevBs = expr[j] === "\\" && !prevBs;
        j++;
      }
      if (close < 0) err("unterminated regex");
      let end = close + 1;
      while (end < expr.length && /[ismU]/.test(expr[end])) end++;
      out.push({ k: "re", v: expr.slice(i, end) });
      i = end; continue;
    }
    if (c === "=") {
      if (expr[i + 1] === "=") { out.push({ k: "==" }); i += 2; continue; }
      if (expr[i + 1] === "~") { out.push({ k: "=~" }); i += 2; continue; }
      err("stray `=`");
    }
    if (c === "!") {
      if (expr[i + 1] === "=") { out.push({ k: "!=" }); i += 2; continue; }
      if (expr[i + 1] === "~") { out.push({ k: "!~" }); i += 2; continue; }
      out.push({ k: "!" }); i++; continue;
    }
    if (c === "&") {
      if (expr[i + 1] === "&") { out.push({ k: "&&" }); i += 2; continue; }
      err("stray `&`");
    }
    if (c === "|") {
      if (expr[i + 1] === "|") { out.push({ k: "||" }); i += 2; continue; }
      err("stray `|`");
    }
    if (expr.startsWith("null", i)) { out.push({ k: "null" }); i += 4; continue; }
    if (expr.startsWith("true", i)) { out.push({ k: "bool", v: true }); i += 4; continue; }
    if (expr.startsWith("false", i)) { out.push({ k: "bool", v: false }); i += 5; continue; }
    // Whole code point, so astral characters read like Rust's `char`.
    err("unexpected character `" + String.fromCodePoint(expr.codePointAt(i)) + "`");
  }
  return out;
}

// Lower binds tighter (GitLab's own numbering).
const PREC = { "!": 1, "==": 10, "!=": 10, "=~": 10, "!~": 10, "&&": 11, "||": 12 };

function toRpn(tokens) {
  const out = [], ops = [];
  for (const t of tokens) {
    if (["var", "str", "re", "null", "bool"].includes(t.k)) out.push(t);
    else if (t.k === "(") ops.push(t);
    else if (t.k === ")") {
      for (;;) {
        const op = ops.pop();
        if (!op) throw new Error("unbalanced parentheses");
        if (op.k === "(") break;
        out.push(op);
      }
    } else {
      const p = PREC[t.k];
      while (ops.length) {
        const top = ops[ops.length - 1];
        const tp = PREC[top.k];
        if (tp === undefined) break;
        // Unary ! is right-associative; pop only tighter ops.
        if ((tp <= p && top.k !== "!") || tp < p) out.push(ops.pop());
        else break;
      }
      ops.push(t);
    }
  }
  while (ops.length) {
    const op = ops.pop();
    if (op.k === "(") throw new Error("unbalanced parentheses");
    out.push(op);
  }
  if (!out.length) throw new Error("empty expression");
  return out;
}

/** Ruby truthiness: only nil and false are falsy ("" is truthy here!). */
function rubyTruthy(v) {
  if (v.t === "nil") return "false";
  if (v.t === "bool") return v.v ? "true" : "false";
  if (v.t === "unknown") return "unknown";
  return "true"; // strings (even empty) and regexes are truthy in Ruby
}
// Unicode White_Space, the set Rust's char::is_whitespace uses (JS \s
// differs on U+0085 and U+FEFF).
const NON_BLANK = /[^\t\n\v\f\r \u0085\u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000]/;
/** Rails `present?`, applied to the final value: blank strings are falsy. */
function present(v) {
  if (v.t === "nil") return "false";
  if (v.t === "bool") return v.v ? "true" : "false";
  if (v.t === "unknown") return "unknown";
  if (v.t === "str") return NON_BLANK.test(v.v) ? "true" : "false";
  return "true";
}
function triVal(t) {
  return t === "unknown" ? UNKNOWN : { t: "bool", v: t === "true" };
}
function notTri(t) {
  return t === "unknown" ? "unknown" : t === "true" ? "false" : "true";
}

const POSIX_CLASS = {
  alnum: "a-zA-Z0-9", alpha: "a-zA-Z", ascii: "\\x00-\\x7F", blank: "\\t ",
  cntrl: "\\x00-\\x1F\\x7F", digit: "0-9", graph: "\\x21-\\x7E", lower: "a-z",
  print: "\\x20-\\x7E", punct: "!-\\/:-@\\[-`{-~", space: "\\t\\n\\v\\f\\r ",
  upper: "A-Z", word: "\\w", xdigit: "0-9A-Fa-f",
};

/**
 * Translate a regex-crate pattern body to JS (`u` mode) syntax, or throw when
 * the regex crate would reject it. See the file header for the subset.
 * Returns { source, flags } where flags come from a leading (?ims) group.
 */
function re2ToJs(body) {
  let out = "", flags = "", i = 0, inClass = false;
  const n = body.length;
  const fail = (m) => { throw new Error(m); };
  for (;;) {
    const lead = /^\(\?([imsU]+)\)/.exec(body.slice(i));
    if (!lead) break;
    for (const f of lead[1]) if ("ims".includes(f) && !flags.includes(f)) flags += f;
    i += lead[0].length;
  }
  while (i < n) {
    const c = body[i];
    if (c === "\\") {
      const d = body[i + 1];
      if (d === undefined) fail("trailing backslash");
      if (/[0-9kZGceNQE]/.test(d)) fail("unsupported escape \\" + d);
      if (d === "A") { if (inClass) fail("\\A in class"); out += "(?<![\\s\\S])"; i += 2; continue; }
      if (d === "z") { if (inClass) fail("\\z in class"); out += "(?![\\s\\S])"; i += 2; continue; }
      if (d === "a") { out += "\\x07"; i += 2; continue; }
      if (d === "x") {
        if (body[i + 2] === "{") {
          const end = body.indexOf("}", i + 3);
          if (end < 0) fail("unterminated \\x{");
          out += "\\u{" + body.slice(i + 3, end) + "}"; i = end + 1; continue;
        }
        const hex = body.slice(i + 2, i + 4);
        if (!/^[0-9A-Fa-f]{2}$/.test(hex)) fail("bad \\x escape");
        out += "\\x" + hex; i += 4; continue;
      }
      if (d === "u" || d === "U") {
        if (body[i + 2] === "{") {
          const end = body.indexOf("}", i + 3);
          if (end < 0) fail("unterminated \\u{");
          out += "\\u{" + body.slice(i + 3, end) + "}"; i = end + 1; continue;
        }
        const len = d === "u" ? 4 : 8;
        const hex = body.slice(i + 2, i + 2 + len);
        if (!new RegExp("^[0-9A-Fa-f]{" + len + "}$").test(hex)) fail("bad \\" + d + " escape");
        out += "\\u{" + hex + "}"; i += 2 + len; continue;
      }
      if (d === "p" || d === "P") {
        if (body[i + 2] === "{") {
          const end = body.indexOf("}", i + 3);
          if (end < 0) fail("unterminated \\p{");
          out += "\\" + d + "{" + body.slice(i + 3, end) + "}"; i = end + 1; continue;
        }
        const L = body[i + 2];
        if (!L || !/[A-Za-z]/.test(L)) fail("bad \\p escape");
        out += "\\" + d + "{" + L + "}"; i += 3; continue;
      }
      if (d === "b" || d === "B") { if (inClass) fail("\\b in class"); out += "\\" + d; i += 2; continue; }
      if ("dDsSwWfnrtv".includes(d)) { out += "\\" + d; i += 2; continue; }
      if (/[A-Za-z]/.test(d)) fail("unknown escape \\" + d);
      if (d.codePointAt(0) > 127) fail("escaped non-ASCII character");
      if ("^$\\.*+?()[]{}|/".includes(d)) { out += "\\" + d; i += 2; continue; }
      if (d === "-") { out += inClass ? "\\-" : "-"; i += 2; continue; }
      // Any other escaped ASCII punctuation is a literal in the regex crate;
      // JS `u` mode only accepts it as a hex escape.
      out += "\\x" + d.charCodeAt(0).toString(16).padStart(2, "0"); i += 2; continue;
    }
    if (inClass) {
      if (c === "]") { inClass = false; out += c; i++; continue; }
      if (c === "[") {
        const m = /^\[:(\^?)([a-z]+):\]/.exec(body.slice(i));
        if (!m) fail("nested class");
        if (m[1] || !POSIX_CLASS[m[2]]) fail("unsupported POSIX class");
        out += POSIX_CLASS[m[2]]; i += m[0].length; continue;
      }
      const d = body[i + 1];
      if ((c === "&" || c === "-" || c === "~") && d === c) fail("class set operation");
      out += c; i++; continue;
    }
    if (c === "[") {
      inClass = true; out += c; i++;
      if (body[i] === "^") { out += "^"; i++; }
      if (body[i] === "]") { out += "\\]"; i++; } // leading ] is a literal
      continue;
    }
    if (c === "(") {
      if (body[i + 1] === "?") {
        const rest = body.slice(i + 2);
        if (/^(=|!|<=|<!|>|#)/.test(rest)) fail("lookaround, atomic group or comment");
        if (rest.startsWith("P=")) fail("named backreference");
        if (rest.startsWith("P<")) { out += "(?<"; i += 4; continue; }
        if (rest.startsWith("<")) { out += "(?<"; i += 3; continue; }
        if (rest.startsWith(":")) { out += "(?:"; i += 3; continue; }
        fail("inline flags are only supported at the start of the pattern");
      }
      out += "("; i++; continue;
    }
    if (c === "]") { out += "\\]"; i++; continue; } // literal in the regex crate
    if (c === "}") { out += "\\}"; i++; continue; }
    if (c === "{") {
      const m = /^\{\d+(,\d*)?\}/.exec(body.slice(i));
      if (!m) fail("malformed repetition");
      out += m[0]; i += m[0].length; continue;
    }
    out += c; i++;
  }
  if (inClass) fail("unterminated class");
  return { source: out, flags };
}

/** Compile a /body/flags literal (or a variable value of that shape). */
function compileRe(text) {
  // Same split as the Rust engine: the last "/" separates body and flags, and
  // GitLab's lexeme only allows [ismU] there; an empty body is not a regex.
  const m = /^\/([\s\S]+)\/([ismU]*)$/.exec(text);
  if (!m) return null;
  let flags = "u";
  for (const f of "ims") if (m[2].includes(f)) flags += f;
  // U (swap greediness) never changes whether a match exists; ignored.
  try {
    const t = re2ToJs(m[1].replace(/\\\//g, "/"));
    for (const f of t.flags) if (!flags.includes(f)) flags += f;
    return new RegExp(t.source, flags);
  } catch (e) {
    return null;
  }
}

function matchesRe(a, b, notes) {
  // The right side must be a regex literal, or a variable whose value has
  // /…/flags form (GitLab compiles those too). Validated before the left
  // side is consulted, like the Rust engine.
  let pat = null;
  if (b.t === "re") pat = b.v;
  else if (b.t === "str" && b.v.startsWith("/") && b.v.length > 1) pat = b.v;
  else if (b.t === "unknown") return "unknown";
  else {
    notes.push("the right side of =~ is not a /regex/; GitLab treats the match as false");
    return "false";
  }
  const re = compileRe(pat);
  if (!re) {
    notes.push("invalid regex " + pat + "; GitLab evaluates the rule as false");
    return "false";
  }
  let text;
  if (a.t === "nil") text = ""; // Ruby nil.to_s
  else if (a.t === "str") text = a.v;
  else if (a.t === "unknown") return "unknown";
  else {
    notes.push("the left side of =~ is not a string");
    return "false";
  }
  return re.test(text) ? "true" : "false";
}

/** vars: Map name -> {k:'known',v}|{k:'unset'}|{k:'unknown'} */
function evalIf(expr, vars) {
  const notes = [], varsUsed = [];
  let rpn;
  try {
    rpn = toRpn(lex(expr));
  } catch (e) {
    notes.push("expression does not parse (" + e.message + "); GitLab evaluates it as false");
    return { result: "false", varsUsed, notes };
  }
  const st = [];
  const pop = () => st.pop() || NIL;
  for (const t of rpn) {
    if (t.k === "var") {
      const s = vars.get(t.v) || { k: "unknown" };
      varsUsed.push([t.v, s]);
      st.push(s.k === "known" ? { t: "str", v: s.v } : s.k === "unset" ? NIL : UNKNOWN);
    } else if (t.k === "str") st.push({ t: "str", v: t.v });
    else if (t.k === "re") st.push({ t: "re", v: t.v });
    else if (t.k === "null") st.push(NIL);
    else if (t.k === "bool") st.push({ t: "bool", v: t.v });
    else if (t.k === "!") {
      const r = rubyTruthy(pop());
      st.push(triVal(notTri(r)));
    } else if (t.k === "==" || t.k === "!=") {
      const b = pop(), a = pop();
      let r;
      if (a.t === "unknown" || b.t === "unknown") r = "unknown";
      else if (a.t === "str" && b.t === "str") r = a.v === b.v ? "true" : "false";
      else if (a.t === "nil" && b.t === "nil") r = "true";
      else if (a.t === "bool" && b.t === "bool") r = a.v === b.v ? "true" : "false";
      else r = "false"; // cross-type Ruby == is false
      st.push(triVal(t.k === "!=" ? notTri(r) : r));
    } else if (t.k === "=~" || t.k === "!~") {
      const b = pop(), a = pop();
      const r = matchesRe(a, b, notes);
      st.push(triVal(t.k === "!~" ? notTri(r) : r));
    } else if (t.k === "&&") {
      const b = pop(), a = pop();
      const ta = rubyTruthy(a);
      st.push(ta === "false" ? a : ta === "true" ? b : UNKNOWN);
    } else if (t.k === "||") {
      const b = pop(), a = pop();
      const ta = rubyTruthy(a);
      st.push(ta === "true" ? a : ta === "false" ? b : UNKNOWN);
    }
  }
  return { result: present(pop()), varsUsed, notes };
}

/* ================= rules evaluation (port of rules/mod.rs) ================= */

function outcomeOfWhen(w) {
  if (w === "never") return "skipped";
  if (w === "manual") return "manual";
  if (w === "delayed") return "delayed";
  return "runs";
}
function andTri(a, b) {
  if (a === "false" || b === "false") return "false";
  if (a === "unknown" || b === "unknown") return "unknown";
  return "true";
}
function stateText(s) {
  return s.k === "known" ? '"' + s.v + '"' : s.k;
}
function clauseText(c) {
  const parts = [];
  if (c.if) parts.push("if: " + c.if);
  if (c.changes) parts.push("changes: [" + c.changes.join(", ") + "]");
  if (c.exists) parts.push("exists: [" + c.exists.join(", ") + "]");
  return parts.length ? parts.join(" AND ") : "(always)";
}

/**
 * Evaluate a summarised rules chain. `jobWhen` is the job-level `when`;
 * `facts` = {source, refName, isTag} feed legacy only/except; `atoms`
 * optionally decides exists/changes clauses ({exists(cl,i), changes(cl,i)} →
 * true|false|null) or forces a whole clause ({clause(cl,i)} → true|false|null,
 * used by the outcome explorer). Trace entries always carry every key so the
 * shape matches the Rust engine's JSON.
 */
function evaluateRules(summary, vars, jobWhen, facts, atoms) {
  if (summary.mode === "legacy") return evaluateLegacy(summary, jobWhen, facts);
  if (!summary.rules || !summary.rules.length) {
    return { outcome: outcomeOfWhen(jobWhen), trace: [] };
  }
  const trace = [];
  let decided = null;
  for (let index = 0; index < summary.rules.length; index++) {
    const clause = summary.rules[index];
    if (decided !== null) {
      trace.push({
        index,
        result: "not_reached",
        clause: clauseText(clause),
        when: clause.when || null,
        varsUsed: [],
        note: null,
      });
      continue;
    }
    let result = "true", varsUsed = [], note = null;
    const clauseOverride = atoms && atoms.clause ? atoms.clause(clause, index) : null;
    if (clauseOverride === true || clauseOverride === false) {
      result = clauseOverride ? "true" : "false";
    } else {
      if (clause.if) {
        const r = evalIf(clause.if, vars);
        varsUsed = r.varsUsed.map(([n, s]) => [n, stateText(s)]);
        if (r.notes.length) note = r.notes.join("; ");
        result = andTri(result, r.result);
      }
      if (clause.exists) {
        const a = atoms && atoms.exists ? atoms.exists(clause, index) : null;
        if (a === false) result = andTri(result, "false");
        else if (a !== true) {
          note = note || "exists: undecidable here";
          result = andTri(result, "unknown");
        }
      }
      if (clause.changes) {
        const a = atoms && atoms.changes ? atoms.changes(clause, index) : null;
        if (a === false) result = andTri(result, "false");
        else if (a !== true) {
          note = note || "changes: depends on the diff; undecidable statically";
          result = andTri(result, "unknown");
        }
      }
    }
    const when = clause.when || jobWhen;
    if (result === "true") decided = outcomeOfWhen(when);
    else if (result === "unknown") decided = "unknown";
    trace.push({
      index,
      result: result === "true" ? "matched" : result === "false" ? "no_match" : "unknown",
      clause: clauseText(clause),
      when,
      varsUsed,
      note,
    });
  }
  return { outcome: decided === null ? "skipped" : decided, trace };
}

/** Legacy `only`/`except`, refs lists only; anything richer is unknown. */
function evaluateLegacy(summary, jobWhen, facts) {
  const legacy = summary.rules && summary.rules[0] && summary.rules[0].legacy;
  if (!legacy) return { outcome: "unknown", trace: [] };

  const branchy = ["push", "web", "pipeline", "parent_pipeline", "trigger", "api", "schedule"];
  const matchesRef = (p) => {
    switch (p) {
      case "branches": return !facts.isTag && branchy.includes(facts.source);
      case "tags": return facts.isTag;
      case "merge_requests": return facts.source === "merge_request_event";
      case "schedules": return facts.source === "schedule";
      case "web": return facts.source === "web";
      case "api": return facts.source === "api";
      case "triggers": return facts.source === "trigger";
      case "pipelines": return facts.source === "pipeline";
      case "pushes": return facts.source === "push";
      case "external": return facts.source === "external";
      case "chat": return facts.source === "chat";
      default:
        if (p.startsWith("/")) {
          try {
            const t = re2ToJs(p.replace(/^\/+|\/+$/g, ""));
            return new RegExp(t.source, "u" + t.flags).test(facts.refName);
          } catch (e) { return null; }
        }
        return p === facts.refName;
    }
  };
  const listOf = (v, dflt) => {
    if (v === undefined || v === null) return dflt;
    if (Array.isArray(v)) return v.filter((x) => typeof x === "string");
    if (typeof v === "object" && Object.keys(v).every((k) => k === "refs")) {
      return v.refs === undefined ? [] : v.refs.filter((x) => typeof x === "string");
    }
    return null; // richer than refs → undecidable
  };
  const evalList = (list, dflt) => {
    if (list === null) return null;
    if (!list.length) return dflt;
    let any = false;
    for (const p of list) {
      const m = matchesRef(p);
      if (m === null) return null;
      if (m) any = true;
    }
    return any;
  };
  const only = evalList(listOf(legacy.only, ["branches", "tags"]), true);
  const except = evalList(listOf(legacy.except, []), false);
  let outcome, note = null;
  if (only === null || except === null) {
    outcome = "unknown";
    note = "only/except uses conditions beyond refs; undecidable";
  } else {
    outcome = only && !except ? outcomeOfWhen(jobWhen) : "skipped";
  }
  return {
    outcome,
    trace: [{
      index: 0,
      result: outcome === "skipped" ? "no_match" : outcome === "unknown" ? "unknown" : "matched",
      clause: "legacy only/except",
      when: jobWhen,
      varsUsed: [],
      note,
    }],
  };
}

/* ================= variable tables (mirror of glpv-wasm) ================= */

/** sim: {source, ref, tag, vars: [[k, v]], assumeChanges, assumeExists} */
function applySimVar(t, k, v) {
  if (!k) return;
  if (v === "(unset)") t.set(k, { k: "unset" });
  else t.set(k, { k: "known", v: String(v) });
}
function simAtoms(sim) {
  return {
    changes: () => sim.assumeChanges,
    exists: () => sim.assumeExists,
  };
}

function slugify(s) {
  return s.toLowerCase().replace(/[^a-z0-9]/gu, "-").slice(0, 63).replace(/^-+|-+$/g, "");
}
function simulated(p) {
  return p.kind === "root" || p.kind === "detached";
}
function refNameOf(p, sim) {
  if (simulated(p)) return sim.ref || p.git_ref || p.default_branch || "main";
  return p.git_ref || p.default_branch || "main";
}
function sourceOf(p, sim) {
  if (simulated(p)) return sim.source;
  if (p.kind === "child" || p.kind === "dynamic_child") return "parent_pipeline";
  return "pipeline";
}
function isTagOf(p, sim) {
  return simulated(p) ? sim.tag : false;
}
function factsOf(p, sim) {
  return { source: sourceOf(p, sim), refName: refNameOf(p, sim), isTag: isTagOf(p, sim) };
}

/** The predefined CI_* table of a pipeline (without the simulation's own vars). */
function pipelineVarTable(p, sim) {
  const t = new Map();
  const known = (k, v) => t.set(k, { k: "known", v: String(v) });
  const unset = (k) => t.set(k, { k: "unset" });
  const host = p.project.host, path = p.project.path;
  const name = path.split("/").pop();
  const ns = path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : "";
  const db = p.default_branch || "main";
  const ref = refNameOf(p, sim);
  const source = sourceOf(p, sim);
  const tag = isTagOf(p, sim);

  known("CI", "true"); known("GITLAB_CI", "true");
  known("CI_SERVER_HOST", host); known("CI_SERVER_FQDN", host);
  known("CI_SERVER_URL", "https://" + host);
  known("CI_API_V4_URL", "https://" + host + "/api/v4");
  known("CI_PROJECT_PATH", path); known("CI_PROJECT_NAME", name);
  known("CI_PROJECT_NAMESPACE", ns);
  known("CI_PROJECT_ROOT_NAMESPACE", path.split("/")[0]);
  known("CI_PROJECT_PATH_SLUG", slugify(path));
  known("CI_PROJECT_URL", "https://" + host + "/" + path);
  known("CI_DEFAULT_BRANCH", db);
  known("CI_CONFIG_PATH", p.config_path);
  known("CI_PIPELINE_SOURCE", source);
  known("CI_COMMIT_REF_NAME", ref);
  known("CI_COMMIT_REF_SLUG", slugify(ref));
  if (p.sha) { known("CI_COMMIT_SHA", p.sha); known("CI_COMMIT_SHORT_SHA", p.sha.slice(0, 8)); }
  if (tag) { known("CI_COMMIT_TAG", ref); unset("CI_COMMIT_BRANCH"); }
  else if (source === "merge_request_event") {
    unset("CI_COMMIT_TAG"); unset("CI_COMMIT_BRANCH");
    known("CI_MERGE_REQUEST_SOURCE_BRANCH_NAME", ref);
    known("CI_MERGE_REQUEST_TARGET_BRANCH_NAME", db);
  } else { unset("CI_COMMIT_TAG"); known("CI_COMMIT_BRANCH", ref); }

  for (const [k, v] of Object.entries(p.variables || {})) known(k, v);
  return t;
}
function jobVarTable(p, job, sim) {
  const t = pipelineVarTable(p, sim);
  for (const [k, v] of Object.entries(job.variables || {})) t.set(k, { k: "known", v: String(v) });
  for (const [k, v] of sim.vars) applySimVar(t, k, v);
  return t;
}

/* ================= graph-wide evaluation ================= */

// Parallel/matrix expansions carry only a stub; the first expansion of each
// base holds the shared payload (rules, YAML, provenance, variables).
function baseJobMap(G) {
  const baseJobOf = new Map();
  for (const p of G.pipelines) {
    const firstOfBase = new Map();
    for (const j of p.jobs) {
      const base = j.base_name || j.name;
      if (!firstOfBase.has(base)) firstOfBase.set(base, j);
      baseJobOf.set(j.id, firstOfBase.get(base));
    }
  }
  return baseJobOf;
}

/** The JS fallback evaluator: job id → {outcome, blockedBy?, trace}. */
function evaluateGraph(G, sim, baseJobOf) {
  const jobEval = new Map();
  for (const p of G.pipelines) {
    const facts = factsOf(p, sim);
    const pvars = pipelineVarTable(p, sim);
    for (const [k, v] of sim.vars) applySimVar(pvars, k, v);
    const wf = p.workflow_rules
      ? evaluateRules(p.workflow_rules, pvars, "on_success", facts, simAtoms(sim))
      : null;
    const cache = new Map();
    for (const j of p.jobs) {
      const src = baseJobOf.get(j.id) || j;
      let ev = cache.get(src.id);
      if (!ev) {
        ev = evaluateRules(src.rules, jobVarTable(p, src, sim), src.when, facts, simAtoms(sim));
        cache.set(src.id, ev);
      }
      if (wf && wf.outcome === "skipped") {
        ev = { ...ev, outcome: "blocked", blockedBy: "workflow:rules" };
      } else if (wf && wf.outcome === "unknown" && ev.outcome !== "skipped") {
        ev = { ...ev, outcome: "unknown", blockedBy: "workflow:rules undecided" };
      }
      jobEval.set(j.id, ev);
    }
  }
  return jobEval;
}

/** The `Sim` payload the wasm evaluator consumes. */
function wasmSimOf(sim, traceJob) {
  return {
    source: sim.source,
    ref: sim.ref,
    tag: sim.tag,
    vars: sim.vars.filter((v) => v[0]),
    trace_job: traceJob || null,
    assume_changes: sim.assumeChanges,
    assume_exists: sim.assumeExists,
  };
}

// Node (the parity test) loads this file with require(); browsers have no
// `module` and skip this.
if (typeof module === "object" && module && module.exports) {
  module.exports = {
    SOURCES, MAX_TOKENS, lex, toRpn, re2ToJs, compileRe, evalIf,
    outcomeOfWhen, andTri, stateText, clauseText, evaluateRules, evaluateLegacy,
    applySimVar, simAtoms, slugify, refNameOf, sourceOf, isTagOf, factsOf,
    pipelineVarTable, jobVarTable, baseJobMap, evaluateGraph, wasmSimOf,
  };
}
