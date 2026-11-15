/// cp1251 0x80-0xBF; 0x98 is unassigned.
const HIGH: [u16; 64] = [
    0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021, 0x20AC, 0x2030, 0x0409, 0x2039,
    0x040A, 0x040C, 0x040B, 0x040F, 0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0xFFFD, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F, 0x00A0, 0x040E, 0x045E, 0x0408,
    0x00A4, 0x0490, 0x00A6, 0x00A7, 0x0401, 0x00A9, 0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407,
    0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7, 0x0451, 0x2116, 0x0454, 0x00BB,
    0x0458, 0x0405, 0x0455, 0x0457,
];

fn cp1251_char(b: u8) -> char {
    match b {
        0x00..=0x7F => b as char,
        0x80..=0xBF => char::from_u32(u32::from(HIGH[usize::from(b - 0x80)])).unwrap(),
        0xC0..=0xFF => char::from_u32(0x0410 + u32::from(b - 0xC0)).unwrap(),
    }
}

pub fn decode_label(bytes: &[u8], utf8: bool) -> String {
    if utf8 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    bytes.iter().map(|&b| cp1251_char(b)).collect()
}

const MAX_ENTITY: usize = 12;

/// HTML attribute value to ASCII text; `None` when it holds or decodes to a non-ASCII char.
pub fn unescape_ascii(raw: &[u8]) -> Option<String> {
    if !raw.is_ascii() {
        return None;
    }
    let out = unescape(std::str::from_utf8(raw).ok()?);
    out.is_ascii().then_some(out)
}

/// Decodes `&amp; &lt; &gt; &quot;` and numeric entities; anything else stays as is.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // longest name is `#x10FFFF`; a `;` further away is not an entity
        let window = &rest.as_bytes()[..rest.len().min(MAX_ENTITY)];
        let entity = window
            .iter()
            .position(|&b| b == b';')
            .map(|end| (&rest[1..end], end + 1));
        let decoded = entity.and_then(|(name, len)| {
            let c = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                _ => {
                    let (digits, radix) = match name.strip_prefix("#x").or(name.strip_prefix("#X"))
                    {
                        Some(hex) => (hex, 16),
                        None => (name.strip_prefix('#')?, 10),
                    };
                    if !digits.starts_with(|c: char| c.is_digit(radix)) {
                        return None;
                    }
                    let n = u32::from_str_radix(digits, radix).ok()?;
                    char::from_u32(n)?
                }
            };
            Some((c, len))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Fixtures only: panics on a char outside ASCII, А–я, Ё, ё.
pub fn encode_cp1251(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| match c {
            '\0'..='\x7F' => c as u8,
            'Ё' => 0xA8,
            'ё' => 0xB8,
            'А'..='я' => (c as u32 - 0x0410 + 0xC0) as u8,
            _ => panic!("no cp1251 byte for {c:?}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp1251_table() {
        assert_eq!(
            encode_cp1251("АЯаяЁё"),
            [0xC0, 0xDF, 0xE0, 0xFF, 0xA8, 0xB8]
        );
        assert_eq!(
            decode_label(&[0x41, 0x80, 0xA9, 0xC0, 0x98], false),
            "A\u{0402}\u{00A9}А\u{FFFD}"
        );
        assert_eq!(
            decode_label(&[0xAB, 0x97, 0xB9, 0xBB, 0x96, 0x85, 0xBF], false),
            "\u{00AB}\u{2014}\u{2116}\u{00BB}\u{2013}\u{2026}\u{0457}"
        );
        let all: Vec<u8> = (0x80..=0xBF).collect();
        assert_eq!(
            decode_label(&all, false)
                .chars()
                .filter(|&c| c == char::REPLACEMENT_CHARACTER)
                .count(),
            1
        );
        assert_eq!(decode_label("Чат".as_bytes(), true), "Чат");
    }

    #[test]
    fn unescape_and_escape() {
        let u = |s: &str| unescape_ascii(s.as_bytes());
        assert_eq!(
            u("a&amp;b&lt;&gt;&quot;&#39;&#65;&#x42;&X;&").as_deref(),
            Some("a&b<>\"'AB&X;&")
        );
        assert_eq!(u("&#1046;"), None);
        assert_eq!(u("&#xFFFFFFFF;").as_deref(), Some("&#xFFFFFFFF;"));
        assert_eq!(unescape_ascii(&[b'a', 0xC0]), None);
        assert_eq!(
            escape_html("<a href='x'>&\"</a>"),
            "&lt;a href=&#39;x&#39;&gt;&amp;&quot;&lt;/a&gt;"
        );
    }

    #[test]
    fn numeric_entities_need_a_digit() {
        let u = |s: &str| unescape_ascii(s.as_bytes()).unwrap();
        assert_eq!(u("&#+65;&#x+41;&#-1;&#;&#x;"), "&#+65;&#x+41;&#-1;&#;&#x;");
        assert_eq!(u("&#x41;&#X4a;&#065;"), "AJA");
    }

    #[test]
    fn many_ampersands_stay_linear() {
        let big = "&".repeat(1 << 20);
        assert_eq!(
            unescape_ascii(big.as_bytes()).as_deref(),
            Some(big.as_str())
        );
        let far = format!("&{}amp;", "a".repeat(1 << 20));
        assert_eq!(
            unescape_ascii(far.as_bytes()).as_deref(),
            Some(far.as_str())
        );
    }

    #[test]
    #[should_panic]
    fn encode_outside_table_panics() {
        encode_cp1251("№");
    }
}
