// prelude — JS layer prepended to every run: formatting, host proxy, and bounded output
// Static asset: embedded via include_str!; editing this file edits every runtime foundation
'use strict';

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
    if (!(k in t)) throw new Error(`${path}.${k} does not exist; available: ${Object.keys(t).join(', ')}`);
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

host.fs.replace = (path, oldText, newText, options = {}) => {
  if (typeof path !== 'string' || typeof oldText !== 'string' || typeof newText !== 'string') {
    throw new Error('host.fs.replace(path, oldText, newText[, {all}]) requires string path and text arguments');
  }
  if (!oldText.length) throw new Error('host.fs.replace old text must not be empty');
  if (oldText === newText) throw new Error('host.fs.replace produced identical content; no changes made');
  if (options === null || typeof options !== 'object' || Array.isArray(options) ||
      (options.all !== undefined && typeof options.all !== 'boolean')) {
    throw new Error('host.fs.replace options must be {all: boolean}');
  }
  const content = host.fs.text(path);
  let count = 0;
  for (let at = 0; (at = content.indexOf(oldText, at)) !== -1; at += oldText.length) count++;
  if (!count) throw new Error(`host.fs.replace old text was not found in ${path}`);
  if (count > 1 && !options.all) {
    throw new Error(`host.fs.replace old text matched ${count} times in ${path}; provide more context or use {all: true}`);
  }
  let updated;
  if (options.all) {
    updated = content.split(oldText).join(newText);
  } else {
    const at = content.indexOf(oldText);
    updated = content.slice(0, at) + newText + content.slice(at + oldText.length);
  }
  return {path, replacements: options.all ? count : 1, bytes: host.fs.write(path, updated)};
};

// scan()/walk(): flatten host chunks into per-item async iterators — for-await over scan
// yields {file, no, text} lines, over walk yields {file, size} entries, one by one.
{
  const __streamify = (rawFn, name) => (path, opts) => {
    let raw;
    try { raw = rawFn(path, opts); }
    catch (e) { throw new Error(String(e && e.message || e).replace(`host.fs.${name}(...) call failed: `, '')); }
    return {
      [Symbol.asyncIterator]: async function* () {
        while (true) {
          const batch = await raw.next();
          const items = Array.isArray(batch) ? batch : batch.items;
          if (!items.length) {
            if (Array.isArray(batch) || batch.done) return;
            continue;
          }
          for (const it of items) yield it;
        }
      },
    };
  };
  host.fs.scan = __streamify(host.fs.scan, 'scan');
  host.fs.walk = __streamify(host.fs.walk, 'walk');
}
