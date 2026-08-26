// prelude — JS layer prepended to every run: formatting/output, host proxy, unified execution primitives
// Static asset: embedded via include_str!; editing this file edits every agent's runtime foundation
'use strict';

globalThis.__fmt = (v) => {
  if (typeof v === 'string') return v;
  if (typeof v === 'undefined') return 'undefined';
  if (typeof v === 'function') return String(v);
  // compact serialization: the result re-enters context, indentation is a human luxury paid in tokens (D023)
  try { const j = JSON.stringify(v); return j === undefined ? String(v) : j; }
  catch (e) { return String(v); }
};

// dual log write: Rust collects (final JSON stdout) + JS buffer (runBlock captures sub-block stdout)
const __rawlog = globalThis.__log;
globalThis.__log = (s) => { __rawlog(s); if (globalThis.__buf) globalThis.__buf.push(s); };
globalThis.print = (...a) => __log(a.map(__fmt).join(' '));
globalThis.console = { log: print, info: print, warn: print, error: print, debug: print };

// host proxy: unknown capability → error listing available items; trailing undefined → stripped (host-side Opt only accepts missing args);
// function errors → annotated with the function name (argument errors become locatable)
const __nsProxy = (obj, path) => new Proxy(obj, {
  get(t, k) {
    if (typeof k !== 'string') return t[k];
    if (!(k in t)) throw new Error(`${path}.${k} does not exist; available: ${Object.keys(t).join(', ')}; docs: host.help()`);
    const v = t[k];
    if (typeof v === 'function') {
      return (...a) => {
        const args = [...a];
        while (args.length && args[args.length - 1] === undefined) args.pop();
        try { return v(...args); }
        catch (e) { throw new Error(`${path}.${k}(...) call failed: ${e.message}`); }
      };
    }
    if (v && typeof v === 'object') return __nsProxy(v, `${path}.${k}`);
    return v;
  }
});
globalThis.host = __nsProxy(host, 'host');

globalThis.__tap = (p) => Promise.resolve(p).then(
  x => { globalThis.__settled = {ok: true, v: x}; },
  e => { globalThis.__settled = {ok: false,
      e: (e && e.stack) ? `${e.message ?? String(e)}\n${e.stack}` : String(e)}; }
);

// ===== Unified execution primitive: every agent layer's code runs through it, same semantics as the kernel run =====
// (top-level return submits / top-level await legal / last-expression fallback / stdout captured / errors carry stack)
globalThis.runBlock = async (code) => {
  const wantsBody = code.split('\n').some(l => {
    const t = l.trimStart();
    return t.startsWith('return') || t.startsWith('await ') || t === 'await';
  });
  const src = wantsBody ? `(async () => {\n${code}\n})()` : code;
  globalThis.__buf = [];
  const t0 = Date.now();
  try {
    let v = (0, eval)(src); // indirect eval: global scope, no closure leak
    if (v && typeof v.then === 'function') v = await v;
    const stdout = globalThis.__buf.join('\n');
    globalThis.__buf = [];
    return { ok: true, result: __fmt(v), stdout, elapsed_ms: Date.now() - t0 };
  } catch (e) {
    const stdout = globalThis.__buf.join('\n');
    globalThis.__buf = [];
    return { ok: false, error: (e && e.stack) ? `${e.message}\n${e.stack}` : String(e), stdout, elapsed_ms: Date.now() - t0 };
  }
};

// ===== Sub-agent loop: main/sub agents are the same thing (different context + whoever drives the loop) =====
// Reply→code extraction delegates to the kernel's single implementation (agent::extract_run exposed as
// host.__extractRun, internal); the ```run-fence protocol is defined once, never maintained twice.
globalThis.spawnAgent = async (task, opts = {}) => {
  const maxTurns = opts.maxTurns ?? 8;
  const msgs = [{ role: 'user', content: task }];
  for (let turn = 1; turn <= maxTurns; turn++) {
    if (turn === maxTurns) {
      // convergence guardrail: final turn bans run blocks, forcing a final answer (appended message keeps history append-only)
      msgs.push({ role: 'user', content: '(FINAL TURN: no more run blocks — reply with your best answer in the task\'s FINAL format now.)' });
    }
    const reply = await host.llm.chat(msgs, opts.system);
    msgs.push({ role: 'assistant', content: reply });
    const done = reply.trim().startsWith('FINAL:'); // positive stop signal — nothing after it is extracted
    const code = done ? null : host.__extractRun(reply);
    if (code && turn < maxTurns) {
      const out = await runBlock(code);
      msgs.push({ role: 'user', content: JSON.stringify(out).slice(0, 1500) });
    } else {
      return { answer: reply, turns: turn };
    }
  }
  return { answer: null, turns: maxTurns, error: 'max turns reached' };
};
