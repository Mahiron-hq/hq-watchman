// SPDX-License-Identifier: GPL-3.0-or-later
//! Parsers for Steam Community HTML. Replace this module if the markup changes.

use scraper::{ElementRef, Html, Node, Selector};

use super::account_to_steam_id;
use crate::text;

pub const MAX_HTML: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedComment {
    pub id: String,
    pub author: String,
    pub author_steamid: u64,
    pub posted_at: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedGuide {
    pub id: u64,
    pub title: String,
    pub game: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("page too large")]
    TooLarge,
    #[error("expected element not found: {0}")]
    Missing(&'static str),
}

fn sel(s: &str) -> Result<Selector, ParseError> {
    Selector::parse(s).map_err(|_| ParseError::Missing("selector"))
}

fn element_text(el: ElementRef<'_>) -> String {
    let mut out = String::new();
    for node in el.descendants() {
        match node.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) if e.name() == "br" => out.push('\n'),
            Node::Element(e) if e.name() == "img" => {
                if let Some(alt) = e.attr("alt") {
                    out.push_str(alt);
                }
            }
            _ => {}
        }
    }
    out
}

/// Parses the `comments_html` fragment of the comment render endpoint.
pub fn parse_comments(html: &str) -> Result<Vec<ParsedComment>, ParseError> {
    if html.len() > MAX_HTML {
        return Err(ParseError::TooLarge);
    }
    let doc = Html::parse_fragment(html);
    let comment_sel = sel("div.commentthread_comment")?;
    let author_sel = sel("a.commentthread_author_link")?;
    let name_sel = sel("bdi")?;
    let time_sel = sel(".commentthread_comment_timestamp[data-timestamp]")?;
    let text_sel = sel(".commentthread_comment_text")?;
    let mut out = Vec::new();
    for c in doc.select(&comment_sel) {
        let id = c
            .value()
            .id()
            .and_then(|i| i.strip_prefix("comment_"))
            .filter(|i| !i.is_empty() && i.len() <= 24 && i.bytes().all(|b| b.is_ascii_digit()))
            .ok_or(ParseError::Missing("comment id"))?
            .to_owned();
        let author = c
            .select(&author_sel)
            .next()
            .ok_or(ParseError::Missing("author link"))?;
        let account: u32 = author
            .value()
            .attr("data-miniprofile")
            .and_then(|m| m.parse().ok())
            .ok_or(ParseError::Missing("author account"))?;
        let name = author
            .select(&name_sel)
            .next()
            .map_or_else(|| element_text(author), element_text);
        let posted_at: i64 = c
            .select(&time_sel)
            .next()
            .and_then(|t| t.value().attr("data-timestamp"))
            .and_then(|t| t.parse().ok())
            .ok_or(ParseError::Missing("timestamp"))?;
        let body = c
            .select(&text_sel)
            .next()
            .ok_or(ParseError::Missing("comment text"))?;
        out.push(ParsedComment {
            id,
            author: text::clean_line(&name, 100),
            author_steamid: account_to_steam_id(account),
            posted_at,
            text: text::clean(element_text(body).trim()),
        });
    }
    Ok(out)
}

/// Parses one page of the public guides list. Returns guides and the total count.
pub fn parse_guides_page(html: &str) -> Result<(Vec<ParsedGuide>, Option<usize>), ParseError> {
    if html.len() > MAX_HTML {
        return Err(ParseError::TooLarge);
    }
    let doc = Html::parse_document(html);
    let item_sel = sel("a[data-publishedfileid]")?;
    let title_sel = sel(".workshopItemTitle")?;
    let app_sel = sel(".workshopItemApp")?;
    let paging_sel = sel(".workshopBrowsePagingInfo")?;
    let mut out = Vec::new();
    for item in doc.select(&item_sel) {
        let id = item
            .value()
            .attr("data-publishedfileid")
            .and_then(|v| v.parse().ok())
            .ok_or(ParseError::Missing("guide id"))?;
        let title = item
            .select(&title_sel)
            .next()
            .map(element_text)
            .ok_or(ParseError::Missing("guide title"))?;
        let game = item
            .select(&app_sel)
            .next()
            .map(element_text)
            .unwrap_or_default();
        out.push(ParsedGuide {
            id,
            title: text::clean_line(&title, 300),
            game: text::clean_line(&game, 200),
        });
    }
    let total = doc
        .select(&paging_sel)
        .next()
        .map(element_text)
        .and_then(|t| {
            t.split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .next_back()
        });
    if out.is_empty() && total.is_none() && !html.contains("no_items") {
        return Err(ParseError::Missing("guide list"));
    }
    Ok((out, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SAMPLE: &str = include_str!("../../tests/data/comments_sample.html");

    #[test]
    fn parses_sample_comments() {
        let c = parse_comments(SAMPLE).unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].id, "500000000000000001");
        assert_eq!(c[0].author, "Test Author");
        assert_eq!(c[0].author_steamid, 76_561_197_960_265_729);
        assert_eq!(c[0].posted_at, 1_782_280_738);
        assert_eq!(
            c[0].text,
            "First line\n\nsecond <b>not bold</b> :steamhappy:"
        );
        assert_eq!(c[1].author, "Второй автор");
        assert!(c[2].text.contains("https://evil.example/x"));
    }

    #[test]
    fn missing_parts_are_errors() {
        let broken = SAMPLE.replace("data-miniprofile", "data-x");
        assert_eq!(
            parse_comments(&broken),
            Err(ParseError::Missing("author account"))
        );
        assert_eq!(parse_comments("").unwrap(), vec![]);
        assert_eq!(
            parse_comments(&"a".repeat(MAX_HTML + 1)),
            Err(ParseError::TooLarge)
        );
    }

    #[test]
    fn parses_guides_page() {
        let (g, total) =
            parse_guides_page(include_str!("../../tests/data/guides_page.html")).unwrap();
        assert_eq!(total, Some(2));
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].id, 1001);
        assert_eq!(g[0].title, "Guide one");
        assert_eq!(g[1].game, "Some Game");
        assert!(parse_guides_page("<html><body>captcha</body></html>").is_err());
    }

    proptest! {
        #[test]
        fn never_panics(s in ".{0,2000}") {
            let _ = parse_comments(&s);
            let _ = parse_guides_page(&s);
        }
    }
}
