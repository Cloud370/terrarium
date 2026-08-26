//! host.fs —— surface over real mounted directories: list / windowed read / streaming search / atomic write.
//! Mounting: --mount /proj=real-dir (read-only, the default) or --mount /proj=real-dir:rw (writable).
//! Boundary = lexical `..` guard + parent canonicalize + prefix check; writes additionally require the
//! mount to be declared :rw at launch — the kernel executes that operator decision, it never makes one (D017).

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rquickjs::function::Opt;
use rquickjs::{Ctx, Function, Object};

#[derive(Clone)]
pub struct Mount {
    pub virt: String,
    pub root: PathBuf,
    pub rw: bool,
}

const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", "dist", "ref"];
const SKIP_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "zip", "gz", "exe", "o", "a", "so", "dll", "lock", "bin",
];
const LINE_SCAN_CAP: usize = 4096; // search: skip pathological lines
const LINE_READ_CAP: usize = 2000; // read: per-line character cap — a minified file must not blow context
static TMP_CTR: AtomicU64 = AtomicU64::new(0);

/// Virtual path → (mount, relative remainder). Shared by every fs operation.
fn strip_mount<'a>(mounts: &'a [Mount], js_path: &str) -> Result<(&'a Mount, String), String> {
    for m in mounts {
        if let Some(rel) = js_path
            .strip_prefix(&m.virt)
            .or_else(|| js_path.strip_prefix(m.virt.trim_end_matches('/')))
        {
            return Ok((m, rel.trim_start_matches('/').to_string()));
        }
    }
    let avail: Vec<String> = mounts.iter().map(|m| m.virt.clone()).collect();
    Err(format!(
        "path {js_path} is not under any mount point; available: {}",
        avail.join(", ")
    ))
}

/// Virtual path → real path; escapes (including .. chains, symlinks) are always rejected
pub fn resolve_mount(mounts: &[Mount], js_path: &str) -> Result<PathBuf, String> {
    let (m, rel) = strip_mount(mounts, js_path)?;
    if rel.is_empty() {
        return m
            .root
            .canonicalize()
            .map_err(|e| format!("stat failed {js_path}: {e}"));
    }
    let joined = m.root.join(&rel);
    // target may not exist: canonicalize the parent then re-join the filename; deep .. chains resolve here and get caught
    let parent = joined.parent().unwrap_or(&m.root).to_path_buf();
    let tail = joined
        .file_name()
        .map(|f| f.to_os_string())
        .unwrap_or_default();
    let canon_parent = parent
        .canonicalize()
        .map_err(|e| format!("stat failed {js_path}: {e}"))?;
    if !canon_parent.starts_with(&m.root) {
        return Err(format!("path escapes mount root, rejected: {js_path}"));
    }
    Ok(canon_parent.join(tail))
}

fn list_dir(mounts: &[Mount], dir: &str) -> Result<Vec<String>, String> {
    let p = resolve_mount(mounts, dir)?;
    let mut out = Vec::new();
    for ent in std::fs::read_dir(&p).map_err(|e| format!("{dir}: {e}"))? {
        let ent = ent.map_err(|e| format!("{dir}: {e}"))?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let meta = ent.metadata().map_err(|e| format!("{dir}: {e}"))?;
        out.push(if meta.is_dir() {
            format!("{name}/\tdir")
        } else {
            format!("{name}\t{}", meta.len())
        });
    }
    out.sort();
    Ok(out)
}

