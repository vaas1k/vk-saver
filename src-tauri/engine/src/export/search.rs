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

/// Strips tags and decodes entities from a cp1251 page; keeps the message text.
fn text_of(bytes: &[u8]) -> String {
    let decoded = decode_label(bytes, false);
    let mut out = String::with_capacity(decoded.len());
    let mut in_tag = false;
    for c in decoded.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
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
        let bytes = read_page(&path).map_err(|e| Stop::Storage(e.to_string()))?;
        let text = text_of(&bytes);
        // Cyrillic lowercase is byte-stable in UTF-8, so match positions in the
        // lowered text slice the original cleanly; floor_char_boundary guards the rest.
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
    use crate::testing::{fake_export, FakeExportOpts};

    #[test]
    fn finds_messages_case_insensitively() {
        let root = fake_export("search", FakeExportOpts::default());
        let m = search(&root, "привет", 10).unwrap();
        assert!(!m.is_empty());
        assert!(m.iter().all(|x| x.snippet.to_lowercase().contains("привет")));
        // "Привет" is written in the PEER_A chat of the fixture.
        assert!(m.iter().any(|x| x.peer.to_string() == crate::testing::PEER_A));
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
            assert!(x.snippet.len() <= 2 * SNIPPET + 6 + "вторая страница".len(), "{}", x.snippet.len());
        }
    }
}
