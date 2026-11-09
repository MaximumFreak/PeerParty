//! Just enough JSON for PlayFab's REST bodies: parse a request, build a response.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    /// Keys keep their order; PlayFab's are unique.
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    pub fn get(&self, key: &str) -> &Json {
        match self {
            Json::Obj(pairs) => pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
                .unwrap_or(&Json::Null),
            _ => &Json::Null,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> &[Json] {
        match self {
            Json::Arr(v) => v,
            _ => &[],
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn parse(text: &str) -> Option<Json> {
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
        };
        let v = p.value()?;
        p.ws();
        (p.i == p.s.len()).then_some(v)
    }
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        write_value(&mut out, self);
        f.write_str(&out)
    }
}

fn write_value(out: &mut String, v: &Json) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => {
            let _ = write!(out, "{}", *n as i64);
        }
        Json::Num(n) => {
            let _ = write!(out, "{n}");
        }
        Json::Str(s) => write_str(out, s),
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Json::Obj(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(out, k);
                out.push(':');
                write_value(out, v);
            }
            out.push('}');
        }
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Option<Json> {
        self.ws();
        match *self.s.get(self.i)? {
            b'{' => {
                self.i += 1;
                let mut pairs = Vec::new();
                self.ws();
                if self.eat("}") {
                    return Some(Json::Obj(pairs));
                }
                loop {
                    self.ws();
                    let Json::Str(k) = self.value()? else {
                        return None;
                    };
                    self.ws();
                    if !self.eat(":") {
                        return None;
                    }
                    let v = self.value()?;
                    pairs.push((k, v));
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    return self.eat("}").then_some(Json::Obj(pairs));
                }
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.eat("]") {
                    return Some(Json::Arr(items));
                }
                loop {
                    items.push(self.value()?);
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    return self.eat("]").then_some(Json::Arr(items));
                }
            }
            b'"' => {
                self.i += 1;
                let mut out = String::new();
                loop {
                    let c = *self.s.get(self.i)?;
                    self.i += 1;
                    match c {
                        b'"' => return Some(Json::Str(out)),
                        b'\\' => {
                            let e = *self.s.get(self.i)?;
                            self.i += 1;
                            match e {
                                b'n' => out.push('\n'),
                                b'r' => out.push('\r'),
                                b't' => out.push('\t'),
                                b'b' => out.push('\u{8}'),
                                b'f' => out.push('\u{c}'),
                                b'u' => {
                                    let mut code = self.hex4()?;
                                    if (0xd800..0xdc00).contains(&code) && self.eat("\\u") {
                                        let low = self.hex4()?;
                                        code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                                    }
                                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                }
                                other => out.push(other as char),
                            }
                        }
                        _ => {
                            // Copy a whole UTF-8 sequence at once.
                            let start = self.i - 1;
                            let mut end = self.i;
                            while end < self.s.len() && (self.s[end] & 0xc0) == 0x80 {
                                end += 1;
                            }
                            out.push_str(std::str::from_utf8(&self.s[start..end]).ok()?);
                            self.i = end;
                        }
                    }
                }
            }
            b't' => self.eat("true").then_some(Json::Bool(true)),
            b'f' => self.eat("false").then_some(Json::Bool(false)),
            b'n' => self.eat("null").then_some(Json::Null),
            _ => {
                let start = self.i;
                while self.i < self.s.len()
                    && matches!(self.s[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    self.i += 1;
                }
                std::str::from_utf8(&self.s[start..self.i])
                    .ok()?
                    .parse()
                    .ok()
                    .map(Json::Num)
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(h, 16).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_playfab_shapes() {
        let text = r#"{"Creator":{"Entity":{"Id":"AB12","Type":"title_player_account"},"Attributes":{"EscapedDataObject":"{\"PASS\":\"AAAAA\",\"ROLES\":[\"ALLY\"]}"}},"GiveUpAfterSeconds":120,"QueueName":"coop","x":-1.5e2,"t":true,"n":null,"u":"é😀"}"#;
        let v = Json::parse(text).expect("parses");
        assert_eq!(v.get("QueueName").as_str(), Some("coop"));
        assert_eq!(v.get("GiveUpAfterSeconds").as_f64(), Some(120.0));
        assert_eq!(v.get("x").as_f64(), Some(-150.0));
        assert_eq!(v.get("u").as_str(), Some("é😀"));
        let inner = v.get("Creator").get("Attributes").get("EscapedDataObject");
        let data = Json::parse(inner.as_str().unwrap()).unwrap();
        assert_eq!(data.get("PASS").as_str(), Some("AAAAA"));
        assert_eq!(data.get("ROLES").as_arr().len(), 1);
        assert!(v.get("missing").is_null());
        assert_eq!(Json::parse(&v.to_string()), Some(v));
        assert_eq!(Json::parse("{\"a\":1} x"), None);
    }
}
