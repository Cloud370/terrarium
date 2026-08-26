//! Host API registry: the single source of truth where docs and implementation live together.
//! Both host.help() and the contract ({{HOST_API}} in CONTRACT.md) are generated from it — they can never drift.

use rquickjs::{Ctx, Function, Object};

pub struct HostDoc {
    pub sig: &'static str,
    pub doc: &'static str,
}

pub const HOST_API: &[HostDoc] = &[
    HostDoc { sig: "host.fs.list(dir)", doc: "→ [\"name\\tbytes\", ...] one directory level, sorted; directories appear as \"name/\\tdir\". File sizes come free here — there is no stat. Paths must be under a mount root (e.g. /proj/)" },
    HostDoc { sig: "host.fs.read(path, from, to)", doc: "→ lines from..to (1-based, inclusive) as \"N: text\"; to=Infinity reads to EOF. A trailing footer appears iff more lines follow (no footer = you saw EOF). Lines over 2000 chars are cut. THE ONLY read form — pick a window, never dump" },
    HostDoc { sig: "host.fs.text(path)", doc: "→ whole file as one LF-normalized string — the PROGRAM-side channel (no line numbers, no caps; read windows are for your eyes, this is for your programs). Spot-edit idiom: host.fs.write(p, host.fs.text(p).replace(old, new)) — string-arg replace hits the first occurrence, check uniqueness first when it matters. Heap (64MB) is the only bound" },
    HostDoc { sig: "host.fs.search(needle, max, skips?)", doc: "→ [\"file:line:text\", ...] streaming substring scan of all mounted text files, capped at max (default 20). skips = {skipDirs: [...], skipExts: [...]} REPLACES the defaults (dirs: target .git node_modules dist ref; exts: png jpg zip lock bin …) — pass {skipDirs: [], skipExts: []} to scan everything, incl. Cargo.lock" },
    HostDoc { sig: "host.fs.write(path, content)", doc: "→ bytes written — that number is the receipt, no need to read back. Text files only; atomic (temp+rename), auto-creates parent dirs. Allowed ONLY under mounts declared :rw at launch; anywhere else it is a policy denial — report it, don't retry another way" },
    HostDoc { sig: "host.llm.call(prompt, system)", doc: "→ Promise<string> one-shot nested LLM call (seconds each). Independent calls run CONCURRENTLY under Promise.all — wall time = max, not sum" },
    HostDoc { sig: "host.llm.chat(messages, system)", doc: "→ Promise<string> multi-turn conversation: messages = [{role:'user'|'assistant', content}, ...]. History must stay strictly append-only or the provider prefix cache stops hitting. This contract is auto-prepended to `system`" },
    HostDoc { sig: "host.help()", doc: "→ this list, generated from the live registry" },
];

/// Registers host.help
pub fn install<'js>(ctx: &Ctx<'js>, host: &Object<'js>) -> rquickjs::Result<()> {
    let help_fn = Function::new(ctx.clone(), || {
        HOST_API
            .iter()
            .map(|d| format!("{}\n    {}", d.sig, d.doc))
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    host.set("help", help_fn)?;
    Ok(())
}

/// API list for the contract (one per line: signature — doc)
pub fn api_lines() -> String {
    HOST_API
        .iter()
        .map(|d| format!("- {} — {}\n", d.sig, d.doc))
        .collect()
}
