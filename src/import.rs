//! Bulk cookie import: turn pasted or dropped text into accounts.
//!
//! Accepted line shapes (one account per line, blank lines and `#` comments are
//! skipped):
//!
//! ```text
//! _|WARNING:-DO-NOT-SHARE-THIS...|_ABCDEF...            bare value
//! .ROBLOSECURITY=_|WARNING:...|_ABCDEF...               with the cookie name
//! main alt<TAB>_|WARNING:...|_ABCDEF...                 label, tab, cookie
//! main alt, _|WARNING:...|_ABCDEF...                    label, comma, cookie
//! ```
//!
//! Only `\t`, `,` and `;` separate a label from a cookie: a real cookie contains
//! none of them, but it does contain `:` and `|`, so those cannot be separators.

use crate::roblox::clean_cookie;

/// A real `.ROBLOSECURITY` value is hundreds of characters long. Anything much
/// shorter is almost certainly not one (a name, a stray word, half a line).
pub const MIN_COOKIE_CHARS: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Name given in the line, if any.
    pub label: Option<String>,
    pub cookie: String,
    /// 1-based line number in the source text, for messages.
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub line: usize,
    pub reason: &'static str,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub entries: Vec<Entry>,
    pub skipped: Vec<Skipped>,
    /// Lines whose cookie already appeared earlier in the same text.
    pub repeated: usize,
}

fn looks_like_cookie(value: &str) -> bool {
    value.chars().count() >= MIN_COOKIE_CHARS && !value.chars().any(char::is_whitespace)
}

/// Split one line into an optional label and a cookie candidate.
fn split_line(line: &str) -> (Option<String>, String) {
    if let Some(position) = line.find(['\t', ',', ';']) {
        let (left, right) = (line[..position].trim(), line[position + 1..].trim());
        // The cookie is whichever side looks like one; a label comes first.
        if looks_like_cookie(&clean_cookie(right)) {
            let label = (!left.is_empty()).then(|| left.to_string());
            return (label, clean_cookie(right));
        }
        if looks_like_cookie(&clean_cookie(left)) {
            let label = (!right.is_empty()).then(|| right.to_string());
            return (label, clean_cookie(left));
        }
    }
    (None, clean_cookie(line))
}

/// Parse pasted text. Repeated cookies inside the text are collapsed into the
/// first one; duplicates of accounts already stored are the caller's business.
pub fn parse(text: &str) -> Parsed {
    let mut parsed = Parsed::default();

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        // A BOM at the start of a file would otherwise glue itself to line one.
        let line = raw.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (label, cookie) = split_line(line);
        if !looks_like_cookie(&cookie) {
            parsed.skipped.push(Skipped {
                line: line_no,
                reason: "не похоже на cookie (слишком короткое или с пробелами)",
            });
            continue;
        }
        if parsed.entries.iter().any(|entry| entry.cookie == cookie) {
            parsed.repeated += 1;
            continue;
        }
        parsed.entries.push(Entry {
            label,
            cookie,
            line: line_no,
        });
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like a real cookie, but obviously fake.
    fn fake(seed: char) -> String {
        format!(
            "_|WARNING:-DO-NOT-SHARE-THIS.--Sharing-this-will-allow-someone-to-log-in|_{}",
            seed.to_string().repeat(60)
        )
    }

    #[test]
    fn one_cookie_per_line() {
        let text = format!("{}\n{}\n", fake('A'), fake('B'));
        let parsed = parse(&text);
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].cookie, fake('A'));
        assert_eq!(parsed.entries[1].line, 2);
        assert!(parsed.skipped.is_empty());
    }

    #[test]
    fn cookie_name_prefix_and_quotes_are_stripped() {
        let text = format!(".ROBLOSECURITY={}\n\"{}\"", fake('A'), fake('B'));
        let parsed = parse(&text);
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].cookie, fake('A'));
        assert_eq!(parsed.entries[1].cookie, fake('B'));
    }

    #[test]
    fn labels_come_from_tab_or_comma() {
        let text = format!("main alt\t{}\nfarm, {}\n", fake('A'), fake('B'));
        let parsed = parse(&text);
        assert_eq!(parsed.entries[0].label.as_deref(), Some("main alt"));
        assert_eq!(parsed.entries[0].cookie, fake('A'));
        assert_eq!(parsed.entries[1].label.as_deref(), Some("farm"));
    }

    #[test]
    fn a_colon_inside_the_cookie_is_not_a_separator() {
        // `WARNING:` and `|_` are part of every real cookie.
        let parsed = parse(&fake('A'));
        assert_eq!(parsed.entries.len(), 1);
        assert!(parsed.entries[0].label.is_none());
        assert_eq!(parsed.entries[0].cookie, fake('A'));
    }

    #[test]
    fn junk_is_reported_with_its_line_number() {
        let text = format!("hello\n{}\nshort,cookie\n", fake('A'));
        let parsed = parse(&text);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(
            parsed.skipped.iter().map(|s| s.line).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn blank_lines_comments_and_bom_are_ignored() {
        let text = format!("\u{feff}# my accounts\n\n   \n{}\n", fake('A'));
        let parsed = parse(&text);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].line, 4);
        assert!(parsed.skipped.is_empty());
    }

    #[test]
    fn repeated_cookies_collapse_into_the_first() {
        let text = format!("one\t{}\ntwo\t{}\n{}\n", fake('A'), fake('A'), fake('B'));
        let parsed = parse(&text);
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].label.as_deref(), Some("one"));
        assert_eq!(parsed.repeated, 1);
    }

    #[test]
    fn windows_line_endings_work() {
        let text = format!("{}\r\n{}\r\n", fake('A'), fake('B'));
        assert_eq!(parse(&text).entries.len(), 2);
    }
}
