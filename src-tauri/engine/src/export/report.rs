use super::index::{Index, Status};
use super::scan::Scan;
use super::session::ResultView;
use super::text::escape_html;
use super::{ReplaceError, replace_file};
use crate::error::Stop;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

const ALL: [Status; 6] = [
    Status::New,
    Status::Saved,
    Status::Gone,
    Status::Failed,
    Status::HostDead,
    Status::NotMedia,
];

const HOST_LABELS: [&str; 5] = ["sun#-#", "psv#-#", "psv#", "pp", "vk"];
const HOST_TAILS: [&str; 3] = ["userapi.com", "vk.com", "vk.ru"];
const SEGMENTS: [&str; 5] = ["impg", "impf", "c#", "s", "v#"];

/// `host/first-segment` with every digit run as `#`; anything outside the known shapes becomes `*`
/// (the file may be posted publicly, so unknown hosts and path words must not pass).
pub fn host_pattern(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let mut parts = rest.split('/');
    let host = mask(parts.next().unwrap_or_default());
    let seg = mask(parts.next().unwrap_or_default());
    let known = host
        .split_once('.')
        .is_some_and(|(l, t)| HOST_LABELS.contains(&l) && HOST_TAILS.contains(&t));
    let host = if known { host } else { "*".to_string() };
    let seg = if SEGMENTS.contains(&seg.as_str()) {
        seg
    } else {
        "*".to_string()
    };
    format!("{host}/{seg}")
}

fn mask(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('#') {
            out.push('#');
        }
    }
    out
}

fn storage(e: impl std::fmt::Display) -> Stop {
    Stop::Storage(e.to_string())
}

fn mib(bytes: u64) -> String {
    format!("{:.1} МБ", bytes as f64 / 1_048_576.0)
}

