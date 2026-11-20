//! Case-insensitive search over the raw message pages of an export.

use crate::error::Stop;
use crate::export::scan::chat_peer;
use crate::export::text::{decode_label, unescape};
use crate::export::{pages, read_page};
use std::path::Path;

/// Characters of context kept on each side of a match.
const SNIPPET: usize = 60;
/// Hard cap on matches per search, no matter how many pages match.
pub const MAX_MATCHES: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub peer: i64,
    pub page: String,
    pub snippet: String,
}

/// Strips tags and decodes entities from a page; keeps the message text.
/// UTF-8 exports are detected like scan does; everything else is cp1251.
/// <script>/<style> blocks are dropped whole: their inner text is code, not messages.
fn text_of(bytes: &[u8]) -> String {
    let decoded = decode_label(bytes, std::str::from_utf8(bytes).is_ok());
    let mut out = String::with_capacity(decoded.len());
    let mut chars = decoded.chars().peekable();
    let mut skip: Option<String> = None;
    let mut pending = String::new();
    while let Some(c) = chars.next() {
        if let Some(close) = &skip {
            pending.push(c.to_ascii_lowercase());
            while pending.chars().count() > close.chars().count() {
                pending.remove(0);
            }
            if pending.ends_with(close.as_str()) {
                skip = None;
                pending.clear();
            }
            continue;
        }
        if c == '<' {
            let mut name = String::new();
            let mut closing = false;
            if chars.peek() == Some(&'/') {
                closing = true;
                chars.next();
            }
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_alphanumeric() {
                    name.push(nc.to_ascii_lowercase());
                    chars.next();
                } else {
                    break;
                }
            }
            while let Some(nc) = chars.next() {
                if nc == '>' {
                    break;
                }
            }
            if !closing && (name == "script" || name == "style") {
                skip = Some(format!("</{name}>"));
                pending.clear();
            }
            continue;
        }
        out.push(c);
    }
    unescape(&out)
}

fn snippet_of(text: &str, start: usize, end: usize) -> String {
    let from = text.floor_char_boundary(start.saturating_sub(SNIPPET));
    let to = text.floor_char_boundary((end + SNIPPET).min(text.len()));
    let mut s = text[from..to].replace(['\n', '\r', '\t'], " ");
    s = s.trim().to_string();
    if from > 0 {
        s.insert(0, '…');
    }
    if to < text.len() {
        s.push('…');
    }
    s
}

pub fn search(root: &Path, query: &str, limit: usize) -> Result<Vec<Match>, Stop> {
    let needle = query.trim().to_lowercase();
    if needle.chars().count() < 2 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for page in pages(root)? {
        if !page.starts_with("messages/") {
            continue;
        }
        let Some(peer) = chat_peer(&page) else {
            continue;
        };
        let path = page.split('/').fold(root.to_path_buf(), |p, c| p.join(c));
        // Oversized or unreadable pages are skipped, not fatal: one bad page must
        // not abort the search over the whole archive (scan skips them too).
        let Ok(bytes) = read_page(&path) else {
            continue;
        };
        let text = text_of(&bytes);
        // Match positions come from the lowered text. Cyrillic lowercase is
        // byte-stable in UTF-8; a few rare letters (İ, ẞ) change byte length, so
        // floor_char_boundary keeps slicing panic-free (snippet may shift by a char).
        let lower = text.to_lowercase();
        for (start, _) in lower.match_indices(needle.as_str()) {
            out.push(Match {
                peer,
                page: page.clone(),
                snippet: snippet_of(&text, start, start + needle.len()),
            });
            if out.len() >= limit {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeExportOpts, fake_export};

    #[test]
    fn finds_messages_case_insensitively() {
        let root = fake_export("search", FakeExportOpts::default());
        let m = search(&root, "привет", 10).unwrap();
        assert!(!m.is_empty());
        assert!(
            m.iter()
                .all(|x| x.snippet.to_lowercase().contains("привет"))
        );
        // "Привет" is written in the PEER_A chat of the fixture.
        assert!(
            m.iter()
                .any(|x| x.peer.to_string() == crate::testing::PEER_A)
        );
        assert!(m.iter().all(|x| x.page.starts_with("messages/")));
    }

    #[test]
    fn short_queries_return_nothing() {
        let root = fake_export("search-short", FakeExportOpts::default());
        assert!(search(&root, "п", 10).unwrap().is_empty());
        assert!(search(&root, "  ", 10).unwrap().is_empty());
    }

    #[test]
    fn snippet_stays_within_bounds() {
        let root = fake_export("search-bounds", FakeExportOpts::default());
        let m = search(&root, "вторая страница", 10).unwrap();
        assert!(!m.is_empty());
        for x in &m {
            // 2 * SNIPPET bytes of context + the needle + two 3-byte ellipsis characters.
            assert!(
                x.snippet.len() <= 2 * SNIPPET + 6 + "вторая страница".len(),
                "{}",
                x.snippet.len()
            );
        }
    }

    #[test]
    fn utf8_pages_are_searched() {
        let root = fake_export(
            "search-utf8",
            FakeExportOpts {
                utf8: true,
                ..Default::default()
            },
        );
        let m = search(&root, "привет", 10).unwrap();
        assert!(!m.is_empty(), "UTF-8 export must be searchable");
    }

    #[test]
    fn script_and_style_content_is_not_searched() {
        let html = "<html><head><style>привет css</style><script>привет js</script></head>\
                    <body><div class=\"item\">настоящий привет</div></body></html>";
        let text = text_of(html.as_bytes());
        assert!(text.contains("настоящий привет"));
        assert!(!text.contains("css"));
        assert!(!text.contains("js"));
    }
}