/// Windowed read: "N: text" lines from..to (1-based inclusive), a continue-footer iff more lines follow.
fn read_window(mounts: &[Mount], path: &str, a: usize, b: usize) -> Result<String, String> {
    let p = resolve_mount(mounts, path)?;
    let f = std::fs::File::open(&p).map_err(|e| format!("{path}: {e}"))?;
    let mut out = Vec::new();
    let mut more = false;
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let ln = i + 1;
        if ln > b {
            more = true; // line b+1 exists
            break;
        }
        let line = line.map_err(|e| format!("{path}: {e}"))?;
        if ln >= a {
            if line.chars().count() > LINE_READ_CAP {
                out.push(format!(
                    "{ln}: {}…",
                    line.chars().take(LINE_READ_CAP).collect::<String>()
                ));
            } else {
                out.push(format!("{ln}: {line}"));
            }
        }
    }
    let mut s = out.join("\n");
    if more {
        s.push_str(&format!(
            "\n[more lines follow — continue with host.fs.read(\"{path}\", {}, …)]",
            b + 1
        ));
    }
    Ok(s)
}

/// Whole-file text channel for PROGRAM consumption: LF-normalized (the in-program canonical form, D019),
/// no line numbers or line caps — pair with write, which restores the target's own EOL. Heap is the only bound.
fn read_text(mounts: &[Mount], path: &str) -> Result<String, String> {
    let p = resolve_mount(mounts, path)?;
    std::fs::read_to_string(p)
        .map(|s| s.replace("\r\n", "\n"))
        .map_err(|e| format!("{path}: {e}"))
}

fn walk_text_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(PathBuf, String)>,
    depth: usize,
    skip_dirs: &[String],
    skip_exts: &[String],
) {
    if depth > 10 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in entries.flatten() {
        let p = ent.path();
        let name = ent.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if !skip_dirs.iter().any(|d| d.eq_ignore_ascii_case(&name)) {
                walk_text_files(root, &p, out, depth + 1, skip_dirs, skip_exts);
            }
        } else {
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !skip_exts.contains(&ext) {
                out.push((
                    p.clone(),
                    p.strip_prefix(root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .into_owned(),
                ));
            }
        }
    }
}

/// Streaming substring scan over all mounted text files; skip lists are caller-replaceable policy (defaults are context economy)
fn search_files(
    mounts: &[Mount],
    needle: &str,
    max: usize,
    skip_dirs: Vec<String>,
    skip_exts: Vec<String>,
) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    for mount in mounts {
        let Ok(canon) = mount.root.canonicalize() else {
            continue;
        };
        let mut tmp = Vec::new();
        walk_text_files(&canon, &canon, &mut tmp, 0, &skip_dirs, &skip_exts);
        let virt = mount.virt.trim_end_matches('/');
        for (p, rel) in tmp {
            files.push((p, format!("{virt}/{rel}")));
        }
    }
    let mut out = Vec::new();
    'outer: for (p, vpath) in files {
        let Ok(f) = std::fs::File::open(&p) else {
            continue;
        };
        for (i, line) in BufReader::new(f).lines().enumerate() {
            let Ok(line) = line else { break };
            if line.len() > LINE_SCAN_CAP {
                continue;
            }
            if line.contains(needle) {
                out.push(format!(
                    "{vpath}:{}:{}",
                    i + 1,
                    line.chars().take(72).collect::<String>()
                ));
                if out.len() >= max {
                    break 'outer;
                }
            }
        }
    }
    Ok(out)
}

/// Existing target's shape, from a 4KB sample (dsh detectLineEndings semantics): leading BOM + majority EOL.
/// The in-program canonical form is LF (read strips \r\n), so write restores what the file had.
fn detect_shape(target: &Path) -> (bool, bool) {
    let Ok(mut f) = std::fs::File::open(target) else {
        return (false, false);
    };
    let mut sample = vec![0u8; 4096];
    let n = f.read(&mut sample).unwrap_or(0);
    let sample = &sample[..n];
    let bom = sample.starts_with(&[0xEF, 0xBB, 0xBF]);
    let crlf = sample.windows(2).filter(|w| w == b"\r\n").count();
    let lf = sample.iter().filter(|&&b| b == b'\n').count() - crlf;
    (bom, crlf > lf)
}

