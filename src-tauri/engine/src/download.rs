use crate::cdn::{Cdn, NetError};
use crate::export::{ReplaceError, replace_file};
use crate::progress::{Event, Progress};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const MAX_FILE: u64 = 100 << 20;
pub const MIN_FREE: u64 = 1 << 30;

pub trait FreeSpace {
    fn free_bytes(&self, root: &Path) -> io::Result<u64>;
}

pub struct DiskSpace;

impl FreeSpace for DiskSpace {
    fn free_bytes(&self, root: &Path) -> io::Result<u64> {
        // Volume-wide free bytes, not the quota-limited figure (GetDiskFreeSpaceExW / statvfs f_bfree).
        fs4::free_space(root)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Jpg,
    Png,
    Gif,
    Webp,
    Heic,
    Ogg,
}

impl Media {
    pub fn ext(self) -> &'static str {
        match self {
            Media::Jpg => "jpg",
            Media::Png => "png",
            Media::Gif => "gif",
            Media::Webp => "webp",
            Media::Heic => "heic",
            Media::Ogg => "ogg",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Media::Jpg => "image/jpeg",
            Media::Png => "image/png",
            Media::Gif => "image/gif",
            Media::Webp => "image/webp",
            Media::Heic => "image/heic",
            Media::Ogg => "audio/ogg",
        }
    }
}

/// Any other `ftyp` brand is an MP4-family video, not a photo.
const HEIC_BRANDS: [&[u8]; 7] = [
    b"heic", b"heix", b"hevc", b"heim", b"heis", b"mif1", b"msf1",
];

pub fn sniff(head: &[u8]) -> Option<Media> {
    if head.starts_with(b"\xff\xd8\xff") && head.len() > 3 {
        Some(Media::Jpg)
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Media::Png)
    } else if head.starts_with(b"GIF8") {
        Some(Media::Gif)
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        Some(Media::Webp)
    } else if head.starts_with(b"OggS") {
        Some(Media::Ogg)
    } else if head.get(4..8) == Some(b"ftyp")
        && head.get(8..12).is_some_and(|b| HEIC_BRANDS.contains(&b))
    {
        Some(Media::Heic)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DlError {
    Http(u16),
    Net(NetError),
    Io(String),
    NoSpace(u64),
    TooLarge,
    NotMedia,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub path: PathBuf,
    pub bytes: u64,
    pub media: Media,
    pub sha256: String,
}

/// Streams into `<stem>.part` (kept for resume on network and read errors) and saves `<stem>.<ext>`.
pub fn download(
    cdn: &dyn Cdn,
    space: &dyn FreeSpace,
    url: &str,
    stem: &Path,
    root: &Path,
    cancel: &AtomicBool,
    progress: &dyn Progress,
) -> Result<Saved, DlError> {
    let free = space.free_bytes(root).map_err(io_err)?;
    if free < MIN_FREE {
        return Err(DlError::NoSpace(free));
    }
    if let Some(dir) = stem.parent() {
        fs::create_dir_all(dir).map_err(io_err)?;
    }
    let part = with_suffix(stem, "part");
    let mut restarted = false;
    loop {
        let have = fs::metadata(&part).map_or(0, |m| m.len());
        let resp = cdn.get(url, have).map_err(DlError::Net)?;
        let (append, expected) = match resp.status {
            206 if resp.start == Some(have) || (have == 0 && resp.start.is_none()) => {
                (true, resp.total.or(resp.content_length.map(|n| have + n)))
            }
            200 => (false, resp.content_length),
            // 416: a complete .part left by a crash before the rename; 206: a range that
            // does not start where the part ends would leave a hole.
            206 | 416 if have > 0 && !restarted => {
                fs::remove_file(&part).map_err(io_err)?;
                restarted = true;
                continue;
            }
            status => return Err(DlError::Http(status)),
        };
        if expected.is_some_and(|n| n > MAX_FILE) {
            return Err(drop_part(&part, DlError::TooLarge));
        }
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(&part)
            .map_err(io_err)?;
        let start = if append { have } else { 0 };
        let mut body = resp.body.take(MAX_FILE + 1 - start.min(MAX_FILE));
        let copied = copy_chunks(&mut body, &mut file, cancel, progress);
        let synced = file.sync_all().map_err(io_err);
        drop(file);
        copied?;
        synced?;
        let got = fs::metadata(&part).map_err(io_err)?.len();
        if got > MAX_FILE {
            return Err(drop_part(&part, DlError::TooLarge));
        }
        if let Some(expected) = expected.filter(|&e| e != got) {
            return Err(DlError::Io(format!(
                "short read: {got} of {expected} bytes"
            )));
        }
        let mut head = Vec::with_capacity(16);
        File::open(&part)
            .and_then(|f| f.take(16).read_to_end(&mut head))
            .map_err(io_err)?;
        let Some(media) = sniff(&head) else {
            return Err(drop_part(&part, DlError::NotMedia));
        };
        let path = with_suffix(stem, media.ext());
        replace_file(&part, &path).map_err(|e| match e {
            ReplaceError::Busy => DlError::Io("file busy".into()),
            ReplaceError::Io(_) => DlError::Io("replace failed".into()),
        })?;
        let sha256 = sha256_file(&path)?;
        return Ok(Saved {
            path,
            bytes: got,
            media,
            sha256,
        });
    }
}

/// Removal failure is ignored: `err` is the outcome the caller reports.
fn drop_part(part: &Path, err: DlError) -> DlError {
    let _ = fs::remove_file(part);
    err
}

pub(crate) fn sha256_file(path: &Path) -> Result<String, DlError> {
    let mut f = File::open(path).map_err(io_err)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK as usize];
    loop {
        let n = f.read(&mut buf).map_err(io_err)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

const CHUNK: u64 = 1 << 20;

/// Bytes read before an error are written, so the part keeps them.
fn copy_chunks(
    body: &mut dyn Read,
    file: &mut File,
    cancel: &AtomicBool,
    progress: &dyn Progress,
) -> Result<(), DlError> {
    let mut buf = Vec::with_capacity(CHUNK as usize);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(DlError::Cancelled);
        }
        buf.clear();
        let read = body.take(CHUNK).read_to_end(&mut buf);
        file.write_all(&buf).map_err(io_err)?;
        if !buf.is_empty() {
            progress.event(Event::Bytes(buf.len() as u64));
        }
        match read {
            Err(e) => return Err(io_err(e)),
            Ok(n) if (n as u64) < CHUNK => return Ok(()),
            Ok(_) => {}
        }
    }
}

