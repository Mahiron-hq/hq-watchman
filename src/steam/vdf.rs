// SPDX-License-Identifier: GPL-3.0-or-later
//! Text `KeyValues` (VDF) parser for PICS app info.

use anyhow::{anyhow, bail, Result};

const MAX_DEPTH: usize = 32;
const MAX_INPUT: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vdf {
    Str(String),
    Obj(Vec<(String, Vdf)>),
}

impl Vdf {
    pub fn get(&self, key: &str) -> Option<&Vdf> {
        match self {
            Self::Obj(items) => items
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Self::Str(_) => None,
        }
    }

    pub fn path(&self, keys: &[&str]) -> Option<&Vdf> {
        keys.iter().try_fold(self, |node, k| node.get(k))
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            Self::Obj(_) => None,
        }
    }

    pub fn str_at(&self, keys: &[&str]) -> Option<&str> {
        self.path(keys).and_then(Self::str)
    }

    pub fn entries(&self) -> &[(String, Vdf)] {
        match self {
            Self::Obj(items) => items,
            Self::Str(_) => &[],
        }
    }
}

struct Lexer<'a> {
    s: &'a [u8],
    pos: usize,
}

#[derive(Debug, PartialEq)]
enum Token {
    Str(String),
    Open,
    Close,
}

impl Lexer<'_> {
    fn skip_ws(&mut self) {
        loop {
            while self.s.get(self.pos).is_some_and(u8::is_ascii_whitespace) {
                self.pos += 1;
            }
            if self.s.get(self.pos..self.pos + 2) == Some(b"//") {
                while self.s.get(self.pos).is_some_and(|&b| b != b'\n') {
                    self.pos += 1;
                }
            } else {
                return;
            }
        }
    }

    fn next(&mut self) -> Result<Option<Token>> {
        self.skip_ws();
        let Some(&b) = self.s.get(self.pos) else {
            return Ok(None);
        };
        match b {
            b'{' => {
                self.pos += 1;
                Ok(Some(Token::Open))
            }
            b'}' => {
                self.pos += 1;
                Ok(Some(Token::Close))
            }
            b'"' => {
                self.pos += 1;
                let mut out = Vec::new();
                loop {
                    let c = *self
                        .s
                        .get(self.pos)
                        .ok_or_else(|| anyhow!("unterminated string"))?;
                    self.pos += 1;
                    match c {
                        b'"' => break,
                        b'\\' => {
                            let e = *self
                                .s
                                .get(self.pos)
                                .ok_or_else(|| anyhow!("unterminated escape"))?;
                            self.pos += 1;
                            out.push(match e {
                                b'n' => b'\n',
                                b't' => b'\t',
                                other => other,
                            });
                        }
                        other => out.push(other),
                    }
                }
                Ok(Some(Token::Str(String::from_utf8_lossy(&out).into_owned())))
            }
            _ => {
                let start = self.pos;
                while self.s.get(self.pos).is_some_and(|&c| {
                    !c.is_ascii_whitespace() && c != b'{' && c != b'}' && c != b'"'
                }) {
                    self.pos += 1;
                }
                let raw = self.s.get(start..self.pos).unwrap_or_default();
                Ok(Some(Token::Str(String::from_utf8_lossy(raw).into_owned())))
            }
        }
    }
}

fn is_condition(s: &str) -> bool {
    s.starts_with('[') && s.ends_with(']')
}

pub fn parse(input: &str) -> Result<Vdf> {
    if input.len() > MAX_INPUT {
        bail!("VDF too large");
    }
    let mut lx = Lexer {
        s: input.as_bytes(),
        pos: 0,
    };
    let mut stack: Vec<(String, Vec<(String, Vdf)>)> = vec![(String::new(), Vec::new())];
    let mut pending_key: Option<String> = None;
    while let Some(tok) = lx.next()? {
        match tok {
            Token::Str(s) => {
                if pending_key.is_none() {
                    pending_key = Some(s);
                } else if is_condition(&s) {
                } else if let (Some(k), Some((_, items))) = (pending_key.take(), stack.last_mut()) {
                    items.push((k, Vdf::Str(s)));
                }
            }
            Token::Open => {
                let key = pending_key
                    .take()
                    .ok_or_else(|| anyhow!("object without key"))?;
                if stack.len() > MAX_DEPTH {
                    bail!("VDF nested too deep");
                }
                stack.push((key, Vec::new()));
            }
            Token::Close => {
                if pending_key.is_some_and(|k| !is_condition(&k)) {
                    bail!("key without value");
                }
                pending_key = None;
                if stack.len() < 2 {
                    bail!("unbalanced braces");
                }
                let (key, items) = stack.pop().ok_or_else(|| anyhow!("unbalanced braces"))?;
                if let Some((_, parent)) = stack.last_mut() {
                    parent.push((key, Vdf::Obj(items)));
                }
            }
        }
    }
    if stack.len() != 1 || pending_key.is_some() {
        bail!("unexpected end of VDF");
    }
    let (_, items) = stack.pop().ok_or_else(|| anyhow!("empty VDF"))?;
    Ok(Vdf::Obj(items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_nested() {
        let v = parse("\"appinfo\"\n{\n\t\"appid\"\t\t\"10\"\n\t\"common\" { \"name\" \"A \\\"B\\\"\" } // c\n}\n").unwrap();
        assert_eq!(v.str_at(&["appinfo", "appid"]), Some("10"));
        assert_eq!(v.str_at(&["appinfo", "common", "name"]), Some("A \"B\""));
    }

    #[test]
    fn errors() {
        assert!(parse("\"a\" {").is_err());
        assert!(parse("}").is_err());
        assert!(parse("\"a\" { \"b\" }").is_err());
        assert!(parse(&"\"a\" {".repeat(40)).is_err());
    }

    #[test]
    fn real_sample() {
        let v = parse(include_str!("../../tests/data/appinfo_588430.vdf")).unwrap();
        assert_eq!(
            v.str_at(&["appinfo", "common", "name"]),
            Some("Fallout Shelter")
        );
        assert!(v
            .str_at(&["appinfo", "depots", "branches", "public", "buildid"])
            .is_some());
    }

    proptest! {
        #[test]
        fn never_panics(s in any::<String>()) {
            let _ = parse(&s);
        }
    }
}
