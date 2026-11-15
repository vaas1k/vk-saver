use crate::download::{FreeSpace, MIN_FREE};
use crate::error::Stop;
use crate::export::{ReplaceError, STATE_DIR, find_root, replace_file};
use crate::naming::is_reserved;
use crate::progress::{Event, Progress};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;
use zip::{CompressionMethod, ZipArchive};

const SOURCE: &str = "zip-source";
const MAX_RATIO: u64 = 50;
const CHUNK: usize = 1 << 20;
const MAX_ENTRIES: usize = 500_000;
const MAX_NAME: usize = 240;
const MAX_DEPTH: usize = 16;
/// Infix of our partial directory: `<target name>.vk-saver-unzip-<pid>-<nanos>`.
const PARTIAL: &str = ".vk-saver-unzip-";
/// Marks a partial directory as ours; lives in `vk-saver/`, which no entry may create.
const MARKER: &str = "unzip-partial";

fn storage(e: impl std::fmt::Display) -> Stop {
    Stop::Storage(e.to_string())
}

/// `<size> <mtime>` of the ZIP, kept in `<target>/vk-saver/zip-source`.
fn source_id(zip: &Path) -> Result<String, Stop> {
    let m = std::fs::metadata(zip).map_err(storage)?;
    let mtime = m.modified().map_err(storage)?;
    let secs = mtime
        .duration_since(UNIX_EPOCH)
        .map_err(storage)?
        .as_nanos();
    Ok(format!("{} {secs}", m.len()))
}

/// `<zip dir>/<zip stem>`, or the first `<stem> (n)` that is free or was unpacked from this ZIP.
pub fn target_for(zip: &Path) -> Result<PathBuf, Stop> {
    let dir = zip.parent().ok_or(Stop::NotExport)?;
    let stem = zip.file_stem().ok_or(Stop::NotExport)?.to_string_lossy();
    let id = source_id(zip)?;
    for n in 1.. {
        let name = if n == 1 {
            stem.to_string()
        } else {
            format!("{stem} ({n})")
        };
        let cand = dir.join(name);
        if std::fs::symlink_metadata(&cand).is_err() {
            return Ok(cand);
        }
        if std::fs::read_to_string(cand.join(STATE_DIR).join(SOURCE)).is_ok_and(|s| s == id) {
            return Ok(cand);
        }
    }
    unreachable!()
}

/// Bidi and direction marks that make a name display differently from what it is.
fn is_bidi(ch: char) -> bool {
    matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
}

/// Components of an entry name; refuses anything that could leave the target or clash on Windows (W4, W5).
fn components(name: &str) -> Option<Vec<&str>> {
    if name.starts_with('/') || name.contains(['\\', ':']) {
        return None;
    }
    let parts: Vec<&str> = name.trim_end_matches('/').split('/').collect();
    parts
        .iter()
        .all(|c| {
            !c.is_empty()
                && *c != "."
                && *c != ".."
                && !c
                    .chars()
                    .any(|ch| ch.is_control() || "<>\"|?*".contains(ch) || is_bidi(ch))
                && !is_reserved(c)
        })
        .then_some(parts)
}

struct Entry {
    index: usize,
    parts: Vec<String>,
    dir: bool,
    size: u64,
}

/// 128-bit digest, so the name map holds no path strings (at the caps those would cost gigabytes).
fn digest(s: &str) -> u128 {
    u128::from_le_bytes(
        Sha256::digest(s.as_bytes())[..16]
            .try_into()
            .expect("16 bytes"),
    )
}

