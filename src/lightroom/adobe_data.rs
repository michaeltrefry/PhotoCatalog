//! A data grammar, not a Lua interpreter. Unsupported syntax preserves the input.
use super::{Disposition, Failure, Key, Limits, Property, Value, push};
use std::collections::BTreeSet;
type Parsed<T> = std::result::Result<T, Failure>;

pub(super) fn number(text: &str) -> Parsed<f64> {
    let b = text.as_bytes();
    let mut i = 0;
    if b.first().is_some_and(|c| matches!(c, b'+' | b'-')) {
        i += 1;
    }
    let mut digits = 0;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
        digits += 1;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return Err(Failure::syntax("expected decimal number"));
    }
    if b.get(i).is_some_and(|c| matches!(c, b'e' | b'E')) {
        i += 1;
        if b.get(i).is_some_and(|c| matches!(c, b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if start == i {
            return Err(Failure::syntax("incomplete decimal exponent"));
        }
    }
    if i != b.len() {
        return Err(Failure::syntax("non-decimal numeric expression"));
    }
    let n = text
        .parse::<f64>()
        .map_err(|_| Failure::syntax("invalid number"))?;
    if !n.is_finite() {
        return Err(Failure::syntax("nonfinite or overflowing number"));
    }
    // Underflow cannot silently convert an explicit nonzero value into neutral.
    if n == 0.
        && text
            .split(['e', 'E'])
            .next()
            .unwrap_or("")
            .bytes()
            .any(|c| matches!(c, b'1'..=b'9'))
    {
        return Err(Failure::syntax("numeric underflow"));
    }
    Ok(n)
}
pub(super) enum Root {
    Bare,
    Return,
    Assignment(String),
}
pub(super) struct ParsedData {
    pub properties: Vec<Property>,
    pub root: Root,
}
pub(super) fn parse(bytes: &[u8], limits: Limits) -> Parsed<ParsedData> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| Failure::syntax("catalog data must be UTF-8"))?;
    let mut p = Parser {
        text,
        at: 0,
        tokens: 0,
        limits,
        out: vec![],
    };
    p.space()?;
    let mut path = vec![];
    let mut root = Root::Bare;
    if p.peek() != Some(b'{') {
        let name = p.identifier()?;
        p.space()?;
        if name != "return" {
            p.take(b'=')?;
            root = Root::Assignment(name.clone());
            path.push(Key::Name(name));
        } else {
            root = Root::Return;
        }
    }
    p.space()?;
    if p.peek() != Some(b'{') {
        return Err(Failure::syntax(
            "expected data table, not executable expression",
        ));
    }
    p.value(path, 0)?;
    p.space()?;
    if p.peek() == Some(b';') {
        p.take(b';')?;
        p.space()?;
    }
    if p.at != text.len() {
        return Err(Failure::syntax("trailing executable or unsupported data"));
    }
    Ok(ParsedData {
        properties: p.out,
        root,
    })
}
struct Parser<'a> {
    text: &'a str,
    at: usize,
    tokens: usize,
    limits: Limits,
    out: Vec<Property>,
}
impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }
    fn token(&mut self) -> Parsed<()> {
        self.tokens += 1;
        if self.tokens > self.limits.tokens {
            return Err(Failure::limit("catalog token limit"));
        }
        Ok(())
    }
    fn take(&mut self, c: u8) -> Parsed<()> {
        self.space()?;
        if self.peek() != Some(c) {
            return Err(Failure::syntax("unexpected catalog punctuation"));
        }
        self.at += 1;
        self.token()
    }
    fn space(&mut self) -> Parsed<()> {
        loop {
            while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                self.at += 1;
            }
            if !self.text[self.at..].starts_with("--") {
                return Ok(());
            }
            self.at += 2;
            self.token()?;
            if self.long_level().is_some() {
                self.long_string()?;
            } else {
                while self.peek().is_some_and(|b| b != b'\n' && b != b'\r') {
                    self.at += 1;
                }
            }
        }
    }
    fn identifier(&mut self) -> Parsed<String> {
        self.space()?;
        let start = self.at;
        if !self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        {
            return Err(Failure::syntax("expected data name"));
        }
        self.at += 1;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.at += 1;
        }
        if self.at - start > self.limits.string_bytes {
            return Err(Failure::limit("identifier limit"));
        }
        self.token()?;
        Ok(self.text[start..self.at].into())
    }
    fn long_level(&self) -> Option<usize> {
        let b = self.text.as_bytes();
        if b.get(self.at) != Some(&b'[') {
            return None;
        }
        let mut i = self.at + 1;
        while b.get(i) == Some(&b'=') {
            i += 1;
        }
        if b.get(i) == Some(&b'[') {
            Some(i - self.at - 1)
        } else {
            None
        }
    }
    fn long_string(&mut self) -> Parsed<String> {
        let level = self
            .long_level()
            .ok_or_else(|| Failure::syntax("invalid long string"))?;
        if level > 8 {
            return Err(Failure::limit("long string delimiter limit"));
        }
        self.at += level + 2;
        let mut start = self.at;
        if self.text[start..].starts_with("\r\n") {
            start += 2;
        } else if self.text[start..].starts_with(['\r', '\n']) {
            start += 1;
        }
        let endmark = format!("]{}]", "=".repeat(level));
        let count = self.text[self.at..]
            .find(&endmark)
            .ok_or_else(|| Failure::syntax("unterminated long string/comment"))?;
        let end = self.at + count;
        if end.saturating_sub(start) > self.limits.string_bytes {
            return Err(Failure::limit("long string limit"));
        }
        self.at = end + endmark.len();
        self.token()?;
        Ok(self.text[start.min(end)..end]
            .replace("\r\n", "\n")
            .replace('\r', "\n"))
    }
    fn string(&mut self) -> Parsed<String> {
        self.space()?;
        if self.long_level().is_some() {
            return self.long_string();
        }
        let quote = self
            .peek()
            .filter(|c| matches!(c, b'\'' | b'"'))
            .ok_or_else(|| Failure::syntax("expected quoted string"))?;
        self.at += 1;
        let mut value = vec![];
        loop {
            let c = self
                .peek()
                .ok_or_else(|| Failure::syntax("unterminated string"))?;
            self.at += 1;
            if c == quote {
                break;
            }
            if c == b'\n' || c == b'\r' || c == 0 {
                return Err(Failure::syntax("unescaped newline or NUL in short string"));
            }
            if c != b'\\' {
                value.push(c);
            } else {
                let escaped = self
                    .peek()
                    .ok_or_else(|| Failure::syntax("truncated escape"))?;
                self.at += 1;
                match escaped {
                    b'a' => value.push(7),
                    b'b' => value.push(8),
                    b'f' => value.push(12),
                    b'n' => value.push(b'\n'),
                    b'r' => value.push(b'\r'),
                    b't' => value.push(b'\t'),
                    b'v' => value.push(11),
                    b'\\' | b'\'' | b'"' => value.push(escaped),
                    b'\n' => value.push(b'\n'),
                    b'\r' => {
                        if self.peek() == Some(b'\n') {
                            self.at += 1;
                        }
                        value.push(b'\n');
                    }
                    b'0'..=b'9' => {
                        let mut n = u16::from(escaped - b'0');
                        for _ in 0..2 {
                            if let Some(d) = self.peek().filter(u8::is_ascii_digit) {
                                n = n * 10 + u16::from(d - b'0');
                                self.at += 1;
                            } else {
                                break;
                            }
                        }
                        if n > 255 {
                            return Err(Failure::syntax("decimal byte escape exceeds255"));
                        }
                        value.push(n as u8);
                    }
                    b'x' => {
                        let end = self
                            .at
                            .checked_add(2)
                            .ok_or_else(|| Failure::syntax("escape overflow"))?;
                        let hex = self
                            .text
                            .get(self.at..end)
                            .ok_or_else(|| Failure::syntax("short hex escape"))?;
                        if !hex.bytes().all(|x| x.is_ascii_hexdigit()) {
                            return Err(Failure::syntax("bad hex escape"));
                        }
                        value.push(
                            u8::from_str_radix(hex, 16)
                                .map_err(|_| Failure::syntax("bad hex escape"))?,
                        );
                        self.at = end;
                    }
                    _ => return Err(Failure::syntax("unsupported string escape")),
                }
            }
            if value.len() > self.limits.string_bytes {
                return Err(Failure::limit("decoded string limit"));
            }
        }
        self.token()?;
        String::from_utf8(value)
            .map_err(|_| Failure::syntax("escaped non-UTF8 data remains retained"))
    }
    fn value(&mut self, path: Vec<Key>, depth: usize) -> Parsed<()> {
        if depth > self.limits.depth {
            return Err(Failure::limit("catalog nesting limit"));
        }
        self.space()?;
        let start = self.at;
        let value = match self.peek() {
            Some(b'{') => {
                self.table(path.clone(), depth)?;
                Value::Container
            }
            Some(b'\'' | b'"') => Value::Text(self.string()?),
            Some(b'[') if self.long_level().is_some() => Value::Text(self.long_string()?),
            Some(b'+' | b'-' | b'.' | b'0'..=b'9') => {
                while self
                    .peek()
                    .is_some_and(|c| matches!(c, b'+' | b'-' | b'.' | b'0'..=b'9' | b'e' | b'E'))
                {
                    self.at += 1;
                }
                self.token()?;
                Value::Number(number(&self.text[start..self.at])?)
            }
            Some(c) if c.is_ascii_alphabetic() => match self.identifier()?.as_str() {
                "true" => Value::Boolean(true),
                "false" => Value::Boolean(false),
                "nil" => Value::Null,
                _ => return Err(Failure::syntax("bare identifier/expression is not data")),
            },
            _ => return Err(Failure::syntax("unsupported data value")),
        };
        let name = match path.last() {
            Some(Key::Name(s)) => s.clone(),
            Some(Key::Index(i)) => i.to_string(),
            _ => String::new(),
        };
        push(
            &mut self.out,
            Property {
                path,
                namespace: None,
                name,
                lexical: self.text[start..self.at].into(),
                start,
                end: self.at,
                value,
                disposition: Disposition::RetainedOnly,
                reason: "unknown or unqualified catalog data retained".into(),
            },
            self.limits,
        )
    }
    fn table(&mut self, path: Vec<Key>, depth: usize) -> Parsed<()> {
        self.take(b'{')?;
        let mut keys = BTreeSet::new();
        let mut next = 1;
        loop {
            self.space()?;
            if self.peek() == Some(b'}') {
                self.take(b'}')?;
                return Ok(());
            }
            let mut key = None;
            if self.peek() == Some(b'[') && self.long_level().is_none() {
                self.take(b'[')?;
                self.space()?;
                let k = if matches!(self.peek(), Some(b'\'' | b'"')) {
                    Key::Name(self.string()?)
                } else {
                    let begin = self.at;
                    while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                        self.at += 1;
                    }
                    if begin == self.at {
                        return Err(Failure::syntax(
                            "table key must be string or positive integer",
                        ));
                    }
                    let n = self.text[begin..self.at]
                        .parse::<u64>()
                        .map_err(|_| Failure::syntax("table index overflow"))?;
                    if n == 0 {
                        return Err(Failure::syntax("zero table index unsupported"));
                    }
                    self.token()?;
                    Key::Index(n)
                };
                self.take(b']')?;
                self.take(b'=')?;
                key = Some(k);
            } else if self
                .peek()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
            {
                let before = self.at;
                let name = self.identifier()?;
                self.space()?;
                if self.peek() == Some(b'=') {
                    self.take(b'=')?;
                    key = Some(Key::Name(name));
                } else {
                    self.at = before;
                }
            }
            let k = key.unwrap_or_else(|| {
                let n = next;
                next += 1;
                Key::Index(n)
            });
            if !keys.insert(k.clone()) {
                return Err(Failure::conflict(
                    "duplicate catalog table key; no last-value selection",
                ));
            }
            let mut child = path.clone();
            child.push(k);
            self.value(child, depth + 1)?;
            self.space()?;
            if matches!(self.peek(), Some(b',' | b';')) {
                self.at += 1;
                self.token()?;
            } else if self.peek() != Some(b'}') {
                return Err(Failure::syntax(
                    "table separator required; expressions are forbidden",
                ));
            }
        }
    }
}
