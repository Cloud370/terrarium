// prelude — JS layer prepended to every run: formatting, host proxy, and bounded output
// Static asset: embedded via include_str!; editing this file edits every runtime foundation
'use strict';

const __rawlog = globalThis.__log;
globalThis.__log = (s) => { __rawlog(s); };
globalThis.print = (...a) => __log(a.map((v) => {
  if (typeof v === 'string') return v;
  if (typeof v === 'undefined') return 'undefined';
  if (typeof v === 'function') return String(v);
  try { const j = JSON.stringify(v); return j === undefined ? String(v) : j; }
  catch (_) { return String(v); }
}).join(' '));
globalThis.console = { log: print, info: print, warn: print, error: print, debug: print };

// Unknown capabilities fail with the live surface; trailing undefined arguments are omitted.
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

// scan(): flatten host chunks into a per-line async iterator — for-await yields {file, no, text} one by one.
{
  const __scanRaw = host.fs.scan;
  host.fs.scan = (path, opts) => {
    let raw;
    try { raw = __scanRaw(path, opts); }
    catch (e) { throw new Error(String(e && e.message || e).replace(/^host\.fs\.scan\(\.\.\.\) call failed: /, '')); }
    return {
      [Symbol.asyncIterator]: async function* () {
        while (true) {
          const lines = await raw.next();
          if (!lines.length) return;
          for (const l of lines) yield l;
        }
      },
    };
  };
}
