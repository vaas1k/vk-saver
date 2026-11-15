pub fn is_reserved(component: &str) -> bool {
    if component.ends_with(['.', ' ']) {
        return true;
    }
    let base = component
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    match base.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        b => {
            let mut digit = b
                .strip_prefix("COM")
                .or_else(|| b.strip_prefix("LPT"))
                .unwrap_or("")
                .chars();
            matches!(
                (digit.next(), digit.next()),
                (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_names() {
        for r in [
            "CON",
            "nul.txt",
            "Com1 .x",
            "lpt9",
            "aux ",
            "x.",
            "x ",
            "a.b.",
            "com0",
            "LPT0.txt",
            "COM\u{b9}",
            "lpt\u{b2}.log",
            "Com\u{b3}",
            "CONIN$",
            "conout$.txt",
        ] {
            assert!(is_reserved(r), "{r}");
        }
        for ok in [
            "CONS",
            "LPT10",
            "_nul.txt",
            "trip",
            "",
            "COM\u{b4}",
            "CONIN",
            "COM\u{b9}\u{b9}",
        ] {
            assert!(!is_reserved(ok), "{ok}");
        }
    }
}