/// Atomic write into a :rw mount: lexical `..` guard → mkdir parents → canonicalize + prefix check → temp+rename.
/// Preserves an existing target's BOM and line-ending style; returns bytes written (the receipt — no read-back needed).
fn write_file(mounts: &[Mount], js_path: &str, content: &str) -> Result<usize, String> {
    let (m, rel) = strip_mount(mounts, js_path)?;
    let virt = m.virt.trim_end_matches('/');
    if !m.rw {
        return Err(format!(
            "write denied: {virt} is a read-only mount (the operator declares --mount {virt}=…:rw at launch). \
             Policy denial, not a bug — report it in your final answer, do not retry another way."
        ));
    }
    if rel.is_empty() {
        return Err(format!(
            "{js_path} is the mount root itself; write a file path under {virt}/"
        ));
    }
    if Path::new(&rel)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("path escapes mount root, rejected: {js_path}"));
    }
    let joined = m.root.join(&rel);
    let parent = joined.parent().unwrap_or(&m.root).to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|e| format!("{js_path}: {e}"))?;
    let canon_parent = parent
        .canonicalize()
        .map_err(|e| format!("{js_path}: {e}"))?;
    let root = m
        .root
        .canonicalize()
        .map_err(|e| format!("{js_path}: {e}"))?;
    if !canon_parent.starts_with(&root) {
        return Err(format!("path escapes mount root, rejected: {js_path}"));
    }
    let target = canon_parent.join(joined.file_name().unwrap_or_default());
    let (bom, crlf) = detect_shape(&target);
    let mut body = content.replace("\r\n", "\n"); // canonicalize; lone \r is left untouched
    if crlf {
        body = body.replace('\n', "\r\n");
    }
    let mut out = String::new();
    if bom && !body.starts_with('\u{feff}') {
        out.push('\u{feff}');
    }
    out.push_str(&body);
    let tmp = canon_parent.join(format!(
        ".terrarium-{}-{}",
        std::process::id(),
        TMP_CTR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, &out).map_err(|e| format!("{js_path}: {e}"))?;
    // brief backoff retries: transient holders (AV scanners, indexers, editors — the classic Windows
    // sharing violation, same mitigation git/cargo use); persistent holders still surface as an error
    let mut err = None;
    for attempt in 0..3 {
        match std::fs::rename(&tmp, &target) {
            Ok(()) => {
                err = None;
                break;
            }
            Err(e) => {
                err = Some(e);
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
    if let Some(e) = err {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{js_path}: {e} (after 3 attempts — a lingering editor/antivirus handle is the usual culprit)"));
    }
    Ok(out.len())
}

fn js_err(msg: String) -> rquickjs::Error {
    rquickjs::Error::FromJs {
        from: "fs",
        to: "result",
        message: Some(msg),
    }
}

/// Registers the host.fs namespace
pub fn install<'js>(ctx: &Ctx<'js>, host: &Object<'js>, mounts: &[Mount]) -> rquickjs::Result<()> {
    let fsobj = Object::new(ctx.clone())?;
    let mounts: Vec<Mount> = mounts.to_vec();

    let m = mounts.clone();
    let list_fn = Function::new(ctx.clone(), move |dir: String| {
        list_dir(&m, &dir).map_err(js_err)
    })?;
    fsobj.set("list", list_fn)?;

    let m = mounts.clone();
    // window-only: an explicit line range per read — to=Infinity reads to EOF
    let read_fn = Function::new(
        ctx.clone(),
        move |path: String, from: Opt<f64>, to: Opt<f64>| {
            let (a, b) = match (from.0, to.0) {
            (Some(a), Some(b)) => (a.max(1.0) as usize, b.max(0.0) as usize),
            _ => {
                return Err(js_err(format!(
                    "read(path, from, to) needs an explicit line window, e.g. host.fs.read(\"{path}\", 1, 300); to=Infinity reads to EOF"
                )))
            }
        };
            read_window(&m, &path, a, b).map_err(js_err)
        },
    )?;
    fsobj.set("read", read_fn)?;

    let m = mounts.clone();
    let search_fn = Function::new(
        ctx.clone(),
        move |needle: String, max: Opt<f64>, skips: Opt<Object>| {
            let max = max.0.map(|x| x.max(1.0) as usize).unwrap_or(20);
            let mut skip_dirs: Vec<String> = SKIP_DIRS.iter().map(|s| s.to_string()).collect();
            let mut skip_exts: Vec<String> = SKIP_EXTS.iter().map(|s| s.to_string()).collect();
            if let Some(o) = skips.0 {
                if let Some(v) = o.get::<_, Option<Vec<String>>>("skipDirs").ok().flatten() {
                    skip_dirs = v;
                }
                if let Some(v) = o.get::<_, Option<Vec<String>>>("skipExts").ok().flatten() {
                    skip_exts = v.into_iter().map(|s| s.to_lowercase()).collect();
                }
            }
            search_files(&m, &needle, max, skip_dirs, skip_exts).map_err(js_err)
        },
    )?;
    fsobj.set("search", search_fn)?;

    let m = mounts.clone();
    let text_fn = Function::new(ctx.clone(), move |path: String| {
        read_text(&m, &path).map_err(js_err)
    })?;
    fsobj.set("text", text_fn)?;

    let m = mounts;
    let write_fn = Function::new(ctx.clone(), move |path: String, content: String| {
        write_file(&m, &path, &content).map_err(js_err)
    })?;
    fsobj.set("write", write_fn)?;

    host.set("fs", fsobj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("terrarium-fs-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn mounts_at(root: &Path, rw: bool) -> Vec<Mount> {
        vec![Mount {
            virt: "/t/".into(),
            root: root.to_path_buf(),
            rw,
        }]
    }

    #[test]
    fn write_requires_rw_mount() {
        let root = tmp_root("ro");
        let ms = mounts_at(&root, false);
        let err = write_file(&ms, "/t/new.txt", "hi").unwrap_err();
        assert!(
            err.contains("read-only mount") && err.contains("Policy denial"),
            "{err}"
        );
    }

    #[test]
    fn write_is_atomic_creates_parents_and_round_trips() {
        let root = tmp_root("rw");
        let ms = mounts_at(&root, true);
        let n = write_file(&ms, "/t/a/b/c.txt", "hello\nworld").unwrap();
        assert_eq!(n, 11);
        assert_eq!(
            std::fs::read_to_string(root.join("a/b/c.txt")).unwrap(),
            "hello\nworld"
        );
        // no temp litter after a successful write
        assert!(std::fs::read_dir(root.join("a/b")).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".terrarium-")));
        // overwrite is a rename, not a truncate-in-place
        write_file(&ms, "/t/a/b/c.txt", "x").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a/b/c.txt")).unwrap(),
            "x"
        );
    }

    #[test]
    fn write_rejects_dotdot_escape() {
        let root = tmp_root("esc");
        let ms = mounts_at(&root, true);
        let err = write_file(&ms, "/t/../escape.txt", "x").unwrap_err();
        assert!(err.contains("escapes mount root"), "{err}");
        assert!(!root.parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn read_window_footer_iff_more_lines() {
        let root = tmp_root("read");
        std::fs::write(root.join("f.txt"), "one\ntwo\nthree\nfour").unwrap();
        let ms = mounts_at(&root, false);
        let cut = read_window(&ms, "/t/f.txt", 1, 2).unwrap();
        assert_eq!(
            cut,
            "1: one\n2: two\n[more lines follow — continue with host.fs.read(\"/t/f.txt\", 3, …)]"
        );
        let whole = read_window(&ms, "/t/f.txt", 1, usize::MAX).unwrap();
        assert!(
            whole.ends_with("4: four") && !whole.contains("more lines"),
            "{whole}"
        );
        let mid = read_window(&ms, "/t/f.txt", 2, 3).unwrap();
        assert_eq!(mid, "2: two\n3: three\n[more lines follow — continue with host.fs.read(\"/t/f.txt\", 4, …)]");
    }

    #[test]
    fn read_caps_pathological_lines() {
        let root = tmp_root("longline");
        std::fs::write(root.join("min.txt"), format!("{}\nend", "x".repeat(5000))).unwrap();
        let ms = mounts_at(&root, false);
        let s = read_window(&ms, "/t/min.txt", 1, 2).unwrap();
        let l1 = s.lines().next().unwrap();
        assert!(
            l1.starts_with("1: ") && l1.ends_with('…') && l1.chars().count() <= 2004,
            "{l1:?}"
        );
    }

    #[test]
    fn search_skips_are_replaceable() {
        let root = tmp_root("search");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "needle-here").unwrap();
        std::fs::write(root.join("code.rs"), "needle-too").unwrap();
        let ms = mounts_at(&root, false);
        let defaults = search_files(
            &ms,
            "needle",
            20,
            SKIP_DIRS.iter().map(|s| s.to_string()).collect(),
            SKIP_EXTS.iter().map(|s| s.to_string()).collect(),
        )
        .unwrap();
        assert_eq!(defaults.len(), 1); // .git skipped by default
        let all = search_files(&ms, "needle", 20, vec![], vec![]).unwrap();
        assert_eq!(all.len(), 2); // empty overrides scan everything
    }

    #[test]
    fn write_restores_crlf_bom_and_lone_cr() {
        let root = tmp_root("shape");
        let ms = mounts_at(&root, true);
        // CRLF-dominant + BOM target: round-trip restores both; return value counts real bytes written
        std::fs::write(root.join("win.txt"), b"\xEF\xBB\xBFa\r\nb\r\nc\n").unwrap();
        let n = write_file(&ms, "/t/win.txt", "x\ny").unwrap();
        assert_eq!(
            std::fs::read(root.join("win.txt")).unwrap(),
            b"\xEF\xBB\xBFx\r\ny"
        );
        assert_eq!(n, 3 + 3 + 1); // BOM + "x\r\n" + "y"
                                  // LF target stays LF; lone \r survives untouched either way
        std::fs::write(root.join("unix.txt"), "a\nb\n").unwrap();
        write_file(&ms, "/t/unix.txt", "x\ry\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("unix.txt")).unwrap(),
            "x\ry\n"
        );
        // a brand-new file gets exactly what was given — no BOM, no CRLF invention
        write_file(&ms, "/t/fresh.txt", "1\n2").unwrap();
        assert_eq!(std::fs::read(root.join("fresh.txt")).unwrap(), b"1\n2");
        // majority rules within the 4KB sample: CRLF wins ties only on strict majority
        std::fs::write(root.join("mix.txt"), "a\r\nb\nc\n").unwrap(); // crlf=1, lf=1 → LF
        write_file(&ms, "/t/mix.txt", "z\n").unwrap();
        assert_eq!(std::fs::read(root.join("mix.txt")).unwrap(), b"z\n");
    }

    #[test]
    fn text_channel_is_lf_normalized_and_round_trips() {
        let root = tmp_root("textch");
        let ms = mounts_at(&root, true);
        std::fs::write(root.join("c.txt"), "a\r\nb\r\nc").unwrap();
        assert_eq!(read_text(&ms, "/t/c.txt").unwrap(), "a\nb\nc");
        // surgical idiom: text → replace → write restores the file's own EOL
        let edited = read_text(&ms, "/t/c.txt").unwrap().replace("b", "B");
        write_file(&ms, "/t/c.txt", &edited).unwrap();
        assert_eq!(std::fs::read(root.join("c.txt")).unwrap(), b"a\r\nB\r\nc");
    }

    #[test]
    fn resolve_rejects_escape_and_offmount() {
        let root = tmp_root("resolve");
        let ms = mounts_at(&root, false);
        assert!(resolve_mount(&ms, "/t/../../../etc/passwd").is_err());
        assert!(resolve_mount(&ms, "/nope/x")
            .unwrap_err()
            .contains("not under any mount"));
    }
}
