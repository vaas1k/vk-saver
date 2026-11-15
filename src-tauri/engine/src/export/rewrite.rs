use super::index::Index;
use super::scan::{PHOTO_LINK, attachment_link, link_tags, starts_with_ci, sweep};
use super::{ReplaceError, STATE_DIR, replace_file, safe_component, safe_join, storage};
use crate::error::Stop;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Link from `page` to `target`, both `/` paths from the export root.
pub fn rel_link(page: &str, target: &str) -> String {
    fn dirs(p: &str) -> (Vec<&str>, &str) {
        p.rsplit_once('/')
            .map_or((Vec::new(), p), |(d, n)| (d.split('/').collect(), n))
    }
    let (dir, _) = dirs(page);
    let (tdir, name) = dirs(target);
    let common = dir.iter().zip(&tdir).take_while(|(a, b)| a == b).count();
    let mut out = "../".repeat(dir.len() - common);
    for c in &tdir[common..] {
        out += c;
        out.push('/');
    }
    out + name
}

const INLINE_EXTS: [&str; 4] = [".jpg", ".png", ".gif", ".webp"];

/// End of the `</a>` closing the attachment anchor `tags[tag]`, when no other `<a` opens before it.
fn attachment_close(page: &[u8], tags: &[(Range<usize>, bool)], tag: usize) -> Option<usize> {
    for (r, _) in &tags[tag + 1..] {
        let t = &page[r.start..];
        if starts_with_ci(t, b"</a>") {
            return Some(r.start + 4);
        }
        if starts_with_ci(t, b"<a")
            && t.get(2)
                .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
        {
            return None;
        }
    }
    None
}

/// Index in `tags` of the `<img ...>` holding `value`.
fn img_tag(page: &[u8], tags: &[(Range<usize>, bool)], value: &Range<usize>) -> Option<usize> {
    let i = tags.partition_point(|(r, _)| r.end <= value.start);
    let (img, _) = tags.get(i)?;
    let t = &page[img.start..];
    // A `>` inside the value ends the tag early: such an `<img>` is left as it is.
    (img.start < value.start
        && value.end <= img.end
        && starts_with_ci(t, b"<img")
        && t.get(4).is_some_and(u8::is_ascii_whitespace))
    .then_some(i)
}

/// Bytes of the attachment anchor `tags[tag]` through its `</a>`, when that is the next tag and
/// the opening tag has no unclosed quote.
fn attachment_anchor(
    page: &[u8],
    tags: &[(Range<usize>, bool)],
    tag: usize,
) -> Option<Range<usize>> {
    let open = &tags[tag].0;
    let (close, _) = tags.get(tag + 1)?;
    let mut quote = None;
    for &b in &page[open.clone()] {
        match quote {
            Some(q) if b == q => quote = None,
            None if b == b'"' || b == b'\'' => quote = Some(b),
            _ => {}
        }
    }
    (quote.is_none() && starts_with_ci(&page[close.start..], b"</a>"))
        .then(|| open.start..close.start + 4)
}

/// "источник недоступен" as ASCII character references, valid in cp1251 and UTF-8 pages.
const UNAVAILABLE: &str = "<span class=\"vk-saver-missing\">&#1080;&#1089;&#1090;&#1086;&#1095;&#1085;&#1080;&#1082; &#1085;&#1077;&#1076;&#1086;&#1089;&#1090;&#1091;&#1087;&#1077;&#1085;</span>";

/// `href` value of the `<a ...>` directly wrapping the `<img>` that holds `value`.
fn wrapping_href(
    page: &[u8],
    tags: &[(Range<usize>, bool)],
    value: &Range<usize>,
    last_img: &mut Option<usize>,
) -> Option<Range<usize>> {
    let i = img_tag(page, tags, value)?;
    if last_img.replace(i) == Some(i) {
        return None;
    }
    let is_open_a =
        |t: &[u8]| starts_with_ci(t, b"<a") && t.get(2).is_some_and(u8::is_ascii_whitespace);
    let (open, _) = tags.get(i.checked_sub(1)?)?;
    let (close, _) = tags.get(i + 1)?;
    if !is_open_a(&page[open.clone()]) || !starts_with_ci(&page[close.start..], b"</a>") {
        return None;
    }
    let tag = &page[open.clone()];
    // First `href=` outside quoted values; an unclosed quote makes the tag ambiguous.
    let (mut quote, mut at) = (None, None);
    for j in 1..tag.len() {
        match (quote, tag[j]) {
            (Some(q), b) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b @ (b'"' | b'\'')) => quote = Some(b),
            (None, _) => {
                if at.is_none()
                    && tag[j - 1].is_ascii_whitespace()
                    && starts_with_ci(&tag[j..], b"href=")
                {
                    at = Some(j);
                }
            }
        }
    }
    let q = at? + 5;
    if quote.is_some() {
        return None;
    }
    let quote @ (b'"' | b'\'') = *tag.get(q)? else {
        return None;
    };
    let len = tag[q + 1..].iter().position(|&b| b == quote)?;
    let href = &tag[q + 1..q + 1 + len];
    // Only a VK photo page or the image itself: any other link (a preview card) stays.
    let bare = [&b"https://"[..], b"http://"]
        .iter()
        .find_map(|s| href.strip_prefix(*s))
        .unwrap_or(href);
    let photo = bare
        .strip_prefix(PHOTO_LINK)
        .and_then(|r| r.first())
        .is_some_and(|c| c.is_ascii_digit() || *c == b'-');
    (photo || href == &page[value.clone()]).then(|| open.start + q + 1..open.start + q + 1 + len)
}

