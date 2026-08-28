//! Host API registry: the single source of truth where docs and implementation live together.
//! The contract ({{HOST_API}} in prompts/contract.md) is generated from it — docs can never drift
//! from the surface. There is no in-cage help(): the contract already carries these bytes every turn.

pub struct HostDoc {
    pub sig: &'static str,
    pub doc: &'static str,
}

pub const HOST_API: &[HostDoc] = &[
    HostDoc { sig: "host.fs.list(dir)", doc: "→ [{name, type, size}, ...] one directory level, sorted by name; type is file, directory, symlink, or other; size is the file size in bytes for regular files and null otherwise. Inspect fields directly; do not parse display strings. Paths must be under a mount root (e.g. /proj/)" },
    HostDoc { sig: "host.fs.read(path, from, to)", doc: "→ lines from..to (1-based, inclusive) as \"N: text\"; to=Infinity reads to EOF. A trailing footer appears iff more lines follow (no footer = you saw EOF). Lines over 2000 chars are cut; windows over 64MB in total are rejected — narrow the range. THE ONLY read form — pick a window, never dump" },
    HostDoc { sig: "host.fs.text(path)", doc: "→ whole file as one LF-normalized string, leading BOM stripped — the PROGRAM-side channel (no line numbers, no caps; read windows are for your eyes, this is for your programs). Spot-edit idiom: host.fs.write(p, host.fs.text(p).replace(old, new)) — string-arg replace hits the first occurrence, check uniqueness first when it matters. Files over 64MB are rejected (they'd never fit the cage heap) — use read windows or scan" },
    HostDoc { sig: "host.fs.scan(path, {glob?, skipDirs?, skipExts?, gitignore?, hidden?})", doc: "→ async stream of EVERY line of every text file under path: `for await (const l of host.fs.scan(\"/proj\", {glob: \"*.rs\"}))` yields {file, no, text}. YOUR predicate is the matcher — l.text.includes(needle), /regex/i.test(l.text), file filters, cross-line logic, all plain JS. glob: \"*.rs\" behaves like grep --include, \"src/**/*.rs\" like rg --glob (host-side — non-matching files are never opened). Defaults are ripgrep's, the convention you already know: .gitignore respected (at/below the scan root), hidden (dot) entries skipped, binaries detected by content (a NUL byte, not by name), symlinks never followed. Overrides: {gitignore: false} = --no-ignore, {hidden: true} = --hidden. {skipDirs, skipExts} add extra host-side prunes on top (empty by default). Lines are delivered WHOLE, heap is the bound — a minified one-liner arrives as one long line; locate with l.text.indexOf(needle) and return a slice around it, not the whole line. Single lines over 8MB are cut (marked '…'). ~1000 lines per await so huge trees stream instead of freezing; break when you have enough. Scope the path like you scope reads" },
    HostDoc { sig: "host.fs.write(path, content)", doc: "→ bytes written — that number is the receipt, no need to read back. Text files only; atomic (temp+rename), auto-creates parent dirs. An existing target's BOM/CRLF are preserved and counted (your program works in LF/no-BOM; write restores the target's own shape). Allowed ONLY under mounts declared :rw at launch; anywhere else it is a policy denial — report it, don't retry another way" },
    HostDoc { sig: "host.agent.answer(text)", doc: "→ marks the current agent task complete and returns the supplied text to the operator; returning from a program only ends that run" },

];

/// API list for the contract (one per line: signature — doc)
pub fn api_lines() -> String {
    HOST_API
        .iter()
        .map(|d| format!("- {} — {}\n", d.sig, d.doc))
        .collect()
}