/// Every entry checked before anything is written.
fn plan(ar: &mut ZipArchive<File>) -> Result<Vec<Entry>, Stop> {
    let bad = Stop::BadZip;
    if ar.len() > MAX_ENTRIES {
        return Err(bad("limits"));
    }
    if ar.has_duplicate_names() {
        return Err(bad("duplicate name"));
    }
    ar.validate_header_consistency()
        .map_err(|_| bad("corrupt"))?;
    // lowercase path -> exact directory path (None for a file)
    let mut seen: HashMap<u128, Option<u128>> = HashMap::new();
    let mut out = Vec::new();
    for index in 0..ar.len() {
        let f = ar.by_index_raw(index).map_err(|_| bad("corrupt"))?;
        let name = f.name().map_err(|_| bad("unsafe name"))?.into_owned();
        if name.len() > MAX_NAME {
            return Err(bad("limits"));
        }
        let parts = components(&name).ok_or(bad("unsafe name"))?;
        if parts.len() > MAX_DEPTH {
            return Err(bad("limits"));
        }
        if parts.iter().any(|c| c.eq_ignore_ascii_case(STATE_DIR)) {
            return Err(bad("state dir"));
        }
        // Second opinion only: our own checks above decide.
        f.enclosed_name().ok_or(bad("unsafe name"))?;
        if f.is_symlink() || f.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err(bad("symlink"));
        }
        if f.encrypted() {
            return Err(bad("encrypted"));
        }
        if !(f.is_dir() || f.is_file()) {
            return Err(bad("entry type"));
        }
        if !matches!(
            f.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(bad("method"));
        }
        let dir = f.is_dir();
        for i in 0..parts.len() {
            let path = parts[..=i].join("/");
            let is_dir = dir || i + 1 < parts.len();
            let key = digest(&path.to_lowercase());
            let exact = is_dir.then(|| digest(&path));
            match seen.get(&key) {
                Some(Some(d)) if exact == Some(*d) => {}
                Some(_) => return Err(bad("duplicate name")),
                None => {
                    seen.insert(key, exact);
                }
            }
        }
        out.push(Entry {
            index,
            parts: parts.into_iter().map(String::from).collect(),
            dir,
            size: if dir { 0 } else { f.size() },
        });
    }
    Ok(out)
}

fn join(base: &Path, parts: &[String]) -> PathBuf {
    let mut p = base.to_path_buf();
    for c in parts {
        p.push(c);
    }
    p
}

fn extract(
    ar: &mut ZipArchive<File>,
    entries: &[Entry],
    partial: &Path,
    cancel: &AtomicBool,
    p: &dyn Progress,
) -> Result<(), Stop> {
    let total: u64 = entries.iter().map(|e| e.size).sum();
    let mut done = 0u64;
    let mut buf = vec![0u8; CHUNK];
    for e in entries {
        let dest = join(partial, &e.parts);
        if e.dir {
            std::fs::create_dir_all(&dest).map_err(storage)?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(storage)?;
        }
        let mut out = File::create_new(&dest).map_err(storage)?;
        let f = ar.by_index(e.index).map_err(|_| Stop::BadZip("corrupt"))?;
        // One byte past the declared size is enough to catch a lying entry.
        let mut src = f.take(e.size + 1);
        let mut written = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Stop::Cancelled);
            }
            let n = src.read(&mut buf).map_err(|_| Stop::BadZip("corrupt"))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > e.size || done + n as u64 > total {
                return Err(Stop::BadZip("size"));
            }
            out.write_all(&buf[..n]).map_err(storage)?;
            done += n as u64;
            p.event(Event::Unzip { done, total });
        }
        if written != e.size {
            return Err(Stop::BadZip("size"));
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Stop::Cancelled);
    }
    p.event(Event::Unzip { done, total });
    Ok(())
}

