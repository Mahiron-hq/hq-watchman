// SPDX-License-Identifier: GPL-3.0-or-later
//! Cleaning and escaping of external text.

/// Upper bound for any single external text kept by the bot.
pub const MAX_EXTERNAL_CHARS: usize = 262_144;

const ZWSP: char = '\u{200B}';

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// Drops control and bidi override characters (Trojan Source), keeps `\n` and `\t`,
/// normalizes line ends and caps the length.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_EXTERNAL_CHARS));
    let mut count = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if count >= MAX_EXTERNAL_CHARS {
            break;
        }
        let c = if c == '\r' {
            if chars.peek() == Some(&'\n') {
                continue;
            }
            '\n'
        } else {
            c
        };
        if (c.is_control() && c != '\n' && c != '\t') || is_bidi_control(c) {
            continue;
        }
        out.push(c);
        count += 1;
    }
    out
}

/// Clean single-line value (names, titles).
pub fn clean_line(s: &str, max_chars: usize) -> String {
    let cleaned = clean(s);
    let one: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&one, max_chars)
}

pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => {
            let mut t = s.get(..i).unwrap_or(s).to_owned();
            t.push('…');
            t
        }
        None => s.to_owned(),
    }
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Inserts zero-width spaces so Telegram does not turn links, domains, mentions,
/// hashtags, cashtags and bot commands inside external text into entities.
pub fn defuse_autolinks(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|p| chars.get(p)).copied();
        let next = chars.get(i + 1).copied();
        out.push(c);
        let word_next = next.is_some_and(|n| n.is_alphanumeric() || n == '_');
        let breaks = match c {
            '.' => prev.is_some_and(char::is_alphanumeric) && word_next,
            ':' => next == Some('/'),
            '@' | '#' | '$' => word_next,
            '/' => word_next && prev.is_none_or(|p| !p.is_alphanumeric()),
            _ => false,
        };
        if breaks {
            out.push(ZWSP);
        }
    }
    out
}

/// External text ready for Telegram HTML: escaped and with links defused.
pub fn external_html(s: &str) -> String {
    escape_html(&defuse_autolinks(s))
}

/// Length as Telegram counts it (UTF-16 code units of the visible text).
pub fn telegram_len(visible: &str) -> usize {
    visible.encode_utf16().count()
}

/// Markdown code fence longer than any backtick run inside `s`.
pub fn md_fence(s: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in s.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// External text as a fenced Markdown block that cannot break the document.
pub fn md_block(s: &str) -> String {
    let fence = md_fence(s);
    format!("{fence}text\n{s}\n{fence}")
}

/// Single-line external value for Markdown (headings, list items).
pub fn md_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '#' | '<' | '>' | '|' | '!' | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Escapes line breaks so external strings cannot forge log lines.
pub fn log_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(512));
    for c in s.chars().take(500) {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() || is_bidi_control(c) => out.push('?'),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn clean_strips_controls_and_bidi() {
        assert_eq!(clean("a\u{202E}b\u{0007}c\r\nd\re\tf"), "abc\nd\ne\tf");
        assert_eq!(clean("\u{2066}x\u{2069}"), "x");
    }

    #[test]
    fn escape() {
        assert_eq!(escape_html("<b>&\"</b>"), "&lt;b&gt;&amp;&quot;&lt;/b&gt;");
    }

    #[test]
    fn defuse() {
        let d = defuse_autolinks("see https://evil.com/x @user #tag /pause $USD v1.2 a/b");
        assert!(!d.contains("https://"));
        assert!(!d.contains("evil.com"));
        assert!(!d.contains("@user"));
        assert!(!d.contains("#tag"));
        assert!(!d.contains("/pause"));
        assert!(!d.contains("$USD"));
        assert!(d.contains("a/b"));
        assert_eq!(
            d.replace(ZWSP, ""),
            "see https://evil.com/x @user #tag /pause $USD v1.2 a/b"
        );
    }

    #[test]
    fn fence_longer_than_content() {
        assert_eq!(md_fence("no ticks"), "```");
        assert_eq!(md_fence("a ```` b"), "`````");
        let block = md_block("```\n# x\n```");
        assert!(block.starts_with("````text\n"));
        assert!(block.ends_with("\n````"));
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_chars("абвгд", 3), "абв…");
        assert_eq!(truncate_chars("аб", 3), "аб");
    }

    #[test]
    fn log_safe_escapes_newlines() {
        assert_eq!(log_safe("a\nINFO fake"), "a\\nINFO fake");
    }

    proptest! {
        #[test]
        fn clean_never_keeps_bad_chars(s in any::<String>()) {
            let c = clean(&s);
            prop_assert!(c.chars().all(|ch| (!ch.is_control() || ch == '\n' || ch == '\t') && !is_bidi_control(ch)));
        }

        #[test]
        fn external_html_has_no_tags(s in any::<String>()) {
            let h = external_html(&clean(&s));
            prop_assert!(!h.contains('<') && !h.contains('>'));
        }

        #[test]
        fn defuse_only_adds_zwsp(s in "[a-z@#/.:$ ]{0,40}") {
            prop_assert_eq!(defuse_autolinks(&s).replace(ZWSP, ""), s);
        }
    }
}