pub(crate) fn with_suffix(stem: &Path, ext: &str) -> PathBuf {
    let mut p = stem.as_os_str().to_owned();
    p.push(".");
    p.push(ext);
    PathBuf::from(p)
}

/// Kind and OS code only: io texts can carry paths and urls, and error values reach diagnostics.
fn io_err(e: io::Error) -> DlError {
    DlError::Io(match e.raw_os_error() {
        Some(code) => format!("{:?} os {code}", e.kind()),
        None => format!("{:?}", e.kind()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdn::{CdnResponse, NetError};
    use crate::testing::{FakeCdn, FakeFile, FakeSpace, NoProgress, tmpdir};

    static NO_CANCEL: AtomicBool = AtomicBool::new(false);

    const URL: &str = "https://sun9-1.userapi.com/impg/p1.jpg";

    /// A jpg-looking body of `len` bytes.
    fn jpg(len: usize) -> Vec<u8> {
        let mut b: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        b[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        b
    }

    fn body() -> Vec<u8> {
        jpg(1000)
    }

    /// (root, stem, part, saved jpg)
    fn paths(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let root = tmpdir(name);
        let dir = root.join("albums").join("Trip (10)");
        (
            root,
            dir.join("p1"),
            dir.join("p1.part"),
            dir.join("p1.jpg"),
        )
    }

    fn write_part(part: &Path, bytes: &[u8]) {
        fs::create_dir_all(part.parent().unwrap()).unwrap();
        fs::write(part, bytes).unwrap();
    }

    fn ok_space() -> FakeSpace {
        FakeSpace(Ok(MIN_FREE))
    }

    fn run(cdn: &dyn Cdn, stem: &Path, root: &Path) -> Result<u64, DlError> {
        download(cdn, &ok_space(), URL, stem, root, &NO_CANCEL, &NoProgress).map(|s| s.bytes)
    }

    #[test]
    fn min_free_is_one_gib() {
        assert_eq!(MIN_FREE, 1 << 30);
        let (root, stem, _, dest) = paths("dl-boundary");
        let (cdn, log) = FakeCdn::new(vec![(URL, vec![FakeFile::ok(&body())])]);
        assert_eq!(run(&cdn, &stem, &root), Ok(1000));

        let (root, stem, _, dest2) = paths("dl-boundary-low");
        let low = FakeSpace(Ok((1 << 30) - 1));
        assert_eq!(
            download(&cdn, &low, URL, &stem, &root, &NO_CANCEL, &NoProgress),
            Err(DlError::NoSpace((1 << 30) - 1))
        );
        assert_eq!(log.gets().len(), 1);
        assert!(dest.exists() && !dest2.exists());
    }

    #[test]
    fn disk_space_real_smoke() {
        assert!(DiskSpace.free_bytes(&tmpdir("dl-real-space")).unwrap() > 0);
    }

    #[test]
    fn sniff_table() {
        let ftyp = |brand: &[u8]| [b"\0\0\0\x18ftyp".as_slice(), brand, b"\0\0\0\0"].concat();
        for (head, media) in [
            (b"\xff\xd8\xff\xe0rest".to_vec(), Media::Jpg),
            (b"\x89PNG\r\n\x1a\nrest".to_vec(), Media::Png),
            (b"GIF89a".to_vec(), Media::Gif),
            (b"RIFF\0\0\0\0WEBPVP8 ".to_vec(), Media::Webp),
            (b"OggS\0".to_vec(), Media::Ogg),
            (ftyp(b"heic"), Media::Heic),
            (ftyp(b"mif1"), Media::Heic),
        ] {
            assert_eq!(sniff(&head), Some(media), "{head:?}");
        }
        for head in [
            ftyp(b"isom"),
            ftyp(b"mp42"),
            b"<html>".to_vec(),
            Vec::new(),
            b"\xff\xd8\xff".to_vec(),
        ] {
            assert_eq!(sniff(&head), None, "{head:?}");
        }
        assert_eq!(sniff(b"\xff\xd8\xff\xdb"), Some(Media::Jpg));
        assert_eq!(
            (Media::Heic.ext(), Media::Heic.mime()),
            ("heic", "image/heic")
        );
        assert_eq!((Media::Ogg.ext(), Media::Ogg.mime()), ("ogg", "audio/ogg"));
    }

    #[test]
    fn saved_has_ext_and_sha() {
        let (root, stem, part, _) = paths("dl-png");
        let png = [b"\x89PNG\r\n\x1a\n".as_slice(), b"body"].concat();
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![FakeFile::ok(&png)])]);
        let saved = download(
            &cdn,
            &ok_space(),
            URL,
            &stem,
            &root,
            &NO_CANCEL,
            &NoProgress,
        )
        .unwrap();
        let path = stem.with_file_name("p1.png");
        assert_eq!(
            saved,
            Saved {
                path: path.clone(),
                bytes: png.len() as u64,
                media: Media::Png,
                // sha256 of the 12 png bytes above, from `shasum -a 256`.
                sha256: "776f30dcc133299a75c28d425a30a09788b4a3ca62ce3349d27815888ce5262f".into(),
            }
        );
        assert_eq!(fs::read(&path).unwrap(), png);
        assert!(!part.exists());
    }

    #[test]
    fn content_length_over_cap() {
        let (root, stem, part, _) = paths("dl-over-cap");
        let f = FakeFile {
            content_length: Some(MAX_FILE + 1),
            ..FakeFile::ok(&body())
        };
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![f])]);
        assert_eq!(run(&cdn, &stem, &root), Err(DlError::TooLarge));
        assert!(!part.exists());

        write_part(&part, &body()[..100]);
        let total = FakeFile::ok(&jpg(MAX_FILE as usize + 1));
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![total])]);
        assert_eq!(run(&cdn, &stem, &root), Err(DlError::TooLarge));
        assert!(!part.exists());
    }

    struct Endless;

    impl Cdn for Endless {
        fn get(&self, _url: &str, _from: u64) -> Result<CdnResponse, NetError> {
            Ok(CdnResponse {
                status: 200,
                content_length: None,
                start: None,
                total: None,
                body: Box::new(io::Cursor::new(body()).chain(io::repeat(7).take(MAX_FILE))),
            })
        }
    }

    #[test]
    fn streamed_over_cap() {
        let (root, stem, part, dest) = paths("dl-streamed-cap");
        assert_eq!(run(&Endless, &stem, &root), Err(DlError::TooLarge));
        assert!(!part.exists() && !dest.exists());
    }

    #[test]
    fn html_200_is_not_media() {
        let (root, stem, part, _) = paths("dl-html");
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![FakeFile::ok(b"<!DOCTYPE html>")])]);
        assert_eq!(run(&cdn, &stem, &root), Err(DlError::NotMedia));
        assert!(!part.exists());
        assert_eq!(fs::read_dir(part.parent().unwrap()).unwrap().count(), 0);
    }

    #[test]
    fn resume_keeps_stem_part() {
        let (root, stem, part, dest) = paths("dl-resume-mib");
        let full = jpg(3 << 20);
        let broken = FakeFile {
            content_length: Some(3 << 20),
            broken: true,
            ..FakeFile::ok(&full[..1 << 20])
        };
        let (cdn, log) = FakeCdn::new(vec![(URL, vec![broken, FakeFile::ok(&full)])]);
        assert!(matches!(run(&cdn, &stem, &root), Err(DlError::Io(_))));
        assert_eq!(fs::metadata(&part).unwrap().len(), 1 << 20);
        assert!(!dest.exists());

        let saved = download(
            &cdn,
            &ok_space(),
            URL,
            &stem,
            &root,
            &NO_CANCEL,
            &NoProgress,
        )
        .unwrap();
        assert_eq!(log.gets()[1], (URL.to_string(), 1048576));
        assert_eq!((saved.media, saved.bytes), (Media::Jpg, 3 << 20));
        assert_eq!(saved.path, dest);
        assert_eq!(fs::read(&dest).unwrap(), full);
        assert!(!part.exists());
    }

    #[test]
    fn net_error_keeps_part() {
        let (root, stem, part, _) = paths("dl-net");
        write_part(&part, &body()[..100]);
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![FakeFile::net(NetError::Timeout)])]);
        assert_eq!(
            run(&cdn, &stem, &root),
            Err(DlError::Net(NetError::Timeout))
        );
        assert_eq!(fs::read(&part).unwrap(), &body()[..100]);
    }

    #[test]
    fn download_short_read_keeps_part() {
        let (root, stem, part, dest) = paths("dl-short");
        let short = FakeFile {
            content_length: Some(1000),
            ..FakeFile::ok(&body()[..100])
        };
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![short])]);
        assert_eq!(
            run(&cdn, &stem, &root),
            Err(DlError::Io("short read: 100 of 1000 bytes".into()))
        );
        assert!(!dest.exists());
        assert_eq!(fs::read(&part).unwrap(), &body()[..100]);
    }

    #[test]
    fn download_resumes_with_range() {
        let (root, stem, part, dest) = paths("dl-resume");
        write_part(&part, &body()[..100]);
        let (cdn, log) = FakeCdn::new(vec![(URL, vec![FakeFile::ok(&body())])]);
        assert_eq!(run(&cdn, &stem, &root), Ok(1000));
        assert_eq!(log.gets(), vec![(URL.to_string(), 100)]);
        assert_eq!(fs::read(&dest).unwrap(), body());
        assert!(!part.exists());
    }

    #[test]
    fn download_restarts_when_range_ignored() {
        let (root, stem, part, dest) = paths("dl-range-ignored");
        write_part(&part, &body()[..100]);
        let full = FakeFile {
            range: false,
            ..FakeFile::ok(&body())
        };
        let (cdn, log) = FakeCdn::new(vec![(URL, vec![full])]);
        assert_eq!(run(&cdn, &stem, &root), Ok(1000));
        assert_eq!(log.gets(), vec![(URL.to_string(), 100)]);
        assert_eq!(fs::read(&dest).unwrap(), body());
        assert!(!part.exists());
    }

    #[test]
    fn download_416_restarts() {
        let (root, stem, part, dest) = paths("dl-416");
        write_part(&part, &body());
        let (cdn, log) = FakeCdn::new(vec![(URL, vec![FakeFile::ok(&body())])]);
        assert_eq!(run(&cdn, &stem, &root), Ok(1000));
        assert_eq!(
            log.gets(),
            vec![(URL.to_string(), 1000), (URL.to_string(), 0)]
        );
        assert_eq!(fs::read(&dest).unwrap(), body());
        assert!(!part.exists());
    }

    #[test]
    fn download_416_on_empty_part_is_http() {
        let (root, stem, part, dest) = paths("dl-416-fresh");
        let (cdn, log) = FakeCdn::new(vec![(URL, vec![FakeFile::status(416)])]);
        assert_eq!(run(&cdn, &stem, &root), Err(DlError::Http(416)));
        assert_eq!(log.gets().len(), 1);
        assert!(!dest.exists() && !part.exists());
    }

    #[test]
    fn download_206_without_content_range() {
        let (root, stem, part, dest) = paths("dl-206-no-range");
        write_part(&part, &body()[..100]);
        let f = FakeFile {
            content_range: false,
            ..FakeFile::ok(&body())
        };
        let (cdn, log) = FakeCdn::new(vec![(URL, vec![f])]);
        assert_eq!(run(&cdn, &stem, &root), Ok(1000));
        assert_eq!(
            log.gets(),
            vec![(URL.to_string(), 100), (URL.to_string(), 0)]
        );
        assert_eq!(fs::read(&dest).unwrap(), body());
        assert!(!part.exists());
    }

    #[test]
    fn download_206_wrong_start_restarts() {
        let (root, stem, part, dest) = paths("dl-206-wrong-start");
        write_part(&part, &body()[..100]);
        let f = FakeFile {
            range_start: Some(200),
            ..FakeFile::ok(&body())
        };
        let (cdn, log) = FakeCdn::new(vec![(URL, vec![f])]);
        assert_eq!(run(&cdn, &stem, &root), Ok(1000));
        assert_eq!(
            log.gets(),
            vec![(URL.to_string(), 100), (URL.to_string(), 0)]
        );
        assert_eq!(fs::read(&dest).unwrap(), body());
        assert!(!part.exists());
    }

    #[test]
    fn download_http_error_keeps_part() {
        let (root, stem, part, dest) = paths("dl-http-keeps");
        write_part(&part, &body()[..100]);
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![FakeFile::status(504)])]);
        assert_eq!(run(&cdn, &stem, &root), Err(DlError::Http(504)));
        assert_eq!(fs::read(&part).unwrap(), &body()[..100]);
        assert!(!dest.exists());
    }

    struct NoLengthBroken;

    impl Cdn for NoLengthBroken {
        fn get(&self, _url: &str, _from: u64) -> Result<CdnResponse, NetError> {
            struct Reset;
            impl Read for Reset {
                fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                    Err(io::Error::other("connection reset"))
                }
            }
            Ok(CdnResponse {
                status: 200,
                content_length: None,
                start: None,
                total: None,
                body: Box::new(io::Cursor::new(body()).chain(Reset)),
            })
        }
    }

    #[test]
    fn no_length_read_error_not_renamed() {
        let (root, stem, part, dest) = paths("dl-no-length-broken");
        let r = run(&NoLengthBroken, &stem, &root);
        assert!(matches!(&r, Err(DlError::Io(m)) if m == "Other"), "{r:?}");
        assert!(!dest.exists());
        assert_eq!(fs::read(&part).unwrap(), body());
    }

    #[test]
    fn failed_download_keeps_existing_dest() {
        let (root, stem, _, dest) = paths("dl-keeps-dest");
        write_part(&dest, b"good file");
        let short = FakeFile {
            content_length: Some(1000),
            ..FakeFile::ok(&body()[..10])
        };
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![short])]);
        assert!(run(&cdn, &stem, &root).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"good file");
    }

    /// Records `Bytes` events; sets `cancel` on the first one when given.
    struct Chunks<'a>(std::cell::RefCell<Vec<u64>>, Option<&'a AtomicBool>);

    impl Progress for Chunks<'_> {
        fn event(&self, e: Event) {
            let Event::Bytes(n) = e else { return };
            self.0.borrow_mut().push(n);
            if let Some(c) = self.1 {
                c.store(true, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn io_error_text_has_no_path_or_url() {
        let e = io::Error::other("https://sun9-1.userapi.com/x.jpg?k=1 C:\\Users\\me\\p");
        assert_eq!(io_err(e), DlError::Io("Other".into()));
        let e = io::Error::from_raw_os_error(2);
        assert_eq!(io_err(e), DlError::Io("NotFound os 2".into()));
    }

    #[test]
    fn download_reports_bytes() {
        let (root, stem, _, dest) = paths("dl-bytes");
        let big = jpg(5 << 19);
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![FakeFile::ok(&big)])]);
        let rec = Chunks(Default::default(), None);
        let saved = download(&cdn, &ok_space(), URL, &stem, &root, &NO_CANCEL, &rec).unwrap();
        assert_eq!(saved.bytes, big.len() as u64);
        assert_eq!(*rec.0.borrow(), vec![1 << 20, 1 << 20, 1 << 19]);
        assert_eq!(fs::read(&dest).unwrap(), big);
    }

    #[test]
    fn download_cancel_keeps_part() {
        let (root, stem, part, dest) = paths("dl-cancel");
        let big = jpg(3 << 20);
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![FakeFile::ok(&big)])]);
        let cancel = AtomicBool::new(false);
        let rec = Chunks(Default::default(), Some(&cancel));
        assert_eq!(
            download(&cdn, &ok_space(), URL, &stem, &root, &cancel, &rec),
            Err(DlError::Cancelled)
        );
        assert_eq!(fs::metadata(&part).unwrap().len(), 1 << 20);
        assert!(!dest.exists());
    }

    #[test]
    fn empty_body_is_not_media() {
        let (root, stem, part, dest) = paths("dl-empty");
        let (cdn, _) = FakeCdn::new(vec![(URL, vec![FakeFile::ok(b"")])]);
        assert_eq!(run(&cdn, &stem, &root), Err(DlError::NotMedia));
        assert!(!dest.exists() && !part.exists());
    }
}