/// Unpacks `zip` next to itself and returns the export root inside.
pub fn unzip(
    zip: &Path,
    space: &dyn FreeSpace,
    cancel: &AtomicBool,
    p: &dyn Progress,
) -> Result<PathBuf, Stop> {
    let target = target_for(zip)?;
    if target.exists() {
        return find_root(&target).ok_or(Stop::NotExport);
    }
    let file = File::open(zip).map_err(storage)?;
    let zip_len = file.metadata().map_err(storage)?.len();
    let mut ar = ZipArchive::new(file).map_err(|_| Stop::BadZip("not a zip"))?;
    let entries = plan(&mut ar)?;
    let declared = entries
        .iter()
        .try_fold(0u64, |s, e| s.checked_add(e.size))
        .ok_or(Stop::BadZip("size"))?;
    if declared > zip_len.saturating_mul(MAX_RATIO) {
        return Err(Stop::BadZip("ratio"));
    }
    let dir = target.parent().ok_or(Stop::NotExport)?;
    let free = space.free_bytes(dir).map_err(storage)?;
    if declared > free.saturating_sub(MIN_FREE) {
        return Err(Stop::NoSpace(free));
    }
    if !looks_like_export(&entries) {
        return Err(Stop::NotExport);
    }
    let name = target.file_name().ok_or(Stop::NotExport)?.to_string_lossy();
    let prefix = format!("{name}{PARTIAL}");
    remove_stale_partials(dir, &prefix);
    let nanos = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(storage)?
        .as_nanos();
    let partial = dir.join(format!("{prefix}{}-{nanos}", std::process::id()));
    std::fs::create_dir(&partial).map_err(storage)?;
    let state = partial.join(STATE_DIR);
    let r = std::fs::create_dir(&state)
        .and_then(|()| std::fs::write(state.join(MARKER), b""))
        .map_err(storage)
        .and_then(|()| extract(&mut ar, &entries, &partial, cancel, p))
        .and_then(|()| {
            if only_marker(&state) {
                Ok(())
            } else {
                Err(Stop::BadZip("state dir"))
            }
        })
        .and_then(|()| {
            std::fs::write(state.join(SOURCE), source_id(zip)?).map_err(storage)?;
            rename_dir(&partial, &target)
        });
    if let Err(e) = r {
        if is_ours(&partial) {
            let _ = std::fs::remove_dir_all(&partial);
        }
        return Err(e);
    }
    std::fs::remove_file(target.join(STATE_DIR).join(MARKER)).map_err(storage)?;
    find_root(&target).ok_or_else(|| {
        let _ = std::fs::remove_dir_all(&target);
        Stop::NotExport
    })
}

fn is_ours(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
        && std::fs::symlink_metadata(dir.join(STATE_DIR).join(MARKER)).is_ok_and(|m| m.is_file())
}

/// An entry aliasing `vk-saver/` under some filesystem's case folding would land next to the marker.
fn only_marker(state: &Path) -> bool {
    std::fs::read_dir(state).is_ok_and(|list| {
        let names: Vec<_> = list.map(|e| e.map(|e| e.file_name())).collect();
        matches!(&names[..], [Ok(n)] if n == MARKER)
    })
}

/// `<pid>-<nanos>`, the exact tail of our partial directory names.
fn partial_tail(s: &str) -> bool {
    let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    s.split_once('-')
        .is_some_and(|(a, b)| digits(a) && digits(b))
}

