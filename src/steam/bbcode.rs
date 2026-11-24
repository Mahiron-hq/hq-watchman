// SPDX-License-Identifier: GPL-3.0-or-later
//! Steam `BBCode` to plain readable text.

use std::fmt::Write as _;

const MAX_TAG: usize = 2048;

const DROP_CONTENT: &[&str] = &["img", "previewyoutube", "video", "spoiler_img"];
const BLOCK: &[&str] = &[
    "h1", "h2", "h3", "h4", "h5", "p", "list", "olist", "table", "tr", "quote", "code", "hr",
];
const KNOWN: &[&str] = &[
    "b",
    "i",
    "u",
    "s",
    "strike",
    "url",
    "spoiler",
    "noparse",
    "th",
    "td",
    "*",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "p",
    "list",
    "olist",
    "table",
    "tr",
    "quote",
    "code",
    "hr",
    "img",
    "previewyoutube",
    "video",
    "spoiler_img",
    "emoticon",
];

struct Tag<'a> {
    name: String,
    closing: bool,
    arg: Option<&'a str>,
}

fn parse_tag(raw: &str) -> Option<Tag<'_>> {
    let (closing, body) = raw.strip_prefix('/').map_or((false, raw), |b| (true, b));
    let (name, arg) = match body.split_once('=') {
        Some((n, a)) => (n, Some(a.trim_matches('"'))),
        None => (body.split_whitespace().next().unwrap_or(body), None),
    };
    let name = name.trim().to_ascii_lowercase();
    KNOWN
        .contains(&name.as_str())
        .then_some(Tag { name, closing, arg })
}

pub fn to_text(input: &str) -> String {
    let src = input.replace("\r\n", "\n");
    let mut out = String::with_capacity(src.len());
    let mut rest = src.as_str();
    let mut drop_depth = 0usize;
    let mut noparse = false;
    let mut olist_counters: Vec<Option<u32>> = Vec::new();
    let mut quote_depth = 0usize;
    let mut url_arg: Option<String> = None;
    let mut url_text_start = 0usize;

    while let Some(open) = rest.find('[') {
        let (before, after) = rest.split_at(open);
        if drop_depth == 0 {
            push_text(&mut out, before, quote_depth);
        }
        let close = after
            .get(1..)
            .and_then(|a| a.find(']'))
            .filter(|&c| c < MAX_TAG);
        let Some(close) = close else {
            if drop_depth == 0 {
                out.push('[');
            }
            rest = after.get(1..).unwrap_or_default();
            continue;
        };
        let raw = after.get(1..=close).unwrap_or_default();
        let next = after.get(close + 2..).unwrap_or_default();
        let tag = if noparse && raw != "/noparse" {
            None
        } else {
            parse_tag(raw)
        };
        let Some(tag) = tag else {
            if drop_depth == 0 {
                out.push('[');
            }
            rest = after.get(1..).unwrap_or_default();
            continue;
        };
        rest = next;
        let name = tag.name.as_str();
        if DROP_CONTENT.contains(&name) {
            if tag.closing {
                drop_depth = drop_depth.saturating_sub(1);
            } else {
                drop_depth += 1;
            }
            continue;
        }
        if drop_depth > 0 {
            continue;
        }
        match (name, tag.closing) {
            ("noparse", c) => noparse = !c,
            ("url", false) => {
                url_arg = tag.arg.map(str::to_owned);
                url_text_start = out.len();
            }
            ("url", true) => {
                if let Some(link) = url_arg.take() {
                    if out
                        .get(url_text_start..)
                        .is_some_and(|t| t.trim().is_empty())
                    {
                        out.push_str(&link);
                    }
                }
            }
            ("*", false) => {
                ensure_newline(&mut out);
                match olist_counters.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        let _ = write!(out, "{n}. ");
                    }
                    _ => out.push_str("• "),
                }
            }
            ("list", false) => olist_counters.push(None),
            ("olist", false) => olist_counters.push(Some(0)),
            ("list" | "olist", true) => {
                olist_counters.pop();
            }
            ("quote", false) => {
                ensure_newline(&mut out);
                quote_depth += 1;
                out.push_str(&"> ".repeat(quote_depth));
                continue;
            }
            ("quote", true) => quote_depth = quote_depth.saturating_sub(1),
            ("hr", false) => {
                ensure_newline(&mut out);
                out.push_str("-----\n");
            }
            ("td" | "th", true) => out.push_str(" | "),
            _ => {}
        }
        if BLOCK.contains(&name) && name != "hr" {
            ensure_newline(&mut out);
        }
    }
    if drop_depth == 0 {
        push_text(&mut out, rest, quote_depth);
    }
    tidy(&out)
}

fn push_text(out: &mut String, text: &str, quote_depth: usize) {
    if quote_depth == 0 {
        out.push_str(text);
        return;
    }
    let prefix = "> ".repeat(quote_depth);
    let mut first = true;
    for line in text.split('\n') {
        if !first {
            out.push('\n');
            out.push_str(&prefix);
        }
        out.push_str(line);
        first = false;
    }
}

fn ensure_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn tidy(s: &str) -> String {
    let mut lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    lines.dedup_by(|a, b| a.is_empty() && b.is_empty());
    let joined = lines.join("\n");
    joined
        .replace("{STEAM_CLAN_IMAGE}", "")
        .replace("{STEAM_CLAN_LOC_IMAGE}", "")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn converts_common_tags() {
        let t = to_text("[h1]Patch 1.2[/h1][p][b]Fixed[/b] crash.[/p][list][*]one[*]two[/list][img]{STEAM_CLAN_IMAGE}/x.png[/img][url=https://a.b/c]site[/url]");
        assert_eq!(t, "Patch 1.2\nFixed crash.\n• one\n• two\nsite");
    }

    #[test]
    fn ordered_list_and_quote() {
        let t = to_text("[olist][*]a[*]b[/olist][quote=x]hi\nthere[/quote]end");
        assert_eq!(t, "1. a\n2. b\n> hi\n> there\nend");
    }

    #[test]
    fn unknown_brackets_kept() {
        assert_eq!(
            to_text("array[0] and [foo]bar[/foo]"),
            "array[0] and [foo]bar[/foo]"
        );
        assert_eq!(to_text("[noparse][b]x[/b][/noparse]"), "[b]x[/b]");
    }

    #[test]
    fn empty_link_text_shows_url() {
        assert_eq!(
            to_text("[url=https://store.steampowered.com/][/url]"),
            "https://store.steampowered.com/"
        );
    }

    proptest! {
        #[test]
        fn never_panics(s in any::<String>()) {
            let _ = to_text(&s);
        }
    }
}