/// `vk-saver/report.html`: static UTF-8, no script, no external resource; links stay inside the export.
pub fn write_report(state: &Path, view: &ResultView) -> Result<(), Stop> {
    let mut body = format!(
        "<h1>Отчёт vk-saver</h1>\n<ul>\n<li>Скачано файлов: {} ({})</li>\n<li>Удалены во ВКонтакте: {}</li>\n<li>Сервер не отвечает: {}</li>\n<li>Не удалось скачать: {}</li>\n</ul>\n",
        view.saved,
        mib(view.bytes),
        view.gone,
        view.host_dead,
        view.failed
    );
    let linked: Vec<_> = view
        .chats
        .iter()
        .filter(|c| c.videos + c.docs + c.photo_links > 0)
        .collect();
    if !linked.is_empty() {
        body.push_str("<h2>Остались ссылками</h2>\n<ul>\n");
        for c in linked {
            body.push_str(&format!(
                "<li><a href=\"../messages/{}/messages0.html\">{}</a>: видео {}, документы {}, фото-ссылки {} – только ссылки</li>\n",
                c.peer,
                escape_html(&c.label),
                c.videos,
                c.docs,
                c.photo_links
            ));
        }
        body.push_str("</ul>\n");
    }
    let html = format!(
        "<!DOCTYPE html>\n<html lang=\"ru\">\n<head>\n<meta charset=\"utf-8\">\n<title>Отчёт vk-saver</title>\n</head>\n<body>\n{body}</body>\n</html>\n"
    );
    let dest = state.join("report.html");
    let tmp = state.join("report.html.tmp");
    let r = super::create_fresh(&tmp)
        .and_then(|mut f| f.write_all(html.as_bytes()))
        .map_err(storage)
        .and_then(|()| {
            replace_file(&tmp, &dest).map_err(|e| match e {
                ReplaceError::Busy => Stop::Storage("report.html is busy".into()),
                ReplaceError::Io(e) => Stop::Storage(e),
            })
        });
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Counts, status codes and host patterns only: no url, path, id, label or date (may go to a public tracker).
pub fn diagnostics(index: &Index, scan: &Scan, version: &str) -> Result<Value, Stop> {
    let mut counts: BTreeMap<&str, Map<String, Value>> = BTreeMap::new();
    for ((section, status), n) in index.counts()? {
        counts
            .entry(section.as_str())
            .or_default()
            .insert(status.as_str().into(), n.into());
    }
    let mut http: BTreeMap<String, u64> = BTreeMap::new();
    let mut hosts: BTreeMap<String, u64> = BTreeMap::new();
    let mut attempts = 0u64;
    for r in index.rows(&ALL)? {
        if let Some(code) = r.http {
            *http.entry(code.to_string()).or_default() += 1;
        }
        *hosts.entry(host_pattern(&r.url)).or_default() += 1;
        attempts += u64::from(r.attempts);
    }
    let d = &scan.drift;
    Ok(json!({
        "app": version,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "pages": scan.pages.len(),
        "chats": scan.chats.len(),
        "counts": counts,
        "http": http,
        "hosts": hosts,
        "attempts": attempts,
        "drift": { "unmatched": d.unmatched, "refused": d.refused, "bad_dates": d.bad_dates },
    }))
}

#[cfg(test)]
mod tests {
    use super::super::scan::{ChatGroup, Group};
    use super::super::session::{RunReport, Session, StartOptions};
    use super::super::text::encode_cp1251;
    use super::*;
    use crate::testing::{
        FakeCdn, FakeClock, FakeExportOpts, FakeFile, FakeSpace, NoProgress, OWNER, PEER_A, PEER_B,
        PEER_CLUB, PEER_USER, URL_ALBUM_1, fake_export, tmpdir,
    };
    use std::sync::atomic::AtomicBool;

    const PLENTY: u64 = 10 << 30;

    fn chat(peer: i64, label: &str) -> ChatGroup {
        ChatGroup {
            peer,
            label: label.into(),
            photos: Group::default(),
            voice: Group::default(),
            videos: 2,
            docs: 1,
            photo_links: 3,
            last: None,
        }
    }

    fn jpg() -> Vec<u8> {
        let mut b = vec![7u8; 1000];
        b[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        b
    }

    /// Opens, scans and runs once: `ok` urls answer with a jpg, the rest 404.
    fn run(s: &mut Session, ok: &[&str]) -> RunReport {
        let rows = s.index().rows(&ALL).unwrap();
        let files: Vec<(String, FakeFile)> = rows
            .iter()
            .map(|r| {
                let f = if ok.contains(&r.url.as_str()) {
                    FakeFile::ok(&jpg())
                } else {
                    FakeFile::status(404)
                };
                (r.url.clone(), f)
            })
            .collect();
        let (cdn, _) = FakeCdn::new(
            files
                .iter()
                .map(|(u, f)| (u.as_str(), vec![f.clone()]))
                .collect(),
        );
        s.start(
            &StartOptions::default(),
            &cdn,
            &FakeSpace(Ok(PLENTY)),
            &FakeClock::new().0,
            &AtomicBool::new(false),
            &NoProgress,
        )
    }

    fn opened(name: &str) -> Session {
        let root = fake_export(name, FakeExportOpts::default());
        let mut s = Session::open(&root, &root.with_extension("base")).unwrap();
        s.scan(&AtomicBool::new(false), &FakeSpace(Ok(PLENTY)))
            .unwrap();
        s
    }

    #[test]
    fn host_pattern_masks_digits() {
        assert_eq!(
            host_pattern("https://sun1-2.userapi.com/c1234/v1234/5a/x.jpg"),
            "sun#-#.userapi.com/c#"
        );
        assert_eq!(host_pattern(URL_ALBUM_1), "sun#-#.userapi.com/impg");
        assert_eq!(
            host_pattern("https://psv4-1.userapi.com/s/v1/d/aB3/voice.ogg"),
            "psv#-#.userapi.com/s"
        );
        assert_eq!(
            host_pattern("https://sun9-1.userapi.com/aB3cD/x.jpg"),
            "sun#-#.userapi.com/*"
        );
        assert_eq!(
            host_pattern("https://sun9-1.userapi.com/albums/x.jpg"),
            "sun#-#.userapi.com/*"
        );
        assert_eq!(
            host_pattern("https://friends.userapi.com/impg/x.jpg"),
            "*/impg"
        );
        assert_eq!(host_pattern("https://a.invalid/x"), "*/*");
    }

    #[test]
    fn report_write_failure_is_flagged_and_leaves_no_tmp() {
        let mut s = opened("report-fail");
        let blocker = s.root().join("vk-saver").join("report.html");
        std::fs::create_dir_all(&blocker).unwrap();
        let r = run(&mut s, &[URL_ALBUM_1]);
        assert!(r.report_failed);
        assert!(!s.root().join("vk-saver").join("report.html.tmp").exists());
        assert!(!r.to_json().to_string().contains("\"report_failed\":false"));
        std::fs::remove_dir(&blocker).unwrap();
        assert!(!run(&mut s, &[]).report_failed);
    }

    #[test]
    fn report_rewritten_after_undo() {
        let mut s = opened("report-undo");
        run(&mut s, &[URL_ALBUM_1]);
        let path = s.root().join("vk-saver").join("report.html");
        std::fs::write(&path, "stale").unwrap();
        s.undo().unwrap();
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains("Скачано файлов: 1 "), "{html}");
    }

    #[test]
    fn report_escapes_labels() {
        let d = tmpdir("report-escape");
        let v = ResultView {
            chats: vec![chat(2000000101, "<script>alert(1)</script>")],
            ..ResultView::default()
        };
        write_report(&d, &v).unwrap();
        let html = std::fs::read_to_string(d.join("report.html")).unwrap();
        assert!(!html.contains("<script"), "{html}");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("видео 2, документы 1, фото-ссылки 3 – только ссылки"));
        assert!(!d.join("report.html.tmp").exists());
    }

    #[test]
    fn report_links_relative_inside_export() {
        let mut s = opened("report-links");
        run(&mut s, &[]);
        let html = std::fs::read_to_string(s.root().join("vk-saver").join("report.html")).unwrap();
        let root = s.root().to_string_lossy().into_owned();
        assert!(!html.contains(&root));
        assert!(!html.contains("http") && !html.contains("file:") && !html.contains("src="));
        let hrefs: Vec<&str> = html.split("href=\"").skip(1).collect();
        assert!(!hrefs.is_empty(), "{html}");
        for h in hrefs {
            let h = h.split('"').next().unwrap();
            assert!(
                h.starts_with("../messages/") && h.ends_with("/messages0.html"),
                "{h}"
            );
            let target = h
                .split('/')
                .skip(1)
                .fold(s.root().to_path_buf(), |p, c| p.join(c));
            assert!(target.exists(), "{h}");
        }
        let pages = super::super::pages(s.root()).unwrap();
        assert!(!pages.iter().any(|p| p.contains("report")), "{pages:?}");
    }

    #[test]
    fn report_rewritten_after_each_run() {
        let mut s = opened("report-rerun");
        run(&mut s, &[URL_ALBUM_1]);
        let path = s.root().join("vk-saver").join("report.html");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("Скачано файлов: 1 ")
        );
        std::fs::write(&path, "stale").unwrap();
        run(&mut s, &[]);
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains("Скачано файлов: 1 ") && !html.contains("stale"));
    }

    #[test]
    fn diagnostics_carry_no_personal_data() {
        let mut s = opened("report-diag");
        run(&mut s, &[URL_ALBUM_1]);
        let v = s.diagnostics("0.1.0").unwrap();
        let out = serde_json::to_string(&v).unwrap();
        assert!(v["counts"]["albums"]["saved"].as_u64() == Some(1), "{out}");
        assert!(v["http"]["404"].as_u64().unwrap() > 0, "{out}");
        let rows = s.index().rows(&ALL).unwrap();
        assert!(!rows.is_empty());
        let mut banned: Vec<String> = [PEER_A, PEER_B, PEER_USER, PEER_CLUB, OWNER]
            .iter()
            .map(|x| x.to_string())
            .collect();
        banned.push(s.root().to_string_lossy().into_owned());
        banned.push("vk.ru".into());
        for r in &rows {
            let rest = r
                .url
                .split_once("://")
                .unwrap()
                .1
                .split('?')
                .next()
                .unwrap();
            banned.push(rest.to_string());
            banned.extend(
                rest.split('/')
                    .skip(2)
                    .filter(|x| x.len() >= 3)
                    .map(String::from),
            );
            banned.push(r.page.clone());
        }
        for c in &s.result().unwrap().chats {
            banned.push(c.label.clone());
            banned.push(c.peer.to_string());
        }
        for k in v["hosts"].as_object().unwrap().keys() {
            let (h, seg) = k.split_once('/').unwrap();
            let host_ok = h == "*"
                || h.split_once('.')
                    .is_some_and(|(l, t)| HOST_LABELS.contains(&l) && HOST_TAILS.contains(&t));
            assert!(host_ok && (seg == "*" || SEGMENTS.contains(&seg)), "{k}");
        }
        assert_eq!(host_pattern("https://friends.userapi.com/albums/x"), "*/*");
        for b in &banned {
            assert!(!out.contains(b.as_str()), "{b} in {out}");
        }
        for c in &s.result().unwrap().chats {
            let cp = encode_cp1251(&c.label);
            assert!(!out.as_bytes().windows(cp.len()).any(|w| w == cp.as_slice()));
        }
        for (i, _) in out.match_indices("userapi.com/") {
            let next = out[i + "userapi.com/".len()..].chars().next().unwrap();
            assert!(!next.is_ascii_digit(), "{out}");
        }
    }
}