/// Partial directories of a crashed run: our exact name, our marker, and never a finished
/// extraction (one that already has `zip-source`).
fn remove_stale_partials(dir: &Path, prefix: &str) {
    let Ok(list) = std::fs::read_dir(dir) else {
        return;
    };
    for e in list.flatten() {
        let path = e.path();
        let ours_by_name = e
            .file_name()
            .to_str()
            .and_then(|n| n.strip_prefix(prefix))
            .is_some_and(partial_tail);
        let finished = std::fs::symlink_metadata(path.join(STATE_DIR).join(SOURCE)).is_ok();
        if ours_by_name && is_ours(&path) && !finished {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Antivirus and the indexer hold fresh files open for seconds on Windows, so a busy rename is retried for about 10 s.
fn rename_dir(from: &Path, to: &Path) -> Result<(), Stop> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match replace_file(from, to) {
            Ok(()) => return Ok(()),
            Err(ReplaceError::Busy) if std::time::Instant::now() < deadline => {}
            Err(ReplaceError::Busy) => return Err(Stop::Busy),
            Err(ReplaceError::Io(s)) => return Err(Stop::Storage(s)),
        }
    }
}

/// The entry names hold `index.html` plus `messages/` or `photos/`, at the top or under one top folder (as `find_root`).
fn looks_like_export(entries: &[Entry]) -> bool {
    let mut roots: HashMap<&str, (bool, bool)> = HashMap::new();
    for e in entries {
        for depth in 0..e.parts.len().min(2) {
            let key = if depth == 0 { "" } else { e.parts[0].as_str() };
            let rest = &e.parts[depth..];
            let r = roots.entry(key).or_default();
            r.0 |= !e.dir && rest.len() == 1 && rest[0] == "index.html";
            r.1 |= (rest[0] == "messages" || rest[0] == "photos") && (e.dir || rest.len() > 1);
        }
    }
    roots.get("").is_some_and(|r| r.0 && r.1)
        || roots
            .iter()
            .filter(|(k, r)| !k.is_empty() && r.0 && r.1)
            .count()
            == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::MIN_FREE;
    use crate::progress::Event;
    use crate::testing::{EventLog, FakeExportOpts, FakeSpace, NoProgress, fake_export, tmpdir};
    use std::io::{Cursor, Write};
    use std::sync::atomic::Ordering;
    use zip::write::SimpleFileOptions;

    static NO_CANCEL: AtomicBool = AtomicBool::new(false);

    fn space() -> FakeSpace {
        FakeSpace(Ok(100 * MIN_FREE))
    }

    fn opts() -> SimpleFileOptions {
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)
    }

    fn files(dir: &Path, rel: &str, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = format!("{rel}{}", e.file_name().to_str().unwrap());
            if e.file_type().unwrap().is_dir() {
                files(&e.path(), &format!("{name}/"), out);
            } else {
                out.push((name, std::fs::read(e.path()).unwrap()));
            }
        }
    }

    fn build(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in entries {
            if name.ends_with('/') {
                w.add_directory(name.as_str(), opts()).unwrap();
                continue;
            }
            w.start_file(name.as_str(), opts()).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn fixture_zip(dir: &Path, name: &str, prefix: &str) -> (PathBuf, Vec<(String, Vec<u8>)>) {
        let src = fake_export(
            &format!("{}-src", dir.file_name().unwrap().to_str().unwrap()),
            FakeExportOpts::default(),
        );
        let mut entries = Vec::new();
        files(&src, prefix, &mut entries);
        let zip = dir.join(format!("{name}.zip"));
        std::fs::write(&zip, build(&entries)).unwrap();
        (zip, entries)
    }

    fn only_zip_left(dir: &Path) {
        let names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().all(|n| n.ends_with(".zip")), "{names:?}");
    }

    fn owned(entries: &[(&str, &[u8])]) -> Vec<(String, Vec<u8>)> {
        entries
            .iter()
            .map(|(n, b)| (n.to_string(), b.to_vec()))
            .collect()
    }

    fn unzip_bytes(dir: &Path, name: &str, bytes: Vec<u8>) -> Stop {
        let zip = dir.join(format!("{name}.zip"));
        std::fs::write(&zip, bytes).unwrap();
        unzip(&zip, &space(), &NO_CANCEL, &NoProgress).unwrap_err()
    }

    fn refused(dir: &Path, name: &str, entries: &[(&str, &[u8])]) -> Stop {
        unzip_bytes(dir, name, build(&owned(entries)))
    }

    /// Writes `val` at `off` in the first local header and at the same field of the first central header.
    fn patch(zip: &mut [u8], off: usize, val: &[u8]) {
        zip[off..off + val.len()].copy_from_slice(val);
        let cd = zip
            .windows(4)
            .position(|w| w == [0x50, 0x4b, 0x01, 0x02])
            .unwrap();
        zip[cd + off + 2..cd + off + 2 + val.len()].copy_from_slice(val);
    }

    fn noise(n: u32) -> Vec<u8> {
        (0..n)
            .flat_map(|i| i.wrapping_mul(2654435761).to_le_bytes())
            .collect()
    }

    /// An export-looking ZIP whose first entry is `index.html` with `body`.
    fn export_bytes(body: Vec<u8>) -> Vec<u8> {
        build(&[
            ("index.html".into(), body),
            ("messages/".into(), Vec::new()),
        ])
    }

    #[test]
    fn unzip_fixture_opens() {
        let d = tmpdir("unzip-opens");
        let (zip, entries) = fixture_zip(&d, "export", "");
        let log = EventLog::default();
        let root = unzip(&zip, &space(), &NO_CANCEL, &log).unwrap();
        assert_eq!(root, d.join("export"));
        assert!(root.join("index.html").is_file());
        for (name, body) in &entries {
            let got = std::fs::read(root.join(name)).unwrap();
            assert_eq!(&got, body, "{name}");
        }
        let total: u64 = entries.iter().map(|(_, b)| b.len() as u64).sum();
        assert_eq!(
            log.events().last(),
            Some(&Event::Unzip { done: total, total })
        );
        assert!(!root.join(STATE_DIR).join(MARKER).exists());
        assert!(crate::export::Export::open(&root).is_ok());
    }

    #[test]
    fn zip_slip_refused() {
        let d = tmpdir("unzip-slip");
        for (i, name) in ["../x.html", "/x", "C:/x", "a\\..\\x", "a/../x", "a:b"]
            .iter()
            .enumerate()
        {
            let e = refused(&d, &format!("slip{i}"), &[("ok.html", b"ok"), (name, b"x")]);
            assert_eq!(e, Stop::BadZip("unsafe name"), "{name}");
        }
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("ok.html", opts()).unwrap();
        w.add_symlink("link", "/etc/passwd", opts()).unwrap();
        let e = unzip_bytes(&d, "sym", w.finish().unwrap().into_inner());
        assert_eq!(e, Stop::BadZip("symlink"));
        only_zip_left(&d);
    }

    #[test]
    fn case_and_reserved_refused() {
        let d = tmpdir("unzip-case");
        type Entries<'a> = &'a [(&'a str, &'a [u8])];
        let cases: [(Entries, &str); 10] = [
            (&[("A.html", b"1"), ("a.html", b"2")], "duplicate name"),
            (
                &[("Dir/x.html", b"1"), ("dir/y.html", b"2")],
                "duplicate name",
            ),
            (&[("CON.html", b"1")], "unsafe name"),
            (&[("x.", b"1")], "unsafe name"),
            (&[("d/aux/x.html", b"1")], "unsafe name"),
            (&[("a\u{202e}lmth.exe", b"1")], "unsafe name"),
            (&[("a/\u{200f}b", b"1")], "unsafe name"),
            (&[("x\u{2066}.html", b"1")], "unsafe name"),
            (&[("VK-SAVER/index.db", b"1")], "state dir"),
            (&[("Archive/Vk-Saver/zip-source", b"1")], "state dir"),
        ];
        for (i, (entries, why)) in cases.iter().enumerate() {
            let e = refused(&d, &format!("case{i}"), entries);
            assert_eq!(e, Stop::BadZip(why), "{entries:?}");
        }
        only_zip_left(&d);
    }

    #[test]
    fn name_clashes_refused() {
        let d = tmpdir("unzip-clash");
        let cases: [&[(&str, &[u8])]; 3] = [
            &[("a", b"1"), ("a/", b"")],
            &[("a", b"1"), ("a/b.html", b"2")],
            &[("a/b.html", b"1"), ("A", b"2")],
        ];
        for (i, entries) in cases.iter().enumerate() {
            let e = refused(&d, &format!("clash{i}"), entries);
            assert_eq!(e, Stop::BadZip("duplicate name"), "{entries:?}");
        }
        let mut bytes = build(&owned(&[("a.html", b"1"), ("b.html", b"2")]));
        for i in 0..bytes.len() - 6 {
            if &bytes[i..i + 6] == b"b.html" {
                bytes[i] = b'a';
            }
        }
        assert_eq!(
            unzip_bytes(&d, "exact", bytes),
            Stop::BadZip("duplicate name")
        );
        only_zip_left(&d);
    }

    #[test]
    fn caps_refused() {
        let d = tmpdir("unzip-caps");
        let long = "a".repeat(MAX_NAME + 1);
        let deep = vec!["d"; MAX_DEPTH + 1].join("/");
        let ok_deep = vec!["d"; MAX_DEPTH].join("/");
        for (i, name) in [long.as_str(), deep.as_str()].iter().enumerate() {
            let e = refused(&d, &format!("cap{i}"), &[(name, b"x")]);
            assert_eq!(e, Stop::BadZip("limits"), "{}", name.len());
        }
        let e = refused(&d, "okdeep", &[(&ok_deep, b"x")]);
        assert_eq!(e, Stop::NotExport);
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for i in 0..=MAX_ENTRIES {
            w.start_file(format!("{i:x}"), stored).unwrap();
        }
        let e = unzip_bytes(&d, "many", w.finish().unwrap().into_inner());
        assert_eq!(e, Stop::BadZip("limits"));
        only_zip_left(&d);
    }

    #[test]
    fn broken_entries_refused() {
        let d = tmpdir("unzip-broken");
        let body = noise(20_000);
        let cases: [(usize, Vec<u8>, &str); 4] = [
            (6, 1u16.to_le_bytes().to_vec(), "encrypted"),
            (8, 12u16.to_le_bytes().to_vec(), "method"),
            (14, 0xdead_beefu32.to_le_bytes().to_vec(), "corrupt"),
            (
                22,
                (body.len() as u32 + 1000).to_le_bytes().to_vec(),
                "size",
            ),
        ];
        for (i, (off, val, why)) in cases.iter().enumerate() {
            let mut bytes = export_bytes(body.clone());
            patch(&mut bytes, *off, val);
            assert_eq!(
                unzip_bytes(&d, &format!("broken{i}"), bytes),
                Stop::BadZip(why)
            );
        }
        only_zip_left(&d);
    }

    #[test]
    fn not_export_writes_nothing() {
        let d = tmpdir("unzip-notexport");
        let cases: [&[(&str, &[u8])]; 3] = [
            &[("a.html", b"1"), ("messages/b.html", b"2")],
            &[("index.html", b"1"), ("other/b.html", b"2")],
            &[("a/b/index.html", b"1"), ("a/b/messages/x.html", b"2")],
        ];
        for (i, entries) in cases.iter().enumerate() {
            assert_eq!(refused(&d, &format!("n{i}"), entries), Stop::NotExport);
        }
        only_zip_left(&d);
    }

    #[test]
    fn bomb_refused_before_write() {
        let d = tmpdir("unzip-bomb");
        let e = refused(&d, "bomb", &[("index.html", &vec![0u8; 4 << 20])]);
        assert_eq!(e, Stop::BadZip("ratio"));
        let zip = d.join("big.zip");
        std::fs::write(&zip, export_bytes(noise(300_000))).unwrap();
        let e = unzip(
            &zip,
            &FakeSpace(Ok(MIN_FREE + 1000)),
            &NO_CANCEL,
            &NoProgress,
        )
        .unwrap_err();
        assert_eq!(e, Stop::NoSpace(MIN_FREE + 1000));
        only_zip_left(&d);
    }

    #[test]
    fn lying_size_aborts() {
        let d = tmpdir("unzip-lie");
        let mut bytes = export_bytes(noise(20_000));
        patch(&mut bytes, 22, &100u32.to_le_bytes());
        // zip stops at the declared size, so the CRC check refuses it first.
        assert_eq!(unzip_bytes(&d, "lie", bytes), Stop::BadZip("corrupt"));
        only_zip_left(&d);
    }

    #[test]
    fn foreign_partial_untouched_stale_ours_removed() {
        let d = tmpdir("unzip-partial");
        let (zip, _) = fixture_zip(&d, "export", "");
        let foreign = [
            d.join("export.partial"),
            d.join(format!("export{PARTIAL}1-2")),
        ];
        for f in &foreign {
            std::fs::create_dir(f).unwrap();
            std::fs::write(f.join("mine.txt"), b"user").unwrap();
        }
        let mut kept = Vec::new();
        for (name, finished) in [
            (format!("export{PARTIAL}5-6x"), false),
            (format!("export{PARTIAL}7-8"), true),
        ] {
            let k = d.join(name);
            std::fs::create_dir_all(k.join(STATE_DIR)).unwrap();
            std::fs::write(k.join(STATE_DIR).join(MARKER), b"").unwrap();
            if finished {
                std::fs::write(k.join(STATE_DIR).join(SOURCE), b"1 2").unwrap();
            }
            kept.push(k);
        }
        let stale = d.join(format!("export{PARTIAL}3-4"));
        std::fs::create_dir_all(stale.join(STATE_DIR)).unwrap();
        std::fs::write(stale.join(STATE_DIR).join(MARKER), b"").unwrap();
        unzip(&zip, &space(), &NO_CANCEL, &NoProgress).unwrap();
        for f in &foreign {
            assert_eq!(std::fs::read(f.join("mine.txt")).unwrap(), b"user");
        }
        assert!(!stale.exists());
        for k in &kept {
            assert!(k.join(STATE_DIR).join(MARKER).exists(), "{k:?}");
        }
    }

    #[test]
    fn zip_nested_root_reused_target_and_suffix() {
        let d = tmpdir("unzip-nested");
        let (zip, _) = fixture_zip(&d, "vk", "Archive/");
        let root = unzip(&zip, &space(), &NO_CANCEL, &NoProgress).unwrap();
        assert_eq!(root, d.join("vk").join("Archive"));
        std::fs::write(root.join("marker"), b"m").unwrap();
        assert_eq!(target_for(&zip).unwrap(), d.join("vk"));
        let again = unzip(&zip, &space(), &NO_CANCEL, &NoProgress).unwrap();
        assert_eq!(again, root);
        assert!(root.join("marker").is_file());

        let other = tmpdir("unzip-nested-other");
        let (zip2, _) = fixture_zip(&other, "vk", "");
        std::fs::write(
            &zip2,
            [std::fs::read(&zip2).unwrap(), b"tail".to_vec()].concat(),
        )
        .unwrap();
        let moved = d.join("sub");
        std::fs::create_dir(&moved).unwrap();
        std::fs::rename(&zip2, d.join("vk.zip.other")).unwrap();
        std::fs::rename(d.join("vk.zip"), moved.join("vk.zip")).unwrap();
        std::fs::rename(d.join("vk.zip.other"), d.join("vk.zip")).unwrap();
        assert_eq!(target_for(&d.join("vk.zip")).unwrap(), d.join("vk (2)"));
        let r2 = unzip(&d.join("vk.zip"), &space(), &NO_CANCEL, &NoProgress).unwrap();
        assert_eq!(r2, d.join("vk (2)"));
    }

    #[test]
    fn cancel_removes_partial() {
        let d = tmpdir("unzip-cancel");
        let (zip, _) = fixture_zip(&d, "export", "");
        let cancel = AtomicBool::new(true);
        assert_eq!(
            unzip(&zip, &space(), &cancel, &NoProgress),
            Err(Stop::Cancelled)
        );
        only_zip_left(&d);
        cancel.store(false, Ordering::Relaxed);
        assert!(unzip(&zip, &space(), &cancel, &NoProgress).is_ok());
    }

    #[test]
    fn header_mismatch_refused() {
        let d = tmpdir("unzip-mismatch");
        let good = export_bytes(b"<html>".to_vec());
        // local header only: name byte, uncompressed size
        for (i, (off, val)) in [(30usize, &b"j"[..]), (22, &[9u8, 0, 0, 0][..])]
            .into_iter()
            .enumerate()
        {
            let mut z = good.clone();
            z[off..off + val.len()].copy_from_slice(val);
            assert_eq!(
                unzip_bytes(&d, &format!("m{i}"), z),
                Stop::BadZip("corrupt"),
                "{off}"
            );
        }
        assert_eq!(
            unzip_bytes(&d, "junk", b"not a zip at all".to_vec()),
            Stop::BadZip("not a zip")
        );
        let left = std::fs::read_dir(&d).unwrap().count();
        assert_eq!(left, 3);
    }

    #[test]
    fn state_dir_must_hold_only_marker() {
        let d = tmpdir("unzip-only-marker");
        std::fs::write(d.join(MARKER), b"").unwrap();
        assert!(only_marker(&d));
        std::fs::write(d.join("index.db"), b"").unwrap();
        assert!(!only_marker(&d));
        assert!(!only_marker(&d.join("missing")));
    }
}
