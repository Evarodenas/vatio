//! Making text that came from the network safe to show.

/// Remove control characters (which could carry terminal escape sequences) and
/// invisible formatting characters such as bidirectional overrides, then trim.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|&c| !c.is_control() && !is_invisible_format(c))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// [`clean`], capped at 200 characters: for fragments quoted from an upstream answer.
pub fn clip(text: &str) -> String {
    clean(text).chars().take(200).collect()
}

fn is_invisible_format(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_invisible_overrides_are_removed() {
        assert_eq!(clean("  Eólica\u{1b}[31m\r\n"), "Eólica[31m");
        assert_eq!(clean("\u{1b}]0;title\u{07}ok"), "]0;titleok");
        assert_eq!(clean("abc\u{202E}def"), "abcdef");
        assert_eq!(clip(&"x".repeat(500)).len(), 200);
    }
}