/// Swaps only the value bytes of saved URLs, replaces an `<img>` or an `attachment__link` anchor
/// of an unavailable one with a fixed text, points the anchor wrapping a saved `<img>` at the
/// same file and inserts a fixed linked `<img>` after each saved image `attachment__link`; every
/// other byte is copied as is.
pub fn rewrite_page(
    original: &[u8],
    page: &str,
    saved: &dyn Fn(&str) -> Option<String>,
    unavailable: &dyn Fn(&str) -> bool,
) -> Vec<u8> {
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let tags = link_tags(original);
    let mut last_tag = None;
    let mut last_img = None;
    for a in sweep(original) {
        let Some(path) = saved(&a.url) else {
            if !unavailable(&a.url) {
                continue;
            }
            if let Some(i) = img_tag(original, &tags, &a.value) {
                let img = &tags[i].0;
                edits.push((img.start..img.end + 1, UNAVAILABLE.to_string()));
            } else if let Some(anchor) = attachment_link(original, &tags, &a.value)
                .and_then(|tag| attachment_anchor(original, &tags, tag))
            {
                edits.push((anchor, UNAVAILABLE.to_string()));
            }
            continue;
        };
        let link = rel_link(page, &path);
        let image = INLINE_EXTS
            .iter()
            .any(|e| path.to_ascii_lowercase().ends_with(e));
        // The link goes inside a quoted attribute: only safe-alphabet ASCII paths.
        let safe = path.split('/').all(safe_component);
        if image
            && safe
            && let Some(tag) = attachment_link(original, &tags, &a.value)
            && last_tag.replace(tag) != Some(tag)
            && let Some(end) = attachment_close(original, &tags, tag)
        {
            edits.push((end..end, format!(
                "<a href=\"{link}\"><img class=\"vk-saver-inline\" src=\"{link}\" loading=\"lazy\" alt=\"\" style=\"display:block;max-width:320px;max-height:320px\"></a>"
            )));
        }
        if safe && let Some(href) = wrapping_href(original, &tags, &a.value, &mut last_img) {
            edits.push((href, link.clone()));
        }
        edits.push((a.value, link));
    }
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = Vec::with_capacity(original.len());
    let mut at = 0;
    for (r, bytes) in edits {
        // An insertion point inside an already replaced value is dropped, never misplaced.
        if r.start < at {
            continue;
        }
        out.extend_from_slice(&original[at..r.start]);
        out.extend_from_slice(bytes.as_bytes());
        at = r.end;
    }
    out.extend_from_slice(&original[at..]);
    out
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RewriteReport {
    pub pages: u64,
    pub changed: u64,
    pub busy: u64,
    pub skipped: u64,
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn read_opt(p: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match super::read_page(p) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn with(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn listed(sidecar: &[u8], hash: &str) -> bool {
    sidecar.split(|c| *c == b'\n').any(|l| l == hash.as_bytes())
}

/// Appends `hash` as its own line, even after a torn last line, and fsyncs.
fn append_hash(path: &Path, sidecar: &[u8], hash: &str) -> std::io::Result<()> {
    let sep = if sidecar.last().is_some_and(|c| *c != b'\n') {
        "\n"
    } else {
        ""
    };
    let mut f = File::options().create(true).append(true).open(path)?;
    f.write_all(format!("{sep}{hash}\n").as_bytes())?;
    f.sync_all()
}

/// Only a name that is certainly absent is free: a link or an unreadable entry is not.
fn free(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_err_and(|e| e.kind() == ErrorKind::NotFound)
}

/// Moves the backup and its sidecar to the first `.old-<n>` where neither name exists. Sidecar
/// first: a crash in between leaves a backup without a sidecar, archived on the next run.
fn archive(backup: &Path) -> std::io::Result<()> {
    let old = (1..)
        .map(|n| with(backup, &format!(".old-{n}")))
        .find(|p| free(p) && free(&with(p, ".outputs")))
        .expect("a free slot");
    match std::fs::rename(with(backup, ".outputs"), with(&old, ".outputs")) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::rename(backup, old)
}

/// Writes `<dest>.tmp`, fsyncs it, renames it over `dest`; the `.tmp` never survives a failure (W1).
fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<(), ReplaceError> {
    let tmp = with(dest, ".tmp");
    let r = super::create_fresh(&tmp)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        })
        .map_err(|e| ReplaceError::Io(e.to_string()))
        .and_then(|()| replace_file(&tmp, dest));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Replaces `dest` only while it still holds `seen`: a page changed since it was read (a newer
/// export unpacked during the run) is left for the next run and counted busy.
fn replace_if_unchanged(dest: &Path, seen: &[u8], bytes: &[u8]) -> Result<(), ReplaceError> {
    match read_opt(dest) {
        Ok(Some(now)) if now == seen => write_atomic(dest, bytes),
        _ => Err(ReplaceError::Busy),
    }
}

/// The backup and its sidecar of `page`.
struct Originals {
    backup: Option<Vec<u8>>,
    sidecar: Option<Vec<u8>>,
    /// `live` is the backup or our own output of it.
    ours: bool,
}

fn originals(backup_path: &Path, live: &[u8]) -> std::io::Result<Originals> {
    let backup = read_opt(backup_path)?;
    let sidecar = read_opt(&with(backup_path, ".outputs"))?;
    let ours = backup.as_ref().is_some_and(|b| {
        live == b.as_slice() || sidecar.as_ref().is_some_and(|s| listed(s, &sha(live)))
    });
    Ok(Originals {
        backup,
        sidecar,
        ours,
    })
}

/// The export's own bytes of `page`: its backup while the live page is our output, else `live`.
pub(super) fn export_bytes(root: &Path, page: &str, live: Vec<u8>) -> std::io::Result<Vec<u8>> {
    let Ok(backup_path) = safe_join(root, &format!("{STATE_DIR}/originals/{page}")) else {
        return Ok(live);
    };
    // An unreadable backup or sidecar in the untrusted export never hides the live page.
    match originals(&backup_path, &live) {
        Ok(Originals {
            backup: Some(backup),
            ours: true,
            ..
        }) => Ok(backup),
        _ => Ok(live),
    }
}

/// The backup `originals/<page>` is never overwritten. The live page is rewritten from it when the
/// live bytes equal it or their sha256 is in `<backup>.outputs` (every output we ever wrote,
/// appended before the page is replaced); any other live page is a new original: the old backup
/// moves to `.old-<n>` and the live page becomes the backup.
pub fn rewrite_all(root: &Path, index: &Index, pages: &[String]) -> Result<RewriteReport, Stop> {
    let saved = index.saved_paths()?;
    let unavailable = index.unavailable_urls()?;
    let lookup = |url: &str| {
        saved
            .get(url)
            .filter(|p| safe_join(root, p).is_ok_and(|f| f.is_file()))
            .cloned()
    };
    let mut rep = RewriteReport::default();
    for page in pages {
        let backup_rel = format!("{STATE_DIR}/originals/{page}");
        // A page name outside the safe alphabet has no backup slot, so it is never written.
        let (Ok(live_path), Ok(backup_path)) =
            (safe_join(root, page), safe_join(root, &backup_rel))
        else {
            rep.skipped += 1;
            continue;
        };
        rep.pages += 1;
        let live = match read_opt(&live_path) {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(e) if e.kind() == ErrorKind::FileTooLarge => {
                rep.skipped += 1;
                continue;
            }
            // Held without read sharing (W1): left for the next run.
            Err(_) => {
                rep.busy += 1;
                continue;
            }
        };
        let outputs = with(&backup_path, ".outputs");
        let Originals {
            backup,
            sidecar,
            ours,
        } = originals(&backup_path, &live).map_err(storage)?;
        // Without its backup a sidecar cannot say whose the live page is: never adopt it.
        if backup.is_none() && sidecar.is_some() {
            rep.skipped += 1;
            continue;
        }
        let mut sidecar = sidecar.unwrap_or_default();
        let from_backup = backup.as_ref().filter(|_| ours);
        let output = rewrite_page(from_backup.unwrap_or(&live), page, &lookup, &|u| {
            unavailable.contains(u)
        });
        if output == live {
            continue;
        }
        if from_backup.is_none() {
            if backup.is_some() {
                archive(&backup_path).map_err(storage)?;
            }
            sidecar.clear();
            std::fs::create_dir_all(backup_path.parent().expect("backup has a parent"))
                .map_err(storage)?;
            match write_atomic(&backup_path, &live) {
                Ok(()) => {}
                Err(ReplaceError::Busy) => {
                    rep.busy += 1;
                    continue;
                }
                Err(ReplaceError::Io(e)) => return Err(Stop::Storage(e)),
            }
        }
        let hash = sha(&output);
        if !listed(&sidecar, &hash) {
            append_hash(&outputs, &sidecar, &hash).map_err(storage)?;
        }
        match replace_if_unchanged(&live_path, &live, &output) {
            Ok(()) => rep.changed += 1,
            Err(ReplaceError::Busy) => rep.busy += 1,
            Err(ReplaceError::Io(e)) => return Err(Stop::Storage(e)),
        }
    }
    Ok(rep)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UndoReport {
    pub restored: u64,
    pub busy: u64,
    /// Live pages that are neither our output nor the backup: changed since, left as they are.
    pub kept: u64,
    /// Live pages with a backup that are over `MAX_PAGE`, never read.
    pub skipped: u64,
}

/// Puts the newest backup back over each live page that is our output; a page changed since
/// (or gone) is kept. Backups, sidecars and media stay, so undo repeats and a later run rewrites again.
pub fn undo(root: &Path) -> Result<UndoReport, Stop> {
    let mut rep = UndoReport::default();
    for page in super::pages(root)? {
        let (Ok(live_path), Ok(backup_path)) = (
            safe_join(root, &page),
            safe_join(root, &format!("{STATE_DIR}/originals/{page}")),
        ) else {
            continue;
        };
        let Some(backup) = read_opt(&backup_path).map_err(storage)? else {
            continue;
        };
        if std::fs::metadata(&live_path).is_ok_and(|m| m.len() > super::MAX_PAGE) {
            rep.skipped += 1;
            continue;
        }
        let live = match read_opt(&live_path) {
            Ok(Some(b)) => b,
            Ok(None) => {
                rep.kept += 1;
                continue;
            }
            Err(_) => {
                rep.busy += 1;
                continue;
            }
        };
        if live == backup {
            continue;
        }
        let sidecar = read_opt(&with(&backup_path, ".outputs"))
            .map_err(storage)?
            .unwrap_or_default();
        if !listed(&sidecar, &sha(&live)) {
            rep.kept += 1;
            continue;
        }
        match replace_if_unchanged(&live_path, &live, &backup) {
            Ok(()) => rep.restored += 1,
            Err(ReplaceError::Busy) => rep.busy += 1,
            Err(ReplaceError::Io(e)) => return Err(Stop::Storage(e)),
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::{Media, Saved};
    use crate::export::index::{Outcome, Status};
    use crate::export::scan::scan;
    use crate::testing::{
        FakeExportOpts, OWNER, PEER_A, URL_ALBUM_1, URL_CHAT_1, URL_CHAT_2, URL_CHAT_3, URL_VOICE,
        fake_export,
    };
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    fn page_a() -> String {
        format!("messages/{PEER_A}/messages0.html")
    }

    fn page_a50() -> String {
        format!("messages/{PEER_A}/messages50.html")
    }

    fn backup(root: &Path, page: &str) -> PathBuf {
        safe_join(root, &format!("vk-saver/originals/{page}")).unwrap()
    }

    fn lines(p: &Path) -> Vec<String> {
        std::fs::read_to_string(p)
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn read(root: &Path, page: &str) -> Vec<u8> {
        std::fs::read(safe_join(root, page).unwrap()).unwrap()
    }

    struct Fx {
        root: PathBuf,
        ix: Index,
        pages: Vec<String>,
        pristine: HashMap<String, Vec<u8>>,
    }

    fn setup(name: &str, crlf: bool) -> Fx {
        let root = fake_export(name, FakeExportOpts { crlf, utf8: false });
        let s = scan(&root, &AtomicBool::new(false)).unwrap();
        std::fs::create_dir_all(root.join("vk-saver")).unwrap();
        let ix = Index::open(&root.join("vk-saver")).unwrap();
        ix.sync(&s.found).unwrap();
        let pristine = s
            .pages
            .iter()
            .map(|p| (p.clone(), read(&root, p)))
            .collect();
        Fx {
            root,
            ix,
            pages: s.pages,
            pristine,
        }
    }

    /// A `Saved` mark also puts the file on disk: links point only at files that exist.
    fn mark(root: &Path, ix: &Index, url: &str, status: Status) {
        let saved = Saved {
            path: PathBuf::new(),
            bytes: 3,
            media: Media::Jpg,
            sha256: "cd".repeat(32),
        };
        let o = Outcome {
            status,
            http: Some(200),
            saved: (status == Status::Saved).then_some(&saved),
            now_s: 1_700_000_000,
        };
        ix.record(url, &o).unwrap();
        if status == Status::Saved {
            let f = safe_join(root, &ix.saved_paths().unwrap()[url]).unwrap();
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"jpg").unwrap();
        }
    }

    fn lookup(ix: &Index) -> impl Fn(&str) -> Option<String> {
        let m = ix.saved_paths().unwrap();
        move |u: &str| m.get(u).cloned()
    }

    fn none(_: &str) -> bool {
        false
    }

    fn esc(url: &str) -> String {
        url.replace('&', "&amp;")
    }

    fn count(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    #[test]
    fn rel_link_cases() {
        let a = "messages/2000000101/messages0.html";
        assert_eq!(
            rel_link(a, "messages/2000000101/media/1001_1.jpg"),
            "media/1001_1.jpg"
        );
        assert_eq!(
            rel_link(a, "messages/2000000102/media/1001_1.jpg"),
            "../2000000102/media/1001_1.jpg"
        );
        assert_eq!(
            rel_link(
                "photos/photo-albums/album-1.html",
                "photos/photo-albums/media/200001_5001.jpg"
            ),
            "media/200001_5001.jpg"
        );
        assert_eq!(
            rel_link("index.html", "profile/media/avatar.jpg"),
            "profile/media/avatar.jpg"
        );
        assert_eq!(
            rel_link(a, "profile/media/avatar.jpg"),
            "../../profile/media/avatar.jpg"
        );
        assert_eq!(
            rel_link("index.html", "vk-saver/media/u_ab.jpg"),
            "vk-saver/media/u_ab.jpg"
        );
    }

    #[test]
    fn non_url_bytes_identical() {
        for crlf in [false, true] {
            let fx = setup(&format!("rw-bytes-{crlf}"), crlf);
            let page = page_a();
            let orig = &fx.pristine[&page];
            let target = format!("messages/{PEER_A}/media/x.bin");
            let saved = |u: &str| (u == URL_CHAT_1 || u == URL_CHAT_2).then(|| target.clone());
            let out = rewrite_page(orig, &page, &saved, &none);
            let hits: Vec<_> = sweep(orig)
                .into_iter()
                .filter(|a| saved(&a.url).is_some())
                .collect();
            assert_eq!(hits.len(), 2);
            let mut expect = Vec::new();
            let mut at = 0;
            for a in &hits {
                expect.extend_from_slice(&orig[at..a.value.start]);
                expect.extend_from_slice(b"media/x.bin");
                at = a.value.end;
            }
            expect.extend_from_slice(&orig[at..]);
            assert_eq!(out, expect);
            assert_eq!(count(&out, b"\r\n"), count(orig, b"\r\n"));
            assert_eq!(count(&out, b"\r\n") > 0, crlf);
            assert_eq!(
                out.iter().filter(|b| **b >= 0x80).count(),
                orig.iter().filter(|b| **b >= 0x80).count()
            );
            assert_eq!(count(&out, b"href=\"messages50.html\".html>"), 1);
            assert_eq!(count(&out, b"href='media/x.bin'"), 2);
        }
    }

    #[test]
    fn unsaved_links_keep_cdn_url() {
        let fx = setup("rw-unsaved", false);
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        mark(&fx.root, &fx.ix, URL_CHAT_2, Status::NotMedia);
        mark(&fx.root, &fx.ix, URL_CHAT_3, Status::HostDead);
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        let out = read(&fx.root, &page_a());
        for u in [URL_CHAT_2, URL_CHAT_3] {
            assert_eq!(count(&out, format!("href='{}'", esc(u)).as_bytes()), 1);
        }
        assert_eq!(
            count(&out, format!("href='{}'", esc(URL_CHAT_1)).as_bytes()),
            0
        );
    }

    #[test]
    fn backup_byte_identical_before_first_write() {
        for crlf in [false, true] {
            let fx = setup(&format!("rw-backup-{crlf}"), crlf);
            mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
            let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
            assert_eq!(r.changed, 2);
            assert_eq!(r.busy, 0);
            assert_eq!(r.pages, fx.pages.len() as u64);
            for p in &fx.pages {
                let b = backup(&fx.root, p);
                if *p == page_a() || *p == page_a50() {
                    assert_eq!(std::fs::read(&b).unwrap(), fx.pristine[p]);
                    assert_ne!(read(&fx.root, p), fx.pristine[p]);
                } else {
                    assert!(!b.exists(), "{p}");
                    assert_eq!(read(&fx.root, p), fx.pristine[p]);
                }
            }
        }
    }

    #[test]
    fn rewrite_twice_same_bytes() {
        let fx = setup("rw-twice", false);
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        let b = backup(&fx.root, &page_a());
        assert_eq!(
            lines(&with(&b, ".outputs")),
            [sha(&read(&fx.root, &page_a()))]
        );
        let snap = |p: &String| {
            let b = backup(&fx.root, p);
            (
                read(&fx.root, p),
                std::fs::read(&b).ok(),
                std::fs::read(with(&b, ".outputs")).ok(),
            )
        };
        let first: Vec<_> = fx.pages.iter().map(snap).collect();
        let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(r.changed, 0);
        assert_eq!(fx.pages.iter().map(snap).collect::<Vec<_>>(), first);
    }

    #[test]
    fn more_saved_later_rewrites_from_backup() {
        let fx = setup("rw-more", false);
        let page = page_a();
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        mark(&fx.root, &fx.ix, URL_CHAT_2, Status::Saved);
        let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(r.changed, 1);
        let b = std::fs::read(backup(&fx.root, &page)).unwrap();
        assert_eq!(b, fx.pristine[&page]);
        assert_eq!(
            read(&fx.root, &page),
            rewrite_page(&b, &page, &lookup(&fx.ix), &none)
        );
        assert_eq!(count(&read(&fx.root, &page), esc(URL_CHAT_2).as_bytes()), 1);
    }

    #[test]
    fn externally_changed_page_becomes_new_original() {
        let fx = setup("rw-external", false);
        let page = page_a();
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        let mut newer = fx.pristine[&page].clone();
        newer.extend_from_slice(b"<p>\xd2\xe5\xf1\xf2</p>\n");
        std::fs::write(safe_join(&fx.root, &page).unwrap(), &newer).unwrap();
        let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(r.changed, 1);
        let b = backup(&fx.root, &page);
        assert_eq!(std::fs::read(&b).unwrap(), newer);
        assert_eq!(
            read(&fx.root, &page),
            rewrite_page(&newer, &page, &lookup(&fx.ix), &none)
        );
        assert!(!backup(&fx.root, &format!("{page}.tmp")).exists());
        assert_eq!(
            std::fs::read(with(&b, ".old-1")).unwrap(),
            fx.pristine[&page]
        );
        assert_eq!(lines(&with(&b, ".old-1.outputs")).len(), 1);
        assert_eq!(lines(&with(&b, ".outputs")), [sha(&read(&fx.root, &page))]);

        let mut newest = newer.clone();
        newest.extend_from_slice(b"<p>2</p>\n");
        std::fs::write(safe_join(&fx.root, &page).unwrap(), &newest).unwrap();
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(std::fs::read(with(&b, ".old-2")).unwrap(), newer);
        assert_eq!(
            std::fs::read(with(&b, ".old-1")).unwrap(),
            fx.pristine[&page]
        );
        assert_eq!(std::fs::read(&b).unwrap(), newest);
        assert_eq!(super::super::pages(&fx.root).unwrap(), fx.pages);
    }

    #[test]
    fn crash_after_rename_then_more_saved() {
        let fx = setup("rw-c1", false);
        let page = page_a();
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        let out = rewrite_page(&fx.pristine[&page], &page, &lookup(&fx.ix), &none);
        let b = backup(&fx.root, &page);
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&b, &fx.pristine[&page]).unwrap();
        std::fs::write(with(&b, ".outputs"), format!("{}\n", sha(&out))).unwrap();
        std::fs::write(safe_join(&fx.root, &page).unwrap(), &out).unwrap();
        mark(&fx.root, &fx.ix, URL_CHAT_2, Status::Saved);
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), fx.pristine[&page]);
        assert_eq!(
            read(&fx.root, &page),
            rewrite_page(&fx.pristine[&page], &page, &lookup(&fx.ix), &none)
        );
        assert!(!with(&b, ".old-1").exists());
    }

    #[test]
    fn index_recreated_keeps_backup() {
        let Fx {
            root,
            ix,
            pages,
            pristine,
        } = setup("rw-c2", false);
        mark(&root, &ix, URL_CHAT_1, Status::Saved);
        rewrite_all(&root, &ix, &pages).unwrap();
        drop(ix);
        std::fs::remove_file(root.join("vk-saver").join("index.db")).unwrap();
        let ix = Index::open(&root.join("vk-saver")).unwrap();
        rewrite_all(&root, &ix, &pages).unwrap();
        ix.sync(&scan(&root, &AtomicBool::new(false)).unwrap().found)
            .unwrap();
        mark(&root, &ix, URL_CHAT_2, Status::Saved);
        rewrite_all(&root, &ix, &pages).unwrap();
        for p in [page_a(), page_a50()] {
            let b = backup(&root, &p);
            assert_eq!(std::fs::read(&b).unwrap(), pristine[&p]);
            assert!(!with(&b, ".old-1").exists());
        }
    }

    #[test]
    fn crash_after_sidecar_before_rename() {
        let fx = setup("rw-crash-side", false);
        let page = page_a();
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        let out = rewrite_page(&fx.pristine[&page], &page, &lookup(&fx.ix), &none);
        let b = backup(&fx.root, &page);
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&b, &fx.pristine[&page]).unwrap();
        std::fs::write(with(&b, ".outputs"), format!("{}\n", sha(&out))).unwrap();
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), fx.pristine[&page]);
        assert_eq!(read(&fx.root, &page), out);
        assert!(!with(&b, ".old-1").exists());
        assert_eq!(lines(&with(&b, ".outputs")), [sha(&out)]);
    }

    #[test]
    fn crash_between_backup_and_rename() {
        let fx = setup("rw-crash", false);
        let page = page_a();
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        let b = backup(&fx.root, &page);
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&b, &fx.pristine[&page]).unwrap();
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), fx.pristine[&page]);
        assert_eq!(
            read(&fx.root, &page),
            rewrite_page(&fx.pristine[&page], &page, &lookup(&fx.ix), &none)
        );
    }

    #[cfg(windows)]
    #[test]
    fn locked_page_left_intact_and_counted() {
        use std::os::windows::fs::OpenOptionsExt;
        // 0 = no sharing (reads fail too); 1 = FILE_SHARE_READ, like a preview pane (rename fails).
        for share in [0, 1] {
            let fx = setup(&format!("rw-locked-{share}"), false);
            let page = page_a();
            let live = safe_join(&fx.root, &page).unwrap();
            mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
            let held = File::options()
                .read(true)
                .share_mode(share)
                .open(&live)
                .unwrap();
            let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
            assert_eq!((r.busy, r.changed), (1, 1));
            drop(held);
            assert_eq!(std::fs::read(&live).unwrap(), fx.pristine[&page]);
            let mut tmp = live.clone().into_os_string();
            tmp.push(".tmp");
            assert!(!PathBuf::from(tmp).exists());
            let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
            assert_eq!((r.busy, r.changed), (0, 1));
            assert_eq!(
                std::fs::read(backup(&fx.root, &page)).unwrap(),
                fx.pristine[&page]
            );
            assert_ne!(std::fs::read(&live).unwrap(), fx.pristine[&page]);
        }
    }

    fn inline(path: &str) -> String {
        format!(
            "<a href=\"{path}\"><img class=\"vk-saver-inline\" src=\"{path}\" loading=\"lazy\" alt=\"\" style=\"display:block;max-width:320px;max-height:320px\"></a>"
        )
    }

    #[test]
    fn inline_img_after_photo_link() {
        for crlf in [false, true] {
            let fx = setup(&format!("rw-inline-{crlf}"), crlf);
            let page = page_a();
            let orig = &fx.pristine[&page];
            let target = format!("messages/{PEER_A}/media/1001_1.jpg");
            let saved = |u: &str| (u == URL_CHAT_1).then(|| target.clone());
            let out = rewrite_page(orig, &page, &saved, &none);
            let link = format!("<a class='attachment__link' href='{}'>", esc(URL_CHAT_1));
            let close = link.len() + esc(URL_CHAT_1).len() + "</a>".len();
            let at = orig
                .windows(link.len())
                .position(|w| w == link.as_bytes())
                .unwrap()
                + close;
            let mut expect = orig[..at].to_vec();
            expect.extend_from_slice(inline("media/1001_1.jpg").as_bytes());
            expect.extend_from_slice(&orig[at..]);
            let a = sweep(orig)
                .into_iter()
                .find(|a| a.url == URL_CHAT_1)
                .unwrap();
            expect.splice(a.value.clone(), b"media/1001_1.jpg".iter().copied());
            assert_eq!(out, expect);
            let pair = format!(
                "href='media/1001_1.jpg'>{}</a>{}",
                esc(URL_CHAT_1),
                inline("media/1001_1.jpg")
            );
            assert_eq!(count(&out, pair.as_bytes()), 1);
        }
    }

    #[test]
    fn no_inline_for_voice_heic_or_unsaved() {
        let fx = setup("rw-inline-no", false);
        let page = page_a();
        let orig = &fx.pristine[&page];
        let dir = format!("messages/{PEER_A}/media");
        let saved = |u: &str| match u {
            URL_VOICE => Some(format!("{dir}/1002_3.ogg")),
            URL_CHAT_2 => Some(format!("{dir}/1002_1.heic")),
            _ => None,
        };
        let out = rewrite_page(orig, &page, &saved, &none);
        assert_eq!(count(&out, b"vk-saver-inline"), 0);
        assert_eq!(count(&out, b"href='media/1002_3.ogg'"), 1);
        assert_eq!(count(&out, b"href='media/1002_1.heic'"), 1);
    }

    #[test]
    fn no_inline_in_album_pages() {
        let fx = setup("rw-inline-album", false);
        let page = "photos/photo-albums/album-1.html".to_string();
        let orig = &fx.pristine[&page];
        let saved = |u: &str| {
            (u == URL_ALBUM_1).then(|| "photos/photo-albums/media/200001_5001.jpg".to_string())
        };
        let out = rewrite_page(orig, &page, &saved, &none);
        assert_eq!(count(&out, b"src=\"media/200001_5001.jpg\""), 1);
        assert_eq!(count(&out, b"vk-saver-inline"), 0);
        assert_eq!(count(&out, b"<a href=\"media/200001_5001.jpg\"><img"), 1);
    }

    const ALBUM_PAGE: &str = "photos/photo-albums/album-7.html";
    const PHOTO_A: &str = "https://sun1-2.userapi.com/impg/a1/700001.jpg?size=10x10&type=album";
    const PHOTO_B: &str = "https://sun1-3.userapi.com/impg/b2/700002.jpg?size=10x10&type=album";

    fn album_item(id: u32, url: &str) -> String {
        format!(
            "<div class=\"item\"><div><a href=\"https://vk.ru/photo-7_{id}\"><img src=\"{}\" alt=\"\"></a></div><div><a href=\"https://vk.ru/photo-7_{id}\">https://vk.ru/photo-7_{id}</a></div></div>\n",
            esc(url)
        )
    }

    fn album_saved(u: &str) -> Option<String> {
        (u == PHOTO_A).then(|| "photos/photo-albums/media/7_700001.jpg".to_string())
    }

    #[test]
    fn saved_album_photo_anchor_points_to_file() {
        let orig = album_item(700001, PHOTO_A);
        let out = rewrite_page(orig.as_bytes(), ALBUM_PAGE, &album_saved, &none);
        let expect = orig
            .replacen("https://vk.ru/photo-7_700001", "media/7_700001.jpg", 1)
            .replace(&esc(PHOTO_A), "media/7_700001.jpg");
        assert_eq!(String::from_utf8(out).unwrap(), expect);
    }

    #[test]
    fn album_text_link_stays_remote() {
        let out = rewrite_page(
            album_item(700001, PHOTO_A).as_bytes(),
            ALBUM_PAGE,
            &album_saved,
            &none,
        );
        let text = b"<a href=\"https://vk.ru/photo-7_700001\">https://vk.ru/photo-7_700001</a>";
        assert_eq!(count(&out, text), 1);
        assert_eq!(
            count(
                &out,
                b"<a href=\"media/7_700001.jpg\"><img src=\"media/7_700001.jpg\""
            ),
            1
        );
    }

    #[test]
    fn unsaved_album_photo_untouched() {
        let orig = album_item(700002, PHOTO_B);
        assert_eq!(
            rewrite_page(orig.as_bytes(), ALBUM_PAGE, &album_saved, &none),
            orig.into_bytes()
        );
    }

    #[test]
    fn bare_img_without_anchor_gets_no_anchor() {
        let orig = format!(
            "<div><img src=\"{}\"></div><a href=\"x\">t</a>",
            esc(PHOTO_A)
        );
        let out = rewrite_page(orig.as_bytes(), ALBUM_PAGE, &album_saved, &none);
        assert_eq!(
            out,
            orig.replace(&esc(PHOTO_A), "media/7_700001.jpg")
                .into_bytes()
        );
    }

    #[test]
    fn foreign_wrapping_href_stays() {
        let orig = format!(
            "<a href=\"https://vk.com/away.php?to=https%3A%2F%2Fexample.org\"><img src=\"{}\"></a>",
            esc(PHOTO_A)
        );
        let out = rewrite_page(orig.as_bytes(), ALBUM_PAGE, &album_saved, &none);
        let expect = orig.replace(&esc(PHOTO_A), "media/7_700001.jpg");
        assert_eq!(String::from_utf8(out).unwrap(), expect);
    }

    #[test]
    fn photos_listing_href_stays() {
        let orig = format!(
            "<a href=\"https://vk.ru/photos-7\"><img src=\"{}\"></a>",
            esc(PHOTO_A)
        );
        let out = rewrite_page(orig.as_bytes(), ALBUM_PAGE, &album_saved, &none);
        let expect = orig.replace(&esc(PHOTO_A), "media/7_700001.jpg");
        assert_eq!(String::from_utf8(out).unwrap(), expect);
    }

    #[test]
    fn wrapping_href_equal_to_img_url_swapped() {
        let orig = format!("<a href=\"{0}\"><img src=\"{0}\"></a>", esc(PHOTO_A));
        let out = rewrite_page(orig.as_bytes(), ALBUM_PAGE, &album_saved, &none);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "<a href=\"media/7_700001.jpg\"><img src=\"media/7_700001.jpg\"></a>"
        );
    }

    #[test]
    fn href_inside_other_attribute_ignored() {
        let orig = format!(
            "<a title=\" href='t'\" href=\"https://vk.ru/photo-7_700001\"><img src=\"{}\"></a>",
            esc(PHOTO_A)
        );
        let out = rewrite_page(orig.as_bytes(), ALBUM_PAGE, &album_saved, &none);
        let expect = orig
            .replacen("https://vk.ru/photo-7_700001", "media/7_700001.jpg", 1)
            .replace(&esc(PHOTO_A), "media/7_700001.jpg");
        assert_eq!(String::from_utf8(out).unwrap(), expect);
    }

    #[test]
    fn ambiguous_img_tags_left_alone() {
        let gone = |u: &str| u == PHOTO_B;
        for orig in [
            format!("<image src=\"{}\">", esc(PHOTO_B)),
            format!("<imgx src=\"{}\">", esc(PHOTO_B)),
            format!("<img src=\"{}&x=>y\">t", esc(PHOTO_B)),
        ] {
            let out = rewrite_page(orig.as_bytes(), ALBUM_PAGE, &album_saved, &gone);
            assert_eq!(count(&out, UNAVAILABLE.as_bytes()), 0, "{orig}");
        }
        let orig = format!(
            "<a title=\"x>y\" href=\"https://vk.ru/photo-7_700001\"><img src=\"{}\"></a>",
            esc(PHOTO_A)
        );
        let out = rewrite_page(orig.as_bytes(), ALBUM_PAGE, &album_saved, &none);
        assert_eq!(count(&out, b"https://vk.ru/photo-7_700001"), 1);
    }

    #[test]
    fn gone_album_photo_shows_text_once() {
        let fx = setup("rw-album-gone", false);
        let page = "photos/photo-albums/album-1.html";
        mark(&fx.root, &fx.ix, URL_ALBUM_1, Status::Gone);
        assert_eq!(rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap().changed, 1);
        let first = read(&fx.root, page);
        let item = format!("<a href=\"https://vk.ru/photo{OWNER}_5001\">{UNAVAILABLE}</a>");
        assert_eq!(count(&first, item.as_bytes()), 1);
        assert_eq!(count(&first, esc(URL_ALBUM_1).as_bytes()), 0);
        assert_eq!(
            count(&first, b"<img") + 1,
            count(&fx.pristine[page], b"<img")
        );
        assert_eq!(rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap().changed, 0);
        assert_eq!(read(&fx.root, page), first);
        undo(&fx.root).unwrap();
        assert_pristine(&fx);
    }

    #[test]
    fn failed_then_saved_album_photo_shows_file() {
        let fx = setup("rw-album-failed", false);
        let page = "photos/photo-albums/album-1.html";
        mark(&fx.root, &fx.ix, URL_ALBUM_1, Status::Failed);
        assert_eq!(rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap().changed, 1);
        assert_eq!(count(&read(&fx.root, page), UNAVAILABLE.as_bytes()), 1);
        mark(&fx.root, &fx.ix, URL_ALBUM_1, Status::Saved);
        assert_eq!(rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap().changed, 1);
        let now = read(&fx.root, page);
        assert_eq!(count(&now, UNAVAILABLE.as_bytes()), 0);
        assert_eq!(
            count(
                &now,
                b"<a href=\"media/200001_5001.jpg\"><img src=\"media/200001_5001.jpg\""
            ),
            1
        );
    }

    #[test]
    fn gone_chat_attachment_shows_text_once() {
        let fx = setup("rw-chat-gone", false);
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Gone);
        assert!(rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap().changed >= 1);
        let first = read(&fx.root, &page_a());
        let anchor = format!("<a class='attachment__link' href='{}'", esc(URL_CHAT_1));
        assert_eq!(count(&fx.pristine[&page_a()], anchor.as_bytes()), 1);
        assert_eq!(count(&first, anchor.as_bytes()), 0);
        assert_eq!(count(&first, esc(URL_CHAT_1).as_bytes()), 0);
        let item = format!("</div>{UNAVAILABLE}</div>");
        assert_eq!(count(&first, item.as_bytes()), 1);
        assert_eq!(count(&first, UNAVAILABLE.as_bytes()), 1);
        assert_eq!(rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap().changed, 0);
        assert_eq!(read(&fx.root, &page_a()), first);
        undo(&fx.root).unwrap();
        assert_pristine(&fx);
    }

    #[test]
    fn ambiguous_unavailable_anchor_left_alone() {
        let url = esc(URL_CHAT_1);
        let gone = |u: &str| u == URL_CHAT_1;
        for orig in [
            format!("<a class='attachment__link' href='{url}'>x<a href='y'>z</a>\n"),
            format!("<a class='attachment__link' href='{url}'>x<b>y</b></a>\n"),
            format!("<a class='attachment__link' href='{url}'>x</div>\n"),
            format!("<a class='attachment__link' title=\"t href='{url}'>x</a>\n"),
        ] {
            let out = rewrite_page(
                orig.as_bytes(),
                "messages/1/messages0.html",
                &|_| None,
                &gone,
            );
            assert_eq!(out, orig.into_bytes());
        }
    }

    #[test]
    fn album_anchor_second_run_noop() {
        let fx = setup("rw-album-twice", false);
        let page = "photos/photo-albums/album-1.html";
        mark(&fx.root, &fx.ix, URL_ALBUM_1, Status::Saved);
        assert_eq!(rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap().changed, 1);
        let first = read(&fx.root, page);
        assert_eq!(count(&first, b"<a href=\"media/"), 1);
        assert_eq!(rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap().changed, 0);
        assert_eq!(read(&fx.root, page), first);
        assert_eq!(
            std::fs::read(backup(&fx.root, page)).unwrap(),
            fx.pristine[page]
        );
    }

    #[test]
    fn inline_idempotent() {
        let fx = setup("rw-inline-twice", false);
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        mark(&fx.root, &fx.ix, URL_CHAT_2, Status::Saved);
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        let first = read(&fx.root, &page_a());
        let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(r.changed, 0);
        assert_eq!(read(&fx.root, &page_a()), first);
        assert_eq!(count(&first, b"vk-saver-inline"), 2);
        assert_eq!(count(&read(&fx.root, &page_a50()), b"vk-saver-inline"), 1);
        assert_eq!(
            std::fs::read(backup(&fx.root, &page_a())).unwrap(),
            fx.pristine[&page_a()]
        );
    }

    #[test]
    fn malformed_anchor_no_insert() {
        let url = esc(URL_CHAT_1);
        let orig = format!(
            "<div class=\"attachment\"><a class='attachment__link' href='{url}'>x<a href='y'>z</a></div>\r\n"
        );
        let saved = |u: &str| (u == URL_CHAT_1).then(|| "messages/1/media/a.jpg".to_string());
        let out = rewrite_page(orig.as_bytes(), "messages/1/messages0.html", &saved, &none);
        assert_eq!(out, orig.replace(&url, "media/a.jpg").into_bytes());
    }

    #[test]
    fn hostile_index_path_not_written() {
        let fx = setup("rw-hostile", false);
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        mark(&fx.root, &fx.ix, URL_CHAT_2, Status::Saved);
        let c = rusqlite::Connection::open(fx.root.join("vk-saver").join("index.db")).unwrap();
        c.execute(
            "UPDATE media SET local_path = '../../evil.jpg' WHERE url = ?1",
            [URL_CHAT_1],
        )
        .unwrap();
        c.execute(
            "UPDATE media SET local_path = '/abs/evil.jpg' WHERE url = ?1",
            [URL_CHAT_2],
        )
        .unwrap();
        let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(r.changed, 0);
        for p in &fx.pages {
            assert_eq!(read(&fx.root, p), fx.pristine[p]);
        }
        assert!(!fx.root.join("vk-saver").join("originals").exists());
        assert!(!fx.root.parent().unwrap().join("evil.jpg").exists());
    }

    #[test]
    fn unsafe_page_name_skipped_and_counted() {
        let fx = setup("rw-skip", false);
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        let mut pages = fx.pages.clone();
        pages.push("../outside.html".into());
        let r = rewrite_all(&fx.root, &fx.ix, &pages).unwrap();
        assert_eq!((r.skipped, r.pages), (1, fx.pages.len() as u64));
    }

    fn rewritten(name: &str, crlf: bool) -> Fx {
        let fx = setup(name, crlf);
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        mark(&fx.root, &fx.ix, URL_CHAT_2, Status::Saved);
        let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(r.changed, 2);
        fx
    }

    fn assert_pristine(fx: &Fx) {
        for p in &fx.pages {
            assert_eq!(sha(&read(&fx.root, p)), sha(&fx.pristine[p]), "{p}");
        }
    }

    #[test]
    fn undo_restores_byte_identical() {
        for crlf in [false, true] {
            let fx = rewritten(&format!("undo-bytes-{crlf}"), crlf);
            let r = undo(&fx.root).unwrap();
            assert_eq!(
                r,
                UndoReport {
                    restored: 2,
                    busy: 0,
                    kept: 0,
                    skipped: 0
                }
            );
            assert_pristine(&fx);
        }
    }

    #[test]
    fn undo_keeps_media_and_backups() {
        let fx = rewritten("undo-keep", false);
        let saved = fx.ix.saved_paths().unwrap();
        undo(&fx.root).unwrap();
        for p in saved.values() {
            assert!(safe_join(&fx.root, p).unwrap().is_file(), "{p}");
        }
        for p in [page_a(), page_a50()] {
            let b = backup(&fx.root, &p);
            assert_eq!(std::fs::read(&b).unwrap(), fx.pristine[&p]);
            assert_eq!(lines(&with(&b, ".outputs")).len(), 1);
        }
    }

    #[test]
    fn undo_twice_is_noop() {
        let fx = rewritten("undo-twice", false);
        undo(&fx.root).unwrap();
        assert_eq!(undo(&fx.root).unwrap(), UndoReport::default());
        assert_pristine(&fx);
    }

    #[test]
    fn redo_after_undo_equals_first() {
        let fx = rewritten("undo-redo", false);
        let first: Vec<_> = fx.pages.iter().map(|p| read(&fx.root, p)).collect();
        undo(&fx.root).unwrap();
        let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(r.changed, 2);
        let again: Vec<_> = fx.pages.iter().map(|p| read(&fx.root, p)).collect();
        assert_eq!(again, first);
        assert!(!with(&backup(&fx.root, &page_a()), ".old-1").exists());
    }

    #[test]
    fn undo_leaves_changed_page() {
        let fx = rewritten("undo-changed", false);
        let page = page_a();
        let mut mine = read(&fx.root, &page);
        mine.extend_from_slice(b"<p>note</p>\n");
        std::fs::write(safe_join(&fx.root, &page).unwrap(), &mine).unwrap();
        let r = undo(&fx.root).unwrap();
        assert_eq!(
            r,
            UndoReport {
                restored: 1,
                busy: 0,
                kept: 1,
                skipped: 0
            }
        );
        assert_eq!(read(&fx.root, &page), mine);
        assert_eq!(read(&fx.root, &page_a50()), fx.pristine[&page_a50()]);
    }

    #[cfg(windows)]
    #[test]
    fn undo_skips_locked_page() {
        use std::os::windows::fs::OpenOptionsExt;
        // 0: the read fails; 1 (FILE_SHARE_READ): the read succeeds, the rename fails.
        for share in [0u32, 1] {
            let fx = rewritten(&format!("undo-locked-{share}"), false);
            let live = safe_join(&fx.root, &page_a()).unwrap();
            let ours = std::fs::read(&live).unwrap();
            let held = File::options()
                .read(true)
                .share_mode(share)
                .open(&live)
                .unwrap();
            let r = undo(&fx.root).unwrap();
            drop(held);
            assert_eq!(
                r,
                UndoReport {
                    restored: 1,
                    busy: 1,
                    kept: 0,
                    skipped: 0
                }
            );
            assert_eq!(std::fs::read(&live).unwrap(), ours);
            assert_eq!(read(&fx.root, &page_a50()), fx.pristine[&page_a50()]);
        }
    }

    #[test]
    fn sidecar_without_backup_skips_page() {
        let fx = rewritten("rw-orphan-side", false);
        let page = page_a();
        let b = backup(&fx.root, &page);
        std::fs::remove_file(&b).unwrap();
        let mut mine = fx.pristine[&page].clone();
        mine.extend_from_slice(b"<p>x</p>\n");
        std::fs::write(safe_join(&fx.root, &page).unwrap(), &mine).unwrap();
        let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!((r.skipped, r.changed), (1, 0));
        assert_eq!(read(&fx.root, &page), mine);
        assert!(!b.exists());
        assert_eq!(lines(&with(&b, ".outputs")).len(), 1);
    }

    #[test]
    fn page_changed_after_read_not_written() {
        let d = crate::testing::tmpdir("rw-changed-after-read");
        let p = d.join("page.html");
        std::fs::write(&p, b"newer").unwrap();
        assert_eq!(
            replace_if_unchanged(&p, b"old", b"out"),
            Err(ReplaceError::Busy)
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"newer");
        assert_eq!(replace_if_unchanged(&p, b"newer", b"out"), Ok(()));
        assert_eq!(std::fs::read(&p).unwrap(), b"out");
    }

    #[cfg(unix)]
    #[test]
    fn archive_skips_dangling_link_slot() {
        let d = crate::testing::tmpdir("rw-archive-link");
        let b = d.join("page.html");
        std::fs::write(&b, b"orig").unwrap();
        std::os::unix::fs::symlink(d.join("nowhere"), with(&b, ".old-1")).unwrap();
        archive(&b).unwrap();
        assert!(
            std::fs::symlink_metadata(with(&b, ".old-1"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(with(&b, ".old-2")).unwrap(), b"orig");
        assert!(!d.join("nowhere").exists());
    }

    fn newer(fx: &Fx, page: &str) -> Vec<u8> {
        let mut n = fx.pristine[page].clone();
        n.extend_from_slice(b"<p>n</p>\n");
        std::fs::write(safe_join(&fx.root, page).unwrap(), &n).unwrap();
        n
    }

    #[test]
    fn crash_inside_archive_between_renames() {
        let fx = rewritten("rw-crash-archive", false);
        let page = page_a();
        let b = backup(&fx.root, &page);
        std::fs::rename(with(&b, ".outputs"), with(&b, ".old-1.outputs")).unwrap();
        let n = newer(&fx, &page);
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(lines(&with(&b, ".old-1.outputs")).len(), 1);
        assert!(!with(&b, ".old-1").exists());
        assert_eq!(
            std::fs::read(with(&b, ".old-2")).unwrap(),
            fx.pristine[&page]
        );
        assert_eq!(std::fs::read(&b).unwrap(), n);
        assert_eq!(
            read(&fx.root, &page),
            rewrite_page(&n, &page, &lookup(&fx.ix), &none)
        );
    }

    #[test]
    fn crash_after_archive_before_new_backup() {
        let fx = rewritten("rw-crash-after-archive", false);
        let page = page_a();
        let b = backup(&fx.root, &page);
        std::fs::rename(with(&b, ".outputs"), with(&b, ".old-1.outputs")).unwrap();
        std::fs::rename(&b, with(&b, ".old-1")).unwrap();
        let n = newer(&fx, &page);
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(
            std::fs::read(with(&b, ".old-1")).unwrap(),
            fx.pristine[&page]
        );
        assert_eq!(std::fs::read(&b).unwrap(), n);
        assert_eq!(
            read(&fx.root, &page),
            rewrite_page(&n, &page, &lookup(&fx.ix), &none)
        );
        assert_eq!(lines(&with(&b, ".outputs")).len(), 1);
    }

    #[test]
    fn unsafe_index_path_dropped_by_sync() {
        let fx = setup("ix-unsafe-path", false);
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        let c = rusqlite::Connection::open(fx.root.join("vk-saver").join("index.db")).unwrap();
        c.execute(
            "UPDATE media SET local_path = '../../evil.jpg' WHERE url = ?1",
            [URL_CHAT_1],
        )
        .unwrap();
        c.execute(
            "UPDATE media SET local_path = NULL WHERE url = ?1",
            [URL_CHAT_2],
        )
        .unwrap();
        let s = scan(&fx.root, &AtomicBool::new(false)).unwrap();
        fx.ix.sync(&s.found).unwrap();
        assert_eq!(fx.ix.reset_missing(&fx.root).unwrap(), 0);
        let rows = fx.ix.rows(&[Status::New]).unwrap();
        for url in [URL_CHAT_1, URL_CHAT_2] {
            let r = rows.iter().find(|r| r.url == url).expect(url);
            assert!(safe_join(&fx.root, &r.local_path).is_ok(), "{url}");
        }
    }

    #[test]
    fn archive_never_renames_over_old_slot() {
        let fx = rewritten("rw-old-slot", false);
        let page = page_a();
        let b = backup(&fx.root, &page);
        std::fs::write(with(&b, ".old-1.outputs"), b"stray\n").unwrap();
        std::fs::write(with(&b, ".old-2"), b"stray").unwrap();
        let mut newer = fx.pristine[&page].clone();
        newer.extend_from_slice(b"<p>n</p>\n");
        std::fs::write(safe_join(&fx.root, &page).unwrap(), &newer).unwrap();
        rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(
            std::fs::read(with(&b, ".old-1.outputs")).unwrap(),
            b"stray\n"
        );
        assert_eq!(std::fs::read(with(&b, ".old-2")).unwrap(), b"stray");
        assert_eq!(
            std::fs::read(with(&b, ".old-3")).unwrap(),
            fx.pristine[&page]
        );
        assert_eq!(lines(&with(&b, ".old-3.outputs")).len(), 1);
        assert_eq!(std::fs::read(&b).unwrap(), newer);
    }

    #[test]
    fn missing_saved_file_keeps_link() {
        let fx = setup("rw-gone-file", false);
        mark(&fx.root, &fx.ix, URL_CHAT_1, Status::Saved);
        let f = safe_join(&fx.root, &fx.ix.saved_paths().unwrap()[URL_CHAT_1]).unwrap();
        std::fs::remove_file(f).unwrap();
        let r = rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap();
        assert_eq!(r.changed, 0);
        assert_pristine(&fx);
    }

    #[test]
    fn one_inline_per_anchor_with_two_hrefs() {
        let url = esc(URL_CHAT_1);
        let orig = format!("<a class='attachment__link' href='{url}' href='{url}'>x</a>\n");
        let saved = |u: &str| (u == URL_CHAT_1).then(|| "messages/1/media/a.jpg".to_string());
        let out = rewrite_page(orig.as_bytes(), "messages/1/messages0.html", &saved, &none);
        assert_eq!(count(&out, b"vk-saver-inline"), 1);
    }

    #[test]
    fn uppercase_close_gets_inline() {
        let url = esc(URL_CHAT_1);
        let orig = format!("<A class='attachment__link' href='{url}'>x</A>\n");
        let saved = |u: &str| (u == URL_CHAT_1).then(|| "messages/1/media/a.jpg".to_string());
        let out = rewrite_page(orig.as_bytes(), "messages/1/messages0.html", &saved, &none);
        let expect = format!(
            "<A class='attachment__link' href='media/a.jpg'>x</A>{}\n",
            inline("media/a.jpg")
        );
        assert_eq!(out, expect.into_bytes());
    }

    #[test]
    fn torn_and_garbage_sidecar_lines() {
        for tail in ["\n0123abc", "\ngarbage\n\x00\x7f\nzz\n"] {
            let fx = rewritten(&format!("rw-torn-{}", tail.len()), false);
            let b = backup(&fx.root, &page_a());
            let ours = read(&fx.root, &page_a());
            let mut side = std::fs::read(with(&b, ".outputs")).unwrap();
            side.extend_from_slice(tail.as_bytes());
            std::fs::write(with(&b, ".outputs"), &side).unwrap();
            assert_eq!(rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap().changed, 0);
            assert_eq!(read(&fx.root, &page_a()), ours);
            mark(&fx.root, &fx.ix, URL_CHAT_3, Status::Saved);
            assert_eq!(rewrite_all(&fx.root, &fx.ix, &fx.pages).unwrap().changed, 1);
            let now = sha(&read(&fx.root, &page_a()));
            assert!(lines(&with(&b, ".outputs")).contains(&now));
            assert!(!with(&b, ".old-1").exists());
            assert_eq!(std::fs::read(&b).unwrap(), fx.pristine[&page_a()]);
        }
    }

    #[test]
    fn growing_saved_set_equals_fresh_rewrite() {
        let grown = setup("rw-grow", false);
        mark(&grown.root, &grown.ix, URL_CHAT_1, Status::Saved);
        rewrite_all(&grown.root, &grown.ix, &grown.pages).unwrap();
        mark(&grown.root, &grown.ix, URL_CHAT_2, Status::Saved);
        rewrite_all(&grown.root, &grown.ix, &grown.pages).unwrap();
        let fresh = rewritten("rw-grow-fresh", false);
        for p in &grown.pages {
            assert_eq!(read(&grown.root, p), read(&fresh.root, p), "{p}");
        }
    }

    #[test]
    fn oversized_page_skipped_by_rewrite_and_undo() {
        let fx = rewritten("rw-big", false);
        let big = "wall/wall9.html";
        File::create(safe_join(&fx.root, big).unwrap())
            .unwrap()
            .set_len(crate::export::MAX_PAGE + 1)
            .unwrap();
        let mut pages = fx.pages.clone();
        pages.push(big.into());
        let r = rewrite_all(&fx.root, &fx.ix, &pages).unwrap();
        assert_eq!((r.skipped, r.changed), (1, 0));
        let u = undo(&fx.root).unwrap();
        assert_eq!((u.skipped, u.restored), (0, 2));
    }

    #[test]
    fn oversized_page_counted_by_undo_only_when_backed_up() {
        let fx = rewritten("rw-big-undo", false);
        File::create(safe_join(&fx.root, &page_a()).unwrap())
            .unwrap()
            .set_len(crate::export::MAX_PAGE + 1)
            .unwrap();
        File::create(safe_join(&fx.root, "wall/wall9.html").unwrap())
            .unwrap()
            .set_len(crate::export::MAX_PAGE + 1)
            .unwrap();
        let u = undo(&fx.root).unwrap();
        assert_eq!((u.skipped, u.restored, u.kept), (1, 1, 0));
    }

    #[test]
    fn hostile_repeated_url_page_rewrites_in_linear_time() {
        let url = "https://sun9-5.userapi.com/impg/x/c1.jpg";
        let saved = |u: &str| (u == url).then(|| "messages/1/media/1_1.jpg".to_string());
        let attr = format!(" href='{url}'");
        let fill = |head: &str| {
            let mut p = head.as_bytes().to_vec();
            while p.len() < 2 << 20 {
                p.extend_from_slice(attr.as_bytes());
            }
            p
        };
        let open = fill("<x");
        let mut anchor = fill("<a class=\"attachment__link\"");
        anchor.extend_from_slice(b">t</a>");
        let n = open.len() / attr.len();
        let start = std::time::Instant::now();
        let out = rewrite_page(&open, "messages/1/messages0.html", &saved, &none);
        let out_anchor = rewrite_page(&anchor, "messages/1/messages0.html", &saved, &none);
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        let count = |b: &[u8], pat: &[u8]| b.windows(pat.len()).filter(|w| *w == pat).count();
        assert_eq!(count(&out, b"href='media/1_1.jpg'"), n);
        assert_eq!(count(&out, b"vk-saver-inline"), 0);
        assert_eq!(count(&out_anchor, b"vk-saver-inline"), 1);
        assert!(out_anchor.ends_with(b"</a><a href=\"media/1_1.jpg\"><img class=\"vk-saver-inline\" src=\"media/1_1.jpg\" loading=\"lazy\" alt=\"\" style=\"display:block;max-width:320px;max-height:320px\"></a>"));
    }
}
