pub mod fetch;
pub mod index;
pub mod known;
pub mod report;
pub mod rewrite;
pub mod scan;
pub mod session;
pub mod text;
pub mod unzip;

use crate::error::Stop;
use crate::naming::is_reserved;
use std::fs::{File, TryLockError};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

pub const STATE_DIR: &str = "vk-saver";
/// At most 2 requests per second.
pub const GAP_MS: u64 = 500;
/// Real pages are tens to hundreds of KiB; a larger one is never read into memory.
pub const MAX_PAGE: u64 = 64 << 20;

const REPLACE_RETRIES: u32 = 5;
const REPLACE_GAP_MS: u64 = 200;

fn is_root(dir: &Path) -> bool {
    dir.join("index.html").is_file()
        && (dir.join("messages").is_dir() || dir.join("photos").is_dir())
}

/// `dir` itself or its only child directory that is an export root.
pub fn find_root(dir: &Path) -> Option<PathBuf> {
    if is_root(dir) {
        return Some(dir.to_path_buf());
    }
    let mut found = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_ok_and(|t| t.is_dir()) && is_root(&entry.path()) {
            if found.is_some() {
                return None;
            }
            found = Some(entry.path());
        }
    }
    found
}

pub struct Export {
    root: PathBuf,
    _lock: File,
}

impl Export {
    pub fn open(dir: &Path) -> Result<Export, Stop> {
        let root = find_root(dir).ok_or(Stop::NotExport)?;
        let root = root.canonicalize().map_err(|_| Stop::NotExport)?;
        let state = root.join(STATE_DIR);
        refuse_links(&state)?;
        std::fs::create_dir_all(&state).map_err(storage)?;
        let mut lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(state.join("lock"))
            .map_err(storage)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Stop::Busy),
            Err(TryLockError::Error(e)) => return Err(storage(e)),
        }
        lock.set_len(0).map_err(storage)?;
        write!(lock, "{}", std::process::id()).map_err(storage)?;
        Ok(Export { root, _lock: lock })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }
}

fn storage(e: impl std::fmt::Display) -> Stop {
    Stop::Storage(e.to_string())
}

fn safe_component(c: &str) -> bool {
    (1..=64).contains(&c.len())
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && c != "."
        && c != ".."
        && !is_reserved(c)
}

/// Whole page, or `ErrorKind::FileTooLarge` past `MAX_PAGE` (stat first, then a bounded read).
pub fn read_page(p: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(p)?;
    let too_large = || std::io::Error::from(std::io::ErrorKind::FileTooLarge);
    if f.metadata()?.len() > MAX_PAGE {
        return Err(too_large());
    }
    let mut b = Vec::new();
    f.take(MAX_PAGE + 1).read_to_end(&mut b)?;
    if b.len() as u64 > MAX_PAGE {
        return Err(too_large());
    }
    Ok(b)
}

/// Joins a `/`-separated relative path; creates nothing.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, Stop> {
    let mut path = root.to_path_buf();
    for c in rel.split('/') {
        if !safe_component(c) {
            return Err(Stop::Storage("unsafe path".into()));
        }
        path.push(c);
    }
    Ok(path)
}

/// Refuses a symlink or junction at `dir` or anywhere under it; a missing `dir` is fine.
fn refuse_links(dir: &Path) -> Result<(), Stop> {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(storage(e)),
        Ok(m) if m.file_type().is_symlink() => return Err(Stop::Link),
        Ok(_) => {}
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).map_err(storage)? {
            let entry = entry.map_err(storage)?;
            let kind = entry.file_type().map_err(storage)?;
            if kind.is_symlink() {
                return Err(Stop::Link);
            }
            if kind.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

/// A new file at `p`: whatever sits there is unlinked first (a link is removed, never followed),
/// and `create_new` refuses anything that reappears in between.
pub fn create_fresh(p: &Path) -> std::io::Result<File> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    File::options().write(true).create_new(true).open(p)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceError {
    Busy,
    Io(String),
}

fn is_busy(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::PermissionDenied | ErrorKind::ResourceBusy
    )
}

