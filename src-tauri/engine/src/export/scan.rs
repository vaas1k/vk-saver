use super::GAP_MS;
use super::text::{decode_label, unescape, unescape_ascii};
use crate::cdn::allowed;
use crate::error::Stop;
use chrono::{NaiveDate, NaiveTime};
use reqwest::Url;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    Albums,
    Messages,
    Profile,
    Other,
}

impl Section {
    pub fn as_str(self) -> &'static str {
        match self {
            Section::Albums => "albums",
            Section::Messages => "messages",
            Section::Profile => "profile",
            Section::Other => "other",
        }
    }

    pub fn of(page: &str) -> Section {
        if page.starts_with("photos/photo-albums/") {
            Section::Albums
        } else if page.starts_with("messages/") {
            Section::Messages
        } else if page.starts_with("profile/") {
            Section::Profile
        } else {
            Section::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Album { owner: i64, id: u64 },
    Message { peer: i64, msg: u64, k: u32 },
    Avatar,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Photo,
    Voice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub value: Range<usize>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Found {
    pub url: String,
    pub page: Arc<str>,
    pub section: Section,
    pub key: Key,
    pub kind: Kind,
    pub date: Option<i64>,
    pub size: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drift {
    pub unmatched: u64,
    pub refused: u64,
    pub bad_dates: u64,
    /// Pages over `MAX_PAGE`, never read.
    pub too_large: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chat {
    pub peer: i64,
    pub label: String,
    pub videos: u64,
    pub docs: u64,
    pub photo_links: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub pages: Vec<String>,
    pub found: Vec<Found>,
    pub chats: Vec<Chat>,
    pub drift: Drift,
}

/// Bytes per pixel of a `size=WxH` photo; start value from spec aggregates, recalibrated in Task 18.
pub const BYTES_PER_PIXEL: f64 = 0.35;
pub const AVG_PHOTO: u64 = 465_000;
pub const AVG_VOICE: u64 = 215_000;

const ITEM: &[u8] = b"class=\"item\"";
pub(super) const PHOTO_LINK: &[u8] = b"vk.ru/photo";
const MESSAGE: &[u8] = b"class=\"message\" data-id=\"";
const ATTACHMENT: &[u8] = b"class=\"attachment\"";
const PEER_ID: &[u8] = b"message-peer--id";
const AVATAR_PAGE: &str = "profile/page-info.html";
const LABELS_PAGE: &str = "messages/index-messages.html";
const LABEL_MAX: usize = 80;
/// Link records per export; a real export has tens of thousands.
pub const MAX_FOUND: usize = 2_000_000;
const MON: [&str; 12] = [
    "янв", "фев", "мар", "апр", "мая", "июн", "июл", "авг", "сен", "окт", "ноя", "дек",
];
const MSK_S: i64 = 3 * 3600;

pub(super) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn positions(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = find(&hay[at..], needle) {
        out.push(at + i);
        at += i + needle.len();
    }
    out
}

/// Text between the `>` after `marker` and the next `<`.
fn tag_text<'a>(hay: &'a [u8], marker: &[u8]) -> Option<&'a [u8]> {
    let at = find(hay, marker)?;
    let start = at + hay[at..].iter().position(|&b| b == b'>')? + 1;
    let len = hay[start..].iter().position(|&b| b == b'<')?;
    Some(&hay[start..start + len])
}

fn text(bytes: &[u8]) -> String {
    decode_label(bytes, std::str::from_utf8(bytes).is_ok())
}

fn digits(s: &[u8]) -> bool {
    (1..=20).contains(&s.len()) && s.iter().all(u8::is_ascii_digit)
}

/// `^-?[1-9]\d{0,19}$` as a peer id.
fn peer_of(dir: &str) -> Option<i64> {
    let num = dir.strip_prefix('-').unwrap_or(dir);
    (digits(num.as_bytes()) && !num.starts_with('0'))
        .then(|| dir.parse().ok())
        .flatten()
}

/// The peer of a `messages/<dir>/<file>.html` page.
fn chat_peer(page: &str) -> Option<i64> {
    match page.split('/').collect::<Vec<_>>()[..] {
        ["messages", dir, _] => peer_of(dir),
        _ => None,
    }
}

pub(super) fn starts_with_ci(hay: &[u8], prefix: &[u8]) -> bool {
    hay.get(..prefix.len())
        .is_some_and(|h| h.eq_ignore_ascii_case(prefix))
}

/// Every quoted `href`/`src` value after whitespace, with its unescaped text.
fn attrs(page: &[u8]) -> Vec<(Range<usize>, Option<String>)> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < page.len() {
        let name = [&b"href="[..], b"src="]
            .into_iter()
            .find(|n| starts_with_ci(&page[i..], n));
        let Some(name) = name.filter(|_| page[i - 1].is_ascii_whitespace()) else {
            i += 1;
            continue;
        };
        let q = i + name.len();
        let Some(&quote @ (b'"' | b'\'')) = page.get(q) else {
            i = q;
            continue;
        };
        let start = q + 1;
        let Some(len) = page[start..].iter().position(|&b| b == quote) else {
            break;
        };
        let end = start + len;
        out.push((start..end, unescape_ascii(&page[start..end])));
        i = end + 1;
    }
    out
}

pub fn sweep(page: &[u8]) -> Vec<Attr> {
    attrs(page)
        .into_iter()
        .filter_map(|(value, url)| url.filter(|u| usable(u)).map(|url| Attr { value, url }))
        .collect()
}

/// `D <mon> YYYY в HH:MM[:SS]` printed in Moscow time, cp1251 or UTF-8.
pub fn parse_date(bytes: &[u8]) -> Option<i64> {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => decode_label(bytes, false),
    };
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [d, mon, y, "в", t] = parts[..] else {
        return None;
    };
    let num = |s: &str| -> Option<u32> {
        (!s.is_empty() && s.len() <= 4 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    let m = MON.iter().position(|m| mon.chars().take(3).eq(m.chars()))? as u32 + 1;
    let hms: Vec<&str> = t.split(':').collect();
    if y.len() != 4 || !(2..=3).contains(&hms.len()) {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(num(y)? as i32, m, num(d)?)?;
    let sec = hms.get(2).map_or(Some(0), |s| num(s))?;
    let time = NaiveTime::from_hms_opt(num(hms[0])?, num(hms[1])?, sec)?;
    // shortcut: fixed +3, pre-2014 summer dates off by 1 h; add an MSK offset table if anyone notices
    Some(date.and_time(time).and_utc().timestamp() - MSK_S)
}

pub fn estimate_bytes(f: &Found) -> u64 {
    match (f.size, f.kind) {
        (Some((w, h)), _) => (f64::from(w) * f64::from(h) * BYTES_PER_PIXEL) as u64,
        (None, Kind::Voice) => AVG_VOICE,
        (None, Kind::Photo) => AVG_PHOTO,
    }
}

fn ids(token: &[u8]) -> Option<Key> {
    let (owner, id) = std::str::from_utf8(token).ok()?.split_once('_')?;
    if !digits(owner.strip_prefix('-').unwrap_or(owner).as_bytes()) || !digits(id.as_bytes()) {
        return None;
    }
    Some(Key::Album {
        owner: owner.parse().ok()?,
        id: id.parse().ok()?,
    })
}

/// `<a href="https://vk.ru/photo<owner>_<id>">` of one album item.
fn album_key(item: &[u8]) -> Option<Key> {
    let start = find(item, PHOTO_LINK)? + PHOTO_LINK.len();
    let len = item[start..]
        .iter()
        .position(|&b| b == b'"' || b == b'\'')?;
    ids(&item[start..start + len])
}

/// `None` when the item has no `clear_fix`, `Some(None)` when its date does not parse.
fn clear_fix_date(item: &[u8]) -> Option<Option<i64>> {
    let at = find(item, b"clear_fix")?;
    let start = at + item[at..].iter().position(|&b| b == b'>')? + 1;
    let end = item[start..]
        .iter()
        .position(|&b| b == b'<')
        .map_or(item.len(), |e| start + e);
    Some(parse_date(&item[start..end]))
}

fn is_src(item: &[u8], value_start: usize) -> bool {
    value_start >= 5 && starts_with_ci(&item[value_start - 5..], b"src=")
}

/// Allowed url without ASCII control characters (an entity like `&#10;` can smuggle one in).
fn usable(url: &str) -> bool {
    !url.bytes().any(|b| b < 0x20 || b == 0x7F) && allowed(url)
}

fn url_facts(url: &str) -> (Kind, Option<(u32, u32)>) {
    let Ok(u) = Url::parse(url) else {
        return (Kind::Photo, None);
    };
    let kind = if u.path().to_ascii_lowercase().ends_with(".ogg") {
        Kind::Voice
    } else {
        Kind::Photo
    };
    let size = u
        .query_pairs()
        .find(|(k, _)| k == "size")
        .and_then(|(_, v)| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        });
    (kind, size)
}

fn page_found(page: &Arc<str>, bytes: &[u8], drift: &mut Drift) -> Vec<Found> {
    let section = Section::of(page);
    let mut cuts = vec![0];
    if section == Section::Albums {
        let mut at = 0;
        while let Some(i) = find(&bytes[at..], ITEM) {
            cuts.push(at + i);
            at += i + ITEM.len();
        }
    }
    cuts.push(bytes.len());
    let mut out = Vec::new();
    for (n, w) in cuts.windows(2).enumerate() {
        let item = &bytes[w[0]..w[1]];
        let (key, date) = if n > 0 {
            let date = clear_fix_date(item);
            if date == Some(None) {
                drift.bad_dates += 1;
            }
            (album_key(item), date.flatten())
        } else if **page == *AVATAR_PAGE {
            (Some(Key::Avatar), None)
        } else {
            (None, None)
        };
        let item_attrs = attrs(item);
        // only the first `<img src>` carries the album key; `<a href>` wrappers repeat its url
        let img = key.as_ref().and_then(|_| {
            item_attrs.iter().find_map(|(r, u)| {
                let u = u.as_deref().filter(|u| usable(u))?;
                is_src(item, r.start).then_some((r.start, u))
            })
        });
        for (r, url) in &item_attrs {
            let Some(url) = url else { continue };
            if !usable(url) {
                if url.to_ascii_lowercase().contains("userapi.com") {
                    drift.refused += 1;
                }
                continue;
            }
            let keyed = match img {
                Some((at, u)) if u == url => {
                    if r.start != at {
                        continue;
                    }
                    true
                }
                _ => false,
            };
            if !keyed {
                drift.unmatched += 1;
            }
            let (kind, size) = url_facts(url);
            out.push(Found {
                url: url.clone(),
                page: page.clone(),
                section,
                key: if keyed {
                    key.clone().unwrap_or(Key::Unknown)
                } else {
                    Key::Unknown
                },
                kind,
                date: if keyed { date } else { None },
                size,
            });
        }
    }
    out
}

fn is_bidi(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')
}

fn has_attachment_class(attrs: &[u8]) -> bool {
    (1..attrs.len()).any(|i| {
        let Some(&q @ (b'"' | b'\'')) = attrs.get(i + 6) else {
            return false;
        };
        if !attrs[i - 1].is_ascii_whitespace() || !starts_with_ci(&attrs[i..], b"class=") {
            return false;
        }
        let v = &attrs[i + 7..];
        find(
            &v[..v.iter().position(|&b| b == q).unwrap_or(v.len())],
            b"attachment__link",
        )
        .is_some()
    })
}

/// `<...>` spans of `block` in one forward pass, each flagged when it is an `<a ...>` with the
/// `attachment__link` class.
pub(super) fn link_tags(block: &[u8]) -> Vec<(Range<usize>, bool)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(s) = block[i..].iter().position(|&b| b == b'<') {
        let start = i + s;
        let Some(e) = block[start..].iter().position(|&b| b == b'>') else {
            break;
        };
        let end = start + e;
        let tag = &block[start..end];
        out.push((
            start..end,
            starts_with_ci(tag, b"<a")
                && tag.get(2).is_some_and(u8::is_ascii_whitespace)
                && has_attachment_class(tag),
        ));
        i = end + 1;
    }
    out
}

/// Index in `tags` of the attachment anchor whose `href` value is `value`.
pub(super) fn attachment_link(
    block: &[u8],
    tags: &[(Range<usize>, bool)],
    value: &Range<usize>,
) -> Option<usize> {
    let i = tags.partition_point(|(r, _)| r.end <= value.start);
    let (r, link) = tags.get(i)?;
    let head = &block[r.start.min(value.start)..value.start];
    (*link
        && r.start < value.start
        && value.end <= r.end
        && head.len() >= 6
        && starts_with_ci(&head[head.len() - 6..], b"href="))
    .then_some(i)
}

fn is_voice_host(url: &str) -> bool {
    Url::parse(url).ok().is_some_and(|u| {
        u.host_str()
            .and_then(|h| h.strip_prefix("psv"))
            .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// `messages/<peer>/` page: attachments numbered per message, link-only ones counted in `chat`.
fn message_found(page: &Arc<str>, bytes: &[u8], drift: &mut Drift, chat: &mut Chat) -> Vec<Found> {
    let starts = positions(bytes, MESSAGE);
    let first = starts.first().copied().unwrap_or(bytes.len());
    let mut out = page_found(page, &bytes[..first], drift);
    for (i, &at) in starts.iter().enumerate() {
        let item = &bytes[at..starts.get(i + 1).copied().unwrap_or(bytes.len())];
        let id = &item[MESSAGE.len()..];
        let msg = id
            .iter()
            .position(|&b| b == b'"')
            .filter(|&n| digits(&id[..n]))
            .and_then(|n| std::str::from_utf8(&id[..n]).ok()?.parse::<u64>().ok());
        let date = find(item, b"message__header").and_then(|at| {
            let end = at + find(&item[at..], b"</div>")?;
            let head = &item[at..end];
            let comma = head.windows(2).rposition(|w| w == b", ")?;
            let date = parse_date(&head[comma + 2..]);
            if date.is_none() {
                drift.bad_dates += 1;
            }
            date
        });
        let atts = positions(item, ATTACHMENT);
        let body = atts.first().copied().unwrap_or(item.len());
        out.extend(page_found(page, &item[..body], drift));
        for (j, &a) in atts.iter().enumerate() {
            let block = &item[a..atts.get(j + 1).copied().unwrap_or(item.len())];
            let desc = tag_text(block, b"attachment__description").map(text);
            let desc = desc.as_deref().map(str::trim);
            let mut key = msg
                .filter(|_| matches!(desc, Some("Фотография" | "Файл")))
                .map(|msg| Key::Message {
                    peer: chat.peer,
                    msg,
                    k: j as u32 + 1,
                });
            // only `attachment__link` anchors count, not a trailing pagination link
            let mut counted = false;
            let tags = link_tags(block);
            for (r, url) in attrs(block) {
                let Some(url) = url else { continue };
                if !usable(&url) {
                    if url.to_ascii_lowercase().contains("userapi.com") {
                        drift.refused += 1;
                    } else if !counted && attachment_link(block, &tags, &r).is_some() {
                        counted = true;
                        match desc {
                            Some("Видеозапись") => chat.videos += 1,
                            Some("Файл") => chat.docs += 1,
                            Some("Фотография") => chat.photo_links += 1,
                            _ => {}
                        }
                    }
                    continue;
                }
                let (mut kind, size) = url_facts(&url);
                if desc == Some("Файл") && is_voice_host(&url) {
                    kind = Kind::Voice;
                }
                let key = key.take();
                if key.is_none() {
                    drift.unmatched += 1;
                }
                out.push(Found {
                    url,
                    page: page.clone(),
                    section: Section::Messages,
                    date: key.as_ref().and(date),
                    key: key.unwrap_or(Key::Unknown),
                    kind,
                    size,
                });
            }
        }
    }
    out
}

/// Peer -> label from `index-messages.html`; the first link per peer wins.
fn labels(bytes: &[u8]) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    let starts = positions(bytes, PEER_ID);
    for (i, &at) in starts.iter().enumerate() {
        let item = &bytes[at..starts.get(i + 1).copied().unwrap_or(bytes.len())];
        let Some((r, Some(href))) = attrs(item).into_iter().next() else {
            continue;
        };
        let Some(peer) = href.split('/').next().and_then(peer_of) else {
            continue;
        };
        let rest = &item[r.end..];
        let Some(start) = rest.iter().position(|&b| b == b'>') else {
            continue;
        };
        let rest = &rest[start + 1..];
        let end = find(rest, b"</a>")
            .or_else(|| rest.iter().position(|&b| b == b'<'))
            .unwrap_or(rest.len());
        let raw = &rest[..end];
        let mut plain = Vec::with_capacity(raw.len());
        let mut in_tag = false;
        for &b in raw {
            match b {
                b'<' => in_tag = true,
                b'>' if in_tag => in_tag = false,
                _ if !in_tag => plain.push(b),
                _ => {}
            }
        }
        let decoded: String = unescape(decode_label(&plain, false).trim())
            .chars()
            .filter(|&c| !c.is_control() && !is_bidi(c))
            .collect();
        let label: String = decoded.trim().chars().take(LABEL_MAX).collect();
        if !label.is_empty() {
            out.entry(peer).or_insert(label);
        }
    }
    out
}

/// Appends one page's records without repeats; past `MAX_FOUND` the input is refused.
fn add_page(all: &mut Vec<Found>, page: Vec<Found>, cap: usize) -> Result<(), Stop> {
    let mut seen = HashSet::new();
    all.extend(page.into_iter().filter(|f| seen.insert(f.clone())));
    if all.len() > cap {
        return Err(Stop::NotExport);
    }
    Ok(())
}

pub fn scan(root: &Path, cancel: &AtomicBool) -> Result<Scan, Stop> {
    let pages = super::pages(root)?;
    let mut s = Scan::default();
    let mut names = HashMap::new();
    for page in &pages {
        if cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let path = page.split('/').fold(root.to_path_buf(), |p, c| p.join(c));
        // The index follows the export, not our rewrite of it: links we replaced stay known.
        let bytes = match super::read_page(&path)
            .and_then(|live| super::rewrite::export_bytes(root, page, live))
        {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::FileTooLarge => {
                s.drift.too_large += 1;
                continue;
            }
            Err(e) => return Err(Stop::Storage(e.to_string())),
        };
        if page == LABELS_PAGE {
            names = labels(&bytes);
        }
        let name: Arc<str> = page.as_str().into();
        let Some(peer) = chat_peer(page) else {
            let found = page_found(&name, &bytes, &mut s.drift);
            add_page(&mut s.found, found, MAX_FOUND)?;
            continue;
        };
        if s.chats.last().is_none_or(|c| c.peer != peer) {
            s.chats.push(Chat {
                peer,
                label: String::new(),
                videos: 0,
                docs: 0,
                photo_links: 0,
            });
        }
        let chat = s.chats.last_mut().expect("pushed above");
        let found = message_found(&name, &bytes, &mut s.drift, chat);
        add_page(&mut s.found, found, MAX_FOUND)?;
    }
    for c in &mut s.chats {
        c.label = names
            .remove(&c.peer)
            .unwrap_or_else(|| format!("Чат {}", c.peer));
    }
    s.pages = pages;
    Ok(s)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Group {
    pub count: u64,
    pub bytes: u64,
    pub saved: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGroup {
    pub peer: i64,
    pub label: String,
    pub photos: Group,
    pub voice: Group,
    pub videos: u64,
    pub docs: u64,
    pub photo_links: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub root: String,
    pub free_bytes: u64,
    pub albums: Group,
    pub messages: Group,
    pub voice: Group,
    pub other: Group,
    pub chats: Vec<ChatGroup>,
    pub videos: u64,
    pub docs: u64,
    pub photo_links: u64,
    pub too_large: u64,
    pub eta_s: u64,
}

fn group_json(g: &Group) -> Value {
    json!({ "count": g.count, "bytes": g.bytes, "saved": g.saved })
}

impl Summary {
    pub fn to_json(&self) -> Value {
        let chats: Vec<Value> = self
            .chats
            .iter()
            .map(|c| {
                json!({
                    "peer": c.peer,
                    "label": c.label,
                    "photos": group_json(&c.photos),
                    "voice": group_json(&c.voice),
                    "videos": c.videos,
                    "docs": c.docs,
                    "photo_links": c.photo_links,
                })
            })
            .collect();
        json!({
            "root": self.root,
            "free_bytes": self.free_bytes,
            "albums": group_json(&self.albums),
            "messages": group_json(&self.messages),
            "voice": group_json(&self.voice),
            "other": group_json(&self.other),
            "chats": chats,
            "videos": self.videos,
            "docs": self.docs,
            "photo_links": self.photo_links,
            "eta_s": self.eta_s,
            "too_large": self.too_large,
        })
    }
}

/// Counts each URL once per group.
fn group<'a>(found: impl Iterator<Item = &'a Found>, saved: &dyn Fn(&str) -> bool) -> Group {
    let mut seen = HashSet::new();
    let mut g = Group::default();
    for f in found.filter(|f| seen.insert(f.url.as_str())) {
        g.count += 1;
        g.bytes = g.bytes.saturating_add(estimate_bytes(f));
        g.saved += u64::from(saved(&f.url));
    }
    g
}

pub fn summarize(
    scan: &Scan,
    saved: &dyn Fn(&str) -> bool,
    root: String,
    free_bytes: u64,
) -> Summary {
    let photos = |section: Section| {
        scan.found
            .iter()
            .filter(move |f| f.kind == Kind::Photo && f.section == section)
    };
    let voice = || scan.found.iter().filter(|f| f.kind == Kind::Voice);
    let mut by_chat: HashMap<i64, Vec<&Found>> = HashMap::new();
    for f in &scan.found {
        if let Some(peer) = chat_peer(&f.page) {
            by_chat.entry(peer).or_default().push(f);
        }
    }
    let chats: Vec<ChatGroup> = scan
        .chats
        .iter()
        .map(|c| {
            let mine = by_chat.get(&c.peer).map_or(&[][..], Vec::as_slice);
            ChatGroup {
                peer: c.peer,
                label: c.label.clone(),
                photos: group(
                    mine.iter()
                        .copied()
                        .filter(|f| f.kind == Kind::Photo && f.section == Section::Messages),
                    saved,
                ),
                voice: group(
                    mine.iter().copied().filter(|f| f.kind == Kind::Voice),
                    saved,
                ),
                videos: c.videos,
                docs: c.docs,
                photo_links: c.photo_links,
            }
        })
        .collect();
    let all: HashSet<&str> = scan.found.iter().map(|f| f.url.as_str()).collect();
    let missing = all.into_iter().filter(|u| !saved(u)).count() as u64;
    Summary {
        root,
        free_bytes,
        albums: group(photos(Section::Albums), saved),
        messages: group(photos(Section::Messages), saved),
        voice: group(voice(), saved),
        other: group(
            photos(Section::Profile).chain(photos(Section::Other)),
            saved,
        ),
        videos: chats.iter().map(|c| c.videos).sum(),
        docs: chats.iter().map(|c| c.docs).sum(),
        photo_links: chats.iter().map(|c| c.photo_links).sum(),
        chats,
        too_large: scan.drift.too_large,
        eta_s: (missing * GAP_MS).div_ceil(1000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::text::encode_cp1251;
    use crate::testing::*;

    static NO: AtomicBool = AtomicBool::new(false);

    fn fixture(name: &str, opts: FakeExportOpts) -> Scan {
        scan(&fake_export(name, opts), &NO).unwrap()
    }

    fn album(owner: i64, id: u64) -> Key {
        Key::Album { owner, id }
    }

    #[test]
    fn sweep_both_quotes_and_unescape() {
        let page = b"<a HREF='https://sun9-1.userapi.com/a.jpg?x=1&amp;sign=x'>t</a>\
            <img src=\"https://sun9-2.userapi.com/b.jpg\">\
            <a href=\"http://sun9-2.userapi.com/c.jpg\"></a><a href=\"https://vk.ru/photo1_2\"></a>\
            <img src='https://sun1-1.userapi.com.evil.invalid/d.jpg'><img data-src=\"https://sun9-3.userapi.com/e.jpg\">";
        let got = sweep(page);
        let urls: Vec<&str> = got.iter().map(|a| a.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://sun9-1.userapi.com/a.jpg?x=1&sign=x",
                "https://sun9-2.userapi.com/b.jpg"
            ]
        );
        assert_eq!(
            &page[got[0].value.clone()],
            b"https://sun9-1.userapi.com/a.jpg?x=1&amp;sign=x"
        );
        assert_eq!(
            &page[got[1].value.clone()],
            b"https://sun9-2.userapi.com/b.jpg"
        );
    }

    #[test]
    fn sweep_ignores_pagination_quirk() {
        let page =
            b"<a class=\"pg_lnk fl_l\" href=\"messages50.html\".html>2</a><img src=\"https://sun9-1.userapi.com/x.jpg\">";
        let got = sweep(page);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].url, "https://sun9-1.userapi.com/x.jpg");
    }

    #[test]
    fn album_items_keyed() {
        let s = fixture("scan-album", FakeExportOpts::default());
        let f: Vec<&Found> = s
            .found
            .iter()
            .filter(|f| &*f.page == "photos/photo-albums/album-1.html")
            .collect();
        assert_eq!(f.len(), 7);
        assert!(f.iter().all(|f| f.section == Section::Albums));
        assert_eq!(f[0].url, URL_ALBUM_1);
        assert_eq!(f[0].key, album(200001, 5001));
        assert_eq!(f[0].kind, Kind::Photo);
        assert_eq!(f[0].date, Some(1709631296));
        assert_eq!(f[0].size, Some((1280, 960)));
        assert_eq!(f[1].size, Some((807, 1080)));
        assert_eq!(f[6].key, album(200001, 5007));
        assert_eq!(s.drift.bad_dates, 0);
    }

    #[test]
    fn placeholder_one_url_three_ids() {
        let s = fixture("scan-placeholder", FakeExportOpts::default());
        let keys: Vec<&Key> = s
            .found
            .iter()
            .filter(|f| f.url == URL_PLACEHOLDER)
            .map(|f| &f.key)
            .collect();
        assert_eq!(
            keys,
            [
                &album(200001, 5004),
                &album(200001, 5005),
                &album(200001, 5006)
            ]
        );
        let sum = summarize(&s, &|_| false, "r".into(), 0);
        // ALBUM_1..3, PLACEHOLDER, OLD_HOST, tagged 5001
        assert_eq!(sum.albums.count, 6);
    }

    #[test]
    fn old_host_no_query_estimated_avg() {
        let s = fixture("scan-old-host", FakeExportOpts::default());
        let f = s.found.iter().find(|f| f.url == URL_OLD_HOST).unwrap();
        assert_eq!(f.size, None);
        assert_eq!(estimate_bytes(f), AVG_PHOTO);
        let a = s.found.iter().find(|f| f.url == URL_ALBUM_1).unwrap();
        assert_eq!(estimate_bytes(a), (1280.0 * 960.0 * BYTES_PER_PIXEL) as u64);
        let v = s.found.iter().find(|f| f.url == URL_VOICE).unwrap();
        assert_eq!((v.kind, estimate_bytes(v)), (Kind::Voice, AVG_VOICE));
    }

    #[test]
    fn date_parse_cases() {
        let s = "5 мар 2024 в 12:34:56";
        assert_eq!(parse_date(&encode_cp1251(s)), Some(1709631296));
        assert_eq!(parse_date(s.as_bytes()), Some(1709631296));
        assert_eq!(
            parse_date(" 5 марта 2024 в 12:34 ".as_bytes()),
            Some(1709631240)
        );
        assert_eq!(
            parse_date(&encode_cp1251("31 дек 1999 в 23:59")),
            Some(946673940)
        );
        for bad in [
            "31 фев 2024 в 12:34",
            "5 abc 2024 в 12:34",
            "5 мар 2024 12:34",
            "5 мар 2024 в 25:00",
            "5 мар 24 в 12:34",
            "",
            "garbage",
        ] {
            assert_eq!(parse_date(bad.as_bytes()), None, "{bad}");
            assert_eq!(parse_date(&encode_cp1251(bad)), None, "{bad}");
        }
    }

    #[test]
    fn hostile_ids_are_unknown() {
        let d = tmpdir("scan-hostile");
        std::fs::create_dir_all(d.join("photos").join("photo-albums")).unwrap();
        std::fs::write(d.join("index.html"), b"x").unwrap();
        let item = |owner: &str, id: &str, n: u32| {
            format!(
                "<div class=\"item\"><a href=\"https://vk.ru/photo{owner}_{id}\"><img src=\"https://sun9-1.userapi.com/{n}.jpg\"></a><div class=\"clear_fix\">5 мар 2024 в 12:34</div></div>\n"
            )
        };
        let digits21 = "1".repeat(21);
        let page = [
            item("../1", "5001", 1),
            item("1/2", "5001", 2),
            item(&digits21, "5001", 3),
            item("200001", "12a", 4),
            item("200001", &digits21, 5),
            item("-200003", "5001", 6),
            "<div class=\"item\"><div class=\"clear_fix\">31 фев 2024 в 12:34</div></div>\n".into(),
        ]
        .concat();
        std::fs::write(
            d.join("photos").join("photo-albums").join("a.html"),
            encode_cp1251(&page),
        )
        .unwrap();
        let s = scan(&d, &NO).unwrap();
        let keys: Vec<&Key> = s.found.iter().map(|f| &f.key).collect();
        let u = &Key::Unknown;
        assert_eq!(keys, [u, u, u, u, u, &album(-200003, 5001)]);
        assert_eq!(s.drift.unmatched, 5);
        assert_eq!(s.drift.bad_dates, 1);
    }

    #[test]
    fn crlf_and_utf8_same_found() {
        let set = |s: Scan| -> HashSet<(String, Key, Option<i64>)> {
            s.found
                .into_iter()
                .map(|f| (f.url, f.key, f.date))
                .collect()
        };
        let lf = set(fixture("scan-lf", FakeExportOpts::default()));
        let crlf = set(fixture(
            "scan-crlf",
            FakeExportOpts {
                crlf: true,
                ..Default::default()
            },
        ));
        let utf8 = set(fixture(
            "scan-utf8",
            FakeExportOpts {
                utf8: true,
                ..Default::default()
            },
        ));
        assert!(lf.len() > 10);
        assert_eq!(lf, crlf);
        assert_eq!(lf, utf8);
    }

    #[test]
    fn drift_counted() {
        let s = fixture("scan-drift", FakeExportOpts::default());
        let w = s.found.iter().find(|f| f.url == URL_WALL_DRIFT).unwrap();
        assert_eq!((w.key.clone(), w.section), (Key::Unknown, Section::Other));
        assert_eq!(&*w.page, "wall/wall0.html");
        let av = s.found.iter().find(|f| f.url == URL_AVATAR).unwrap();
        assert_eq!(av.section, Section::Profile);
        assert_eq!(av.key, Key::Avatar);
        // wall
        assert_eq!(s.drift.unmatched, 1);
        // URL_HTTP and URL_EVIL
        assert_eq!(s.drift.refused, 2);
        assert_eq!(s.pages.len(), 12);
        assert_eq!(s.chats.len(), 4);
    }

    #[test]
    fn summary_counts_and_eta() {
        let s = fixture("scan-summary", FakeExportOpts::default());
        let none = summarize(&s, &|_| false, "R".into(), 7);
        // CHAT_1, CHAT_2, CHAT_3, SHARED; voice separate
        assert_eq!(none.messages.count, 4);
        assert_eq!(none.voice.count, 1);
        assert_eq!(none.voice.bytes, AVG_VOICE);
        assert_eq!(none.other.count, 2);
        assert_eq!(none.albums.saved, 0);
        // 6 album + 4 message + 1 voice + 2 other, all distinct
        assert_eq!(none.eta_s, 7);
        let one = summarize(&s, &|u| u == URL_ALBUM_1, "R".into(), 7);
        assert_eq!(one.albums.saved, 1);
        assert_eq!(one.albums.count, 6);
        assert_eq!(one.eta_s, 6);
        let j = one.to_json();
        assert_eq!(j["root"], "R");
        assert_eq!(j["free_bytes"], 7);
        assert_eq!(j["albums"]["saved"], 1);
        assert_eq!(j["eta_s"], one.eta_s);
        let chats = j["chats"].as_array().unwrap();
        assert_eq!(chats.len(), 4);
        assert_eq!(chats[2]["label"], "Тестовый чат");
        assert_eq!(chats[2]["photos"]["count"], 4);
        assert_eq!(
            (j["videos"].as_u64(), j["docs"].as_u64()),
            (Some(1), Some(1))
        );
    }

    #[test]
    fn album_key_only_on_first_img_src() {
        let u = |n: u32| format!("https://sun9-1.userapi.com/{n}.jpg");
        let page = format!(
            "<div class=\"item\"><a href=\"{0}\"><img src=\"{0}\"></a>\
             <a href=\"https://vk.ru/photo1_2\"></a><img src=\"{1}\">\
             <div class=\"clear_fix\">5 мар 2024 в 12:34</div></div>\
             <div class=\"footer\"><a href=\"{2}\">next</a><img src=\"{3}\"></div>",
            u(1),
            u(2),
            u(3),
            u(4)
        );
        let mut drift = Drift::default();
        let f = page_found(
            &"photos/photo-albums/a.html".into(),
            page.as_bytes(),
            &mut drift,
        );
        let got: Vec<(String, Key)> = f.into_iter().map(|f| (f.url, f.key)).collect();
        assert_eq!(
            got,
            [
                (u(1), album(1, 2)),
                (u(2), Key::Unknown),
                (u(3), Key::Unknown),
                (u(4), Key::Unknown)
            ]
        );
        assert_eq!(drift.unmatched, 3);
    }

    #[test]
    fn control_chars_are_refused() {
        let page = b"<img src=\"https://sun9-1.userapi.com/a&#10;b.jpg\"><img src=\"https://sun9-1.userapi.com/a&#0;b.jpg\"><img src=\"https://sun9-1.userapi.com/ok.jpg\">";
        assert_eq!(sweep(page).len(), 1);
        let mut drift = Drift::default();
        let f = page_found(&"wall/w.html".into(), page, &mut drift);
        assert_eq!((f.len(), drift.refused), (1, 2));
    }

    #[test]
    fn hostile_size_does_not_overflow_totals() {
        let f = |n: u32| Found {
            url: format!("https://sun9-1.userapi.com/{n}.jpg?size=4294967295x4294967295"),
            page: "p".into(),
            section: Section::Albums,
            key: Key::Unknown,
            kind: Kind::Photo,
            date: None,
            size: Some((u32::MAX, u32::MAX)),
        };
        let found = [f(1), f(2), f(3)];
        assert_eq!(group(found.iter(), &|_| false).bytes, u64::MAX);
    }

    fn msg(peer: &str, msg: u64, k: u32) -> Key {
        Key::Message {
            peer: peer.parse().unwrap(),
            msg,
            k,
        }
    }

    fn key_of<'a>(s: &'a Scan, url: &str, page: &str) -> &'a Key {
        &s.found
            .iter()
            .find(|f| f.url == url && &*f.page == page)
            .unwrap()
            .key
    }

    #[test]
    fn message_keys_and_k_numbering() {
        let s = fixture("scan-msg-keys", FakeExportOpts::default());
        let a0 = format!("messages/{PEER_A}/messages0.html");
        assert_eq!(key_of(&s, URL_CHAT_1, &a0), &msg(PEER_A, 1001, 1));
        assert_eq!(key_of(&s, URL_CHAT_2, &a0), &msg(PEER_A, 1002, 1));
        assert_eq!(key_of(&s, URL_SHARED, &a0), &msg(PEER_A, 1002, 2));
        assert_eq!(key_of(&s, URL_VOICE, &a0), &msg(PEER_A, 1002, 3));
        let a50 = format!("messages/{PEER_A}/messages50.html");
        assert_eq!(key_of(&s, URL_CHAT_1, &a50), &msg(PEER_A, 1051, 1));
        let b0 = format!("messages/{PEER_B}/messages0.html");
        assert_eq!(key_of(&s, URL_SHARED, &b0), &msg(PEER_B, 1101, 1));
        let v = s.found.iter().find(|f| f.url == URL_VOICE).unwrap();
        assert_eq!((v.kind, v.date), (Kind::Voice, Some(1709631296)));
        assert_eq!(v.section, Section::Messages);
    }

    #[test]
    fn link_only_counts_per_chat() {
        let s = fixture("scan-links", FakeExportOpts::default());
        let peers: Vec<i64> = s.chats.iter().map(|c| c.peer).collect();
        let p = |x: &str| x.parse::<i64>().unwrap();
        assert_eq!(peers, [p(PEER_CLUB), p(PEER_USER), p(PEER_A), p(PEER_B)]);
        let counts = |peer: &str| {
            let c = s.chats.iter().find(|c| c.peer == p(peer)).unwrap();
            (c.videos, c.docs, c.photo_links)
        };
        assert_eq!(counts(PEER_A), (1, 1, 1));
        assert_eq!(counts(PEER_B), (0, 0, 0));
        assert!(s.found.iter().all(|f| !f.url.contains("vk.ru")));
        let sum = summarize(&s, &|_| false, "R".into(), 0);
        assert_eq!((sum.videos, sum.docs, sum.photo_links), (1, 1, 1));
        let a = sum.chats.iter().find(|c| c.peer == p(PEER_A)).unwrap();
        assert_eq!((a.photos.count, a.voice.count), (4, 1));
        let b = sum.chats.iter().find(|c| c.peer == p(PEER_B)).unwrap();
        assert_eq!(b.photos.count, 1);
    }

    #[test]
    fn labels_decoded_escaped_capped() {
        let s = fixture("scan-labels", FakeExportOpts::default());
        let label = |s: &Scan, peer: &str| {
            let peer: i64 = peer.parse().unwrap();
            s.chats
                .iter()
                .find(|c| c.peer == peer)
                .unwrap()
                .label
                .clone()
        };
        assert_eq!(label(&s, PEER_A), "Тестовый чат");
        assert_eq!(label(&s, PEER_B), "Второй & чат");

        let d = tmpdir("scan-labels-cap");
        std::fs::create_dir_all(d.join("messages").join(PEER_A)).unwrap();
        std::fs::create_dir_all(d.join("messages").join(PEER_B)).unwrap();
        std::fs::write(d.join("index.html"), b"x").unwrap();
        let mut long =
            b"<div class=\"message-peer--id\"><a href=\"2000000102/messages0.html\"> \xAB".to_vec();
        long.extend(encode_cp1251(&"я".repeat(200)));
        long.extend(b"</a></div>");
        std::fs::write(d.join("messages").join("index-messages.html"), long).unwrap();
        for peer in [PEER_A, PEER_B] {
            std::fs::write(d.join("messages").join(peer).join("messages0.html"), b"x").unwrap();
        }
        let s = scan(&d, &NO).unwrap();
        assert_eq!(label(&s, PEER_A), "Чат 2000000101");
        let b = label(&s, PEER_B);
        assert_eq!(b.chars().count(), 80);
        assert!(b.starts_with("\u{00AB}яя"));
    }

    #[test]
    fn hostile_dir_and_data_id() {
        let d = tmpdir("scan-hostile-msg");
        std::fs::create_dir_all(d.join("messages").join("abc")).unwrap();
        std::fs::create_dir_all(d.join("messages").join(PEER_A)).unwrap();
        std::fs::write(d.join("index.html"), b"x").unwrap();
        let m = |id: &str, n: u32| {
            format!(
                "<div class=\"message\" data-id=\"{id}\"><div class=\"message__header\">X, 5 мар 2024 в 12:34</div><div class=\"attachment\"><div class=\"attachment__description\">Фотография</div><a class='attachment__link' href='https://sun9-1.userapi.com/{n}.jpg'>x</a></div></div>"
            )
        };
        std::fs::write(
            d.join("messages").join("abc").join("messages0.html"),
            encode_cp1251(&m("1001", 1)),
        )
        .unwrap();
        std::fs::write(
            d.join("messages").join(PEER_A).join("messages0.html"),
            encode_cp1251(&(m("1/2", 2) + &m("1003", 3))),
        )
        .unwrap();
        let s = scan(&d, &NO).unwrap();
        let u = |n: u32| format!("https://sun9-1.userapi.com/{n}.jpg");
        let get = |n: u32| s.found.iter().find(|f| f.url == u(n)).unwrap();
        assert_eq!(get(1).key, Key::Unknown);
        assert_eq!(get(2).key, Key::Unknown);
        assert_eq!(get(3).key, msg(PEER_A, 1003, 1));
        assert!(crate::export::index::stem(get(1)).starts_with("messages/abc/media/u_"));
        assert!(
            crate::export::index::stem(get(2)).starts_with(&format!("messages/{PEER_A}/media/u_"))
        );
        let peers: Vec<i64> = s.chats.iter().map(|c| c.peer).collect();
        assert_eq!(peers, [PEER_A.parse::<i64>().unwrap()]);
    }

    fn chat_export(name: &str, index: Option<&[u8]>, page: &str) -> Scan {
        let d = tmpdir(name);
        std::fs::create_dir_all(d.join("messages").join(PEER_A)).unwrap();
        std::fs::write(d.join("index.html"), b"x").unwrap();
        if let Some(i) = index {
            std::fs::write(d.join("messages").join("index-messages.html"), i).unwrap();
        }
        std::fs::write(
            d.join("messages").join(PEER_A).join("messages0.html"),
            encode_cp1251(page),
        )
        .unwrap();
        scan(&d, &NO).unwrap()
    }

    fn label_of(index: &[u8]) -> String {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        chat_export(&format!("scan-label-{n}"), Some(index), "x").chats[0]
            .label
            .clone()
    }

    fn counts(s: &Scan) -> (u64, u64, u64) {
        let c = &s.chats[0];
        (c.videos, c.docs, c.photo_links)
    }

    #[test]
    fn trailing_pagination_is_not_a_link() {
        let page = "<div class=\"message\" data-id=\"5\"><div class=\"message__header\">X, 5 мар 2024 в 12:34</div><div class=\"attachment\"><div class=\"attachment__description\">Видеозапись</div></div></div><a href=\"https://vk.ru/x\">1</a><a href=\"https://vk.ru/y\">2</a>";
        assert_eq!(
            counts(&chat_export("scan-pagination", None, page)),
            (0, 0, 0)
        );
    }

    #[test]
    fn video_with_thumbnail_first_still_counted() {
        let page = "<div class=\"message\" data-id=\"5\"><div class=\"attachment\"><div class=\"attachment__description\">Видеозапись</div><a href=\"https://vk.ru/video-1_2\" class=\"attachment__link\">v</a><a class=\"attachment__link\" href=\"https://sun9-1.userapi.com/t.jpg\">t</a></div></div>";
        assert_eq!(
            counts(&chat_export("scan-video-thumb", None, page)),
            (1, 0, 0)
        );
        let page = "<div class=\"message\" data-id=\"5\"><div class=\"attachment\"><div class=\"attachment__description\">Видеозапись</div><a class=\"attachment__link\" href=\"https://sun9-1.userapi.com/t.jpg\">t</a><a class=\"attachment__link\" href=\"https://vk.ru/video-1_2\">v</a></div></div>";
        assert_eq!(
            counts(&chat_export("scan-video-thumb2", None, page)),
            (1, 0, 0)
        );
    }

    #[test]
    fn label_strips_controls_and_bidi() {
        let idx = "<div class=\"message-peer--id\"><a href=\"2000000101/messages0.html\">A&#8238;B&#x200F;C&#10;D&#8234;E</a></div>";
        assert_eq!(label_of(&encode_cp1251(idx)), "ABCDE");
    }

    #[test]
    fn label_without_closing_a_ends_at_next_tag() {
        let idx = "<div class=\"message-peer--id\"><a href=\"2000000101/messages0.html\">Имя<div>Хвост</div>";
        assert_eq!(label_of(&encode_cp1251(idx)), "Имя");
    }

    #[test]
    fn label_is_always_cp1251() {
        // 0xD0 0x90 is valid UTF-8 (U+0410) but cp1251 is wanted
        let mut idx =
            b"<div class=\"message-peer--id\"><a href=\"2000000101/messages0.html\">".to_vec();
        idx.extend([0xD0, 0x90]);
        idx.extend(b"</a></div>");
        assert_eq!(label_of(&idx), "\u{0420}\u{0452}");
    }

    #[test]
    fn label_escaped_markup_stays_text() {
        let idx = "<div class=\"message-peer--id\"><a href=\"2000000101/messages0.html\">&lt;script&gt;x&lt;/script&gt;</a></div>";
        assert_eq!(label_of(&encode_cp1251(idx)), "<script>x</script>");
    }

    #[test]
    fn message_without_data_id_has_no_key() {
        let page = "<div class=\"message\"><div class=\"attachment\"><div class=\"attachment__description\">Фотография</div><a class=\"attachment__link\" href=\"https://sun9-1.userapi.com/1.jpg\">x</a></div></div>";
        let s = chat_export("scan-no-id", None, page);
        assert_eq!(s.found.len(), 1);
        assert!(
            s.found
                .iter()
                .all(|f| !matches!(f.key, Key::Message { .. }))
        );
    }

    #[test]
    fn zero_padded_dir_is_not_a_peer() {
        for dir in ["0123", "-0", "00"] {
            assert_eq!(
                chat_peer(&format!("messages/{dir}/messages0.html")),
                None,
                "{dir}"
            );
        }
        assert_eq!(chat_peer("messages/-123/messages0.html"), Some(-123));
    }

    #[test]
    fn scan_cancellable() {
        let d = fake_export("scan-cancel", FakeExportOpts::default());
        assert_eq!(
            scan(&d, &AtomicBool::new(true)).unwrap_err(),
            Stop::Cancelled
        );
    }

    #[test]
    fn hostile_attachment_block_is_linear() {
        let mut page = [MESSAGE, b"1\"", ATTACHMENT].concat();
        while page.len() < 2 << 20 {
            page.extend_from_slice(b" href='x'");
        }
        assert!(link_tags(&page).is_empty());
        let mut drift = Drift::default();
        let mut chat = Chat {
            peer: 1,
            label: String::new(),
            videos: 0,
            docs: 0,
            photo_links: 0,
        };
        let found = message_found(
            &"messages/1/messages0.html".into(),
            &page,
            &mut drift,
            &mut chat,
        );
        assert!(found.is_empty());
        assert_eq!((chat.videos, chat.docs, chat.photo_links), (0, 0, 0));
        assert_eq!(drift, Drift::default());
    }

    #[test]
    fn link_tags_flag_only_the_anchor() {
        let b =
            b"<div class=\"x\" src=\"a\"><a class=\"attachment__link\" href=\"b\">t</a> href='c'";
        let tags = link_tags(b);
        assert_eq!(
            tags.iter().map(|t| t.1).collect::<Vec<_>>(),
            [false, true, false]
        );
        let at = |pat: &[u8]| find(b, pat).unwrap() + pat.len();
        let v = |pat: &[u8]| at(pat)..at(pat) + 1;
        assert_eq!(attachment_link(b, &tags, &v(b"src=\"")), None);
        assert_eq!(attachment_link(b, &tags, &v(b"href=\"")), Some(1));
        assert_eq!(attachment_link(b, &tags, &v(b"href='")), None);
    }

    #[test]
    fn oversized_page_skipped_and_counted() {
        let root = fake_export("scan-big", FakeExportOpts::default());
        std::fs::File::create(root.join("wall").join("wall9.html"))
            .unwrap()
            .set_len(crate::export::MAX_PAGE + 1)
            .unwrap();
        let s = scan(&root, &NO).unwrap();
        assert_eq!(s.drift.too_large, 1);
        assert!(s.pages.iter().any(|p| p == "wall/wall9.html"));
        let sum = summarize(&s, &|_| false, "r".into(), 0);
        assert_eq!(sum.too_large, 1);
    }

    #[test]
    fn repeated_urls_kept_once_per_page() {
        let root = fake_export("scan-repeat", FakeExportOpts::default());
        let url = "https://sun9-5.userapi.com/impg/x/c1.jpg";
        let page = format!(" href='{url}'").repeat(200_000);
        std::fs::write(root.join("wall").join("wall8.html"), page).unwrap();
        let s = scan(&root, &NO).unwrap();
        assert_eq!(s.found.iter().filter(|f| f.url == url).count(), 1);
        assert!(s.found.len() < 1000);
    }

    #[test]
    fn records_past_cap_refused() {
        let f = |url: &str| Found {
            url: url.into(),
            page: "wall/w.html".into(),
            section: Section::Other,
            key: Key::Unknown,
            kind: Kind::Photo,
            date: None,
            size: None,
        };
        let mut all = Vec::new();
        add_page(&mut all, vec![f("a"), f("a"), f("b")], 2).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(add_page(&mut all, vec![f("c")], 2), Err(Stop::NotExport));
    }
}