/// Renames `tmp` over `dest`, retrying while another process holds `dest` (W1).
pub fn replace_file(tmp: &Path, dest: &Path) -> Result<(), ReplaceError> {
    let mut tries = 0;
    loop {
        match std::fs::rename(tmp, dest) {
            Ok(()) => return Ok(()),
            Err(e) if is_busy(&e) && tries < REPLACE_RETRIES => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(REPLACE_GAP_MS));
            }
            Err(e) if is_busy(&e) => return Err(ReplaceError::Busy),
            Err(e) => return Err(ReplaceError::Io(e.to_string())),
        }
    }
}

pub fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.into_owned()
    }
}

/// Every `*.html` under `root` as a sorted `/` path; refuses symlinks and junctions.
pub fn pages(root: &Path) -> Result<Vec<String>, Stop> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(storage)? {
            let entry = entry.map_err(storage)?;
            let kind = entry.file_type().map_err(storage)?;
            if kind.is_symlink() {
                return Err(Stop::Link);
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = format!("{prefix}{name}");
            if kind.is_dir() {
                if rel != STATE_DIR {
                    stack.push((entry.path(), format!("{rel}/")));
                }
            } else if name.to_ascii_lowercase().ends_with(".html") {
                out.push(rel);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::text::{decode_label, encode_cp1251};
    use super::*;
    use crate::testing::{FakeExportOpts, PEER_A, fake_export, tmpdir};

    fn mkexport(dir: &Path) {
        std::fs::create_dir_all(dir.join("messages")).unwrap();
        std::fs::write(dir.join("index.html"), b"x").unwrap();
    }

    #[test]
    fn find_root_direct_and_single_child() {
        let d = tmpdir("find-root");
        mkexport(&d);
        assert_eq!(find_root(&d), Some(d.clone()));

        let d = tmpdir("find-root-child");
        let child = d.join("Archive");
        std::fs::create_dir_all(child.join("photos")).unwrap();
        std::fs::write(child.join("index.html"), b"x").unwrap();
        std::fs::write(d.join("note.txt"), b"x").unwrap();
        assert_eq!(find_root(&d), Some(child));

        mkexport(&d.join("Archive (1)"));
        assert_eq!(find_root(&d), None);

        let d = tmpdir("find-root-none");
        std::fs::write(d.join("index.html"), b"x").unwrap();
        std::fs::create_dir_all(d.join("wall")).unwrap();
        assert_eq!(find_root(&d), None);
    }

    #[test]
    fn open_refuses_non_export() {
        let d = tmpdir("open-non-export");
        assert!(matches!(Export::open(&d), Err(Stop::NotExport)));
        std::fs::write(d.join("file.html"), b"x").unwrap();
        assert!(matches!(
            Export::open(&d.join("file.html")),
            Err(Stop::NotExport)
        ));
        assert!(matches!(
            Export::open(&d.join("missing")),
            Err(Stop::NotExport)
        ));
    }

    #[test]
    fn second_open_is_busy_until_drop() {
        let d = fake_export("busy", FakeExportOpts::default());
        let first = Export::open(&d).unwrap();
        assert_eq!(first.root(), d.canonicalize().unwrap());
        assert_eq!(first.state(), first.root().join(STATE_DIR));
        assert!(matches!(Export::open(&d), Err(Stop::Busy)));
        drop(first);
        let again = Export::open(&d).unwrap();
        let lock = again.state().join("lock");
        drop(again);
        assert_eq!(
            std::fs::read_to_string(lock).unwrap(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn safe_join_refuses_hostile() {
        let root = tmpdir("safe-join");
        let long = "a".repeat(65);
        for bad in [
            "..", "a/../b", "/x", "C:x", "a\\b", "", "con", "aux.jpg", "x.", "Фото", "a//b", &long,
            "a:b", "x.html:s", "C:", "//srv/x", "a/", "a ",
        ] {
            assert!(safe_join(&root, bad).is_err(), "{bad:?}");
        }
        let ok = safe_join(&root, "messages/2000000101/media/1001_1").unwrap();
        assert_eq!(
            ok,
            root.join("messages")
                .join("2000000101")
                .join("media")
                .join("1001_1")
        );
        assert!(!ok.exists());
        assert!(safe_join(&root, &"a".repeat(64)).is_ok());
    }

    #[test]
    fn pages_sorted_skip_state_dir() {
        let d = fake_export("pages", FakeExportOpts::default());
        let e = Export::open(&d).unwrap();
        std::fs::write(e.state().join("report.html"), b"x").unwrap();
        let got = pages(e.root()).unwrap();
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(got, sorted);
        assert!(
            got.iter()
                .all(|p| !p.starts_with("vk-saver/") && !p.contains('\\'))
        );
        assert!(got.contains(&format!("messages/{PEER_A}/messages50.html")));
        assert!(got.contains(&"index.html".to_string()));
        assert!(!got.iter().any(|p| p.ends_with(".css")));
        assert_eq!(got.len(), 12);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_refused() {
        let d = fake_export("symlink", FakeExportOpts::default());
        let outside = tmpdir("symlink-outside");
        std::fs::write(outside.join("x.html"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("photos").join("link")).unwrap();
        let root = d.canonicalize().unwrap();
        assert_eq!(pages(&root), Err(Stop::Link));
    }

    #[cfg(unix)]
    #[test]
    fn open_refuses_linked_state_dir() {
        let d = fake_export("state-link", FakeExportOpts::default());
        let outside = tmpdir("state-link-outside");
        std::os::unix::fs::symlink(&outside, d.join(STATE_DIR)).unwrap();
        assert!(matches!(Export::open(&d), Err(Stop::Link)));
        assert!(!outside.join("lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn open_refuses_links_inside_state_dir() {
        use std::os::unix::fs::symlink;
        let outside = tmpdir("state-inner-outside");
        let victim = outside.join("victim.txt");
        std::fs::write(&victim, b"keep").unwrap();
        let cases: [(&str, &dyn Fn(&Path)); 3] = [
            ("originals", &|s| {
                symlink(&outside, s.join("originals")).unwrap()
            }),
            ("lock", &|s| symlink(&victim, s.join("lock")).unwrap()),
            ("nested", &|s| {
                let dir = s.join("originals").join("messages").join(PEER_A);
                std::fs::create_dir_all(&dir).unwrap();
                symlink(&victim, dir.join("messages0.html")).unwrap();
            }),
        ];
        for (name, plant) in cases {
            let d = fake_export(&format!("state-inner-{name}"), FakeExportOpts::default());
            let state = d.join(STATE_DIR);
            std::fs::create_dir_all(&state).unwrap();
            plant(&state);
            assert!(matches!(Export::open(&d), Err(Stop::Link)), "{name}");
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep", "{name}");
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1, "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn create_fresh_unlinks_planted_link() {
        let d = tmpdir("fresh-link");
        let victim = d.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let tmp = d.join("x.tmp");
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();
        create_fresh(&tmp).unwrap().write_all(b"new").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(
            !std::fs::symlink_metadata(&tmp)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&tmp).unwrap(), b"new");
    }

    #[cfg(windows)]
    #[test]
    fn junction_refused() {
        let d = fake_export("junction", FakeExportOpts::default());
        let outside = tmpdir("junction-outside");
        let link = d.join("photos").join("link");
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let root = d.canonicalize().unwrap();
        assert_eq!(pages(&root), Err(Stop::Link));
    }

    #[cfg(windows)]
    #[test]
    fn open_refuses_junctions_in_state_dir() {
        let junction = |link: &Path, target: &Path| {
            let ok = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .status()
                .unwrap()
                .success();
            assert!(ok);
        };
        let d = fake_export("junction-state", FakeExportOpts::default());
        let outside = tmpdir("junction-state-outside");
        junction(&d.join(STATE_DIR), &outside);
        assert!(matches!(Export::open(&d), Err(Stop::Link)));
        assert!(!outside.join("lock").exists());

        let d = fake_export("junction-originals", FakeExportOpts::default());
        let outside = tmpdir("junction-originals-outside");
        std::fs::create_dir_all(d.join(STATE_DIR)).unwrap();
        junction(&d.join(STATE_DIR).join("originals"), &outside);
        assert!(matches!(Export::open(&d), Err(Stop::Link)));
        assert!(!d.join(STATE_DIR).join("lock").exists());
    }

    #[cfg(windows)]
    #[test]
    fn replace_file_busy_then_ok() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tmpdir("replace-busy");
        let (tmp, dest) = (d.join("page.html.tmp"), d.join("page.html"));
        std::fs::write(&tmp, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();
        let held = File::options()
            .read(true)
            .share_mode(0)
            .open(&dest)
            .unwrap();
        assert_eq!(replace_file(&tmp, &dest), Err(ReplaceError::Busy));
        drop(held);
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
        assert_eq!(replace_file(&tmp, &dest), Ok(()));
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    }

    #[test]
    fn replace_file_overwrites() {
        let d = tmpdir("replace-ok");
        let (tmp, dest) = (d.join("a.tmp"), d.join("a"));
        std::fs::write(&tmp, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();
        assert_eq!(replace_file(&tmp, &dest), Ok(()));
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(matches!(
            replace_file(&tmp, &dest),
            Err(ReplaceError::Io(_))
        ));
    }

    #[test]
    fn display_path_strips_verbatim() {
        assert_eq!(display_path(Path::new(r"\\?\C:\a")), r"C:\a");
        assert_eq!(display_path(Path::new(r"\\?\UNC\s\x")), r"\\s\x");
        assert_eq!(display_path(Path::new(r"C:\a")), r"C:\a");
    }

    #[test]
    fn cp1251_roundtrip_and_fixture_bytes() {
        let s = "Фотография Ёё";
        assert_eq!(decode_label(&encode_cp1251(s), false), s);

        let page = |opts| {
            let d = fake_export(
                if matches!(opts, FakeExportOpts { utf8: true, .. }) {
                    "bytes-utf8"
                } else {
                    "bytes"
                },
                opts,
            );
            std::fs::read(d.join("messages").join(PEER_A).join("messages0.html")).unwrap()
        };
        let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);

        let cp = page(FakeExportOpts::default());
        assert!(contains(&cp, &encode_cp1251("Фотография")));
        assert!(contains(&cp, b"charset=\"windows-1251\""));
        assert!(std::str::from_utf8(&cp).is_err());

        let u8 = page(FakeExportOpts {
            utf8: true,
            ..Default::default()
        });
        assert!(contains(&u8, "Фотография".as_bytes()));
        assert!(!contains(&u8, &encode_cp1251("Фотография")));
    }

    #[test]
    fn fixture_crlf_variant() {
        let all = |d: &Path| -> Vec<Vec<u8>> {
            pages(d)
                .unwrap()
                .iter()
                .map(|p| std::fs::read(safe_join(d, p).unwrap()).unwrap())
                .collect()
        };
        let lf = fake_export("lf", FakeExportOpts::default());
        assert!(all(&lf).iter().all(|b| !b.contains(&b'\r')));
        let crlf = fake_export(
            "crlf",
            FakeExportOpts {
                crlf: true,
                ..Default::default()
            },
        );
        for b in all(&crlf) {
            assert!(b.contains(&b'\n'));
            assert!(
                b.iter()
                    .enumerate()
                    .all(|(i, &c)| c != b'\n' || (i > 0 && b[i - 1] == b'\r'))
            );
        }
    }
}
