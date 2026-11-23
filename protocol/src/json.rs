//! Just enough JSON for the pipe between the launcher and the hook and for the services'
//! REST bodies: parse a request, build a response.
//!
//! Integers stay integers: a literal without `.`, `e` or `E` reads as `U64` (or `Int` when
//! negative), so 64-bit ids (SteamIDs, XUIDs, invite ids) come back exactly, where an
//! `f64` keeps only 53 bits. `Int` and `U64` of the same value are equal, and each
//! accessor takes either.

use std::fmt::Write as _;

#[derive(Clone, Debug)]
pub enum Json {
    Null,
    Bool(bool),
    /// A negative integer (a non-negative one may be here too when built by hand).
    Int(i64),
    /// A non-negative integer.
    U64(u64),
    /// A number with a fraction or an exponent, or an integer too big for 64 bits.
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    /// Keys keep their order; a key found twice is read as its first.
    Obj(Vec<(String, Json)>),
}

impl PartialEq for Json {
    fn eq(&self, other: &Json) -> bool {
        use Json::*;
        match (self, other) {
            (Null, Null) => true,
            (Bool(a), Bool(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            (U64(a), U64(b)) => a == b,
            (Int(a), U64(b)) | (U64(b), Int(a)) => u64::try_from(*a).is_ok_and(|a| a == *b),
            (Num(a), Num(b)) => a == b,
            // A whole float is written as an integer and reads back as one: the same
            // number, when the float holds it exactly.
            (Num(a), Int(b)) | (Int(b), Num(a)) => Num(*a).as_i64() == Some(*b),
            (Num(a), U64(b)) | (U64(b), Num(a)) => Num(*a).as_u64() == Some(*b),
            (Str(a), Str(b)) => a == b,
            (Arr(a), Arr(b)) => a == b,
            (Obj(a), Obj(b)) => a == b,
            _ => false,
        }
    }
}

/// The largest integer an `f64` holds exactly (2^53): an integral `Num` beyond it is
/// not trusted as an id.
const EXACT_IN_F64: f64 = 9_007_199_254_740_992.0;

impl Json {
    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    /// An integer: `U64` when it isn't negative, so it compares and prints as parsed.
    pub fn int(n: i64) -> Json {
        u64::try_from(n).map_or(Json::Int(n), Json::U64)
    }

    pub fn u64(n: u64) -> Json {
        Json::U64(n)
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

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// An integer that fits `i64`: `Int`, `U64` up to `i64::MAX`, or a whole `Num` an
    /// `f64` holds exactly (what an older writer sent).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(n) => Some(*n),
            Json::U64(n) => i64::try_from(*n).ok(),
            Json::Num(n) if n.fract() == 0.0 && n.abs() <= EXACT_IN_F64 => Some(*n as i64),
            _ => None,
        }
    }

    /// A non-negative integer: `U64`, a non-negative `Int`, or a whole, non-negative
    /// `Num` an `f64` holds exactly.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::U64(n) => Some(*n),
            Json::Int(n) => u64::try_from(*n).ok(),
            Json::Num(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= EXACT_IN_F64 => Some(*n as u64),
            _ => None,
        }
    }

    /// Any number, integers included (rounded beyond 2^53).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            Json::Int(n) => Some(*n as f64),
            Json::U64(n) => Some(*n as f64),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> &[Json] {
        match self {
            Json::Arr(v) => v,
            _ => &[],
        }
    }

    pub fn as_obj(&self) -> &[(String, Json)] {
        match self {
            Json::Obj(v) => v,
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
            depth: 0,
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
        // JSON has no infinity or NaN (1e999 parses as one): written bare, the file
        // wouldn't read back.
        Json::Null => out.push_str("null"),
        Json::Num(n) if !n.is_finite() => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Json::U64(n) => {
            let _ = write!(out, "{n}");
        }
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

/// Arrays and objects nest at most this deep. The parser calls itself for each level,
/// and it reads what other players send (lobby data, match tickets) on the game's own
/// threads: a few kilobytes of '[' ran the stack out, which ends the game with no error.
pub const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
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
        self.depth += 1;
        let v = if self.depth > MAX_DEPTH { None } else { self.one() };
        self.depth -= 1;
        v
    }

    fn one(&mut self) -> Option<Json> {
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
                                        // A high half followed by something that isn't a
                                        // low half is no character.
                                        let low = self.hex4()?;
                                        code = match low.checked_sub(0xdc00) {
                                            Some(low) if low < 0x400 => 0x10000 + ((code - 0xd800) << 10) + low,
                                            _ => 0xfffd,
                                        };
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
                let text = std::str::from_utf8(&self.s[start..self.i]).ok()?;
                number(text)
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(h, 16).ok()
    }
}

/// A number's text: an integer when it has no fraction or exponent and fits 64 bits,
/// else an `f64`.
fn number(text: &str) -> Option<Json> {
    if !text.contains(['.', 'e', 'E']) {
        if text.starts_with('-') {
            if let Ok(n) = text.parse::<i64>() {
                return Some(Json::Int(n));
            }
        } else if let Ok(n) = text.parse::<u64>() {
            return Some(Json::U64(n));
        }
    }
    text.parse().ok().map(Json::Num)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_service_shapes() {
        let text = r#"{"Creator":{"Entity":{"Id":"AB12","Type":"title_player_account"},"Attributes":{"EscapedDataObject":"{\"PASS\":\"AAAAA\",\"ROLES\":[\"ALLY\"]}"}},"GiveUpAfterSeconds":120,"QueueName":"coop","x":-1.5e2,"t":true,"n":null,"u":"é😀"}"#;
        let v = Json::parse(text).expect("parses");
        assert_eq!(v.get("QueueName").as_str(), Some("coop"));
        assert_eq!(v.get("GiveUpAfterSeconds").as_f64(), Some(120.0));
        assert_eq!(v.get("GiveUpAfterSeconds").as_u64(), Some(120));
        assert_eq!(v.get("x").as_f64(), Some(-150.0));
        assert_eq!(v.get("t").as_bool(), Some(true));
        assert_eq!(v.get("u").as_str(), Some("é😀"));
        let inner = v.get("Creator").get("Attributes").get("EscapedDataObject");
        let data = Json::parse(inner.as_str().unwrap()).unwrap();
        assert_eq!(data.get("PASS").as_str(), Some("AAAAA"));
        assert_eq!(data.get("ROLES").as_arr().len(), 1);
        assert!(v.get("missing").is_null());
        assert_eq!(Json::parse(&v.to_string()), Some(v));
        assert_eq!(Json::parse("{\"a\":1} x"), None);
        // An out-of-range number reads as infinity, and is written as null, not "inf".
        let big = Json::parse("{\"v\":1e999}").unwrap();
        assert_eq!(big.to_string(), "{\"v\":null}");
    }

    /// 64-bit ids come back to the last digit; through an f64 they didn't.
    #[test]
    fn integers_keep_every_bit() {
        let steam_id = 76561198012345679u64;
        assert_ne!(steam_id as f64 as u64, steam_id, "the case an f64 gets wrong");
        let v = Json::parse(&format!("{{\"id\":{steam_id},\"max\":{},\"neg\":{}}}", u64::MAX, i64::MIN)).unwrap();
        assert_eq!(v.get("id"), &Json::U64(steam_id));
        assert_eq!(v.get("id").as_u64(), Some(steam_id));
        assert_eq!(v.get("max").as_u64(), Some(u64::MAX));
        assert_eq!(v.get("max").as_i64(), None, "past i64::MAX");
        assert_eq!(v.get("neg"), &Json::Int(i64::MIN));
        assert_eq!(v.get("neg").as_i64(), Some(i64::MIN));
        assert_eq!(v.get("neg").as_u64(), None);
        assert_eq!(v.to_string(), format!("{{\"id\":{steam_id},\"max\":{},\"neg\":{}}}", u64::MAX, i64::MIN));
        // Built either way, the same value is the same JSON.
        assert_eq!(Json::Int(7), Json::U64(7));
        assert_eq!(Json::int(7), Json::U64(7));
        assert_eq!(Json::int(-7), Json::Int(-7));
        assert_ne!(Json::Int(-1), Json::U64(u64::MAX));
        assert_eq!(Json::Num(-150.0), Json::Int(-150));
        assert_eq!(Json::U64(120), Json::Num(120.0));
        assert_ne!(Json::Num(120.5), Json::U64(120));
        assert_ne!(Json::Num(steam_id as f64), Json::U64(steam_id), "not exact");
        assert_eq!(Json::Int(5).as_u64(), Some(5));
        assert_eq!(Json::U64(5).as_i64(), Some(5));
        // Floats stay floats; a whole one an older writer sent still reads as an id.
        assert_eq!(Json::parse("2.5"), Some(Json::Num(2.5)));
        assert_eq!(Json::parse("1e3").and_then(|v| v.as_u64()), Some(1000));
        assert_eq!(Json::Num(42.0).as_u64(), Some(42));
        assert_eq!(Json::Num(-1.0).as_u64(), None);
        assert_eq!(Json::Num(1.5).as_i64(), None);
        assert_eq!(Json::Num(1e300).as_u64(), None, "not exact");
        // Past 64 bits: a float, not an error.
        assert!(matches!(Json::parse("18446744073709551616"), Some(Json::Num(_))));
        assert!(matches!(Json::parse("-9223372036854775809"), Some(Json::Num(_))));
        assert_eq!(Json::parse("-0"), Some(Json::U64(0)));
        assert_eq!(Json::parse("1-2"), None);
    }

    #[test]
    fn strings_are_escaped_and_unescaped() {
        let s = "quote \" backslash \\ newline \n tab \t bell \u{7} é😀";
        let written = Json::str(s).to_string();
        assert_eq!(written, "\"quote \\\" backslash \\\\ newline \\n tab \\t bell \\u0007 é😀\"");
        assert_eq!(Json::parse(&written), Some(Json::str(s)));
        assert_eq!(Json::parse(r#""é\b\f\/""#), Some(Json::str("é\u{8}\u{c}/")));
    }

    #[test]
    fn deep_nesting_is_refused_not_followed() {
        let nest = |open: &str, close: &str, n: usize| format!("{}1{}", open.repeat(n), close.repeat(n));
        assert!(Json::parse(&nest("[", "]", MAX_DEPTH - 1)).is_some());
        assert!(Json::parse(&nest("{\"a\":", "}", MAX_DEPTH - 1)).is_some());
        // On a small stack: without the limit this ends the test run, as it ended games.
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                for n in [MAX_DEPTH, 1000, 100_000] {
                    assert_eq!(Json::parse(&nest("[", "]", n)), None);
                    assert_eq!(Json::parse(&nest("{\"a\":", "}", n)), None);
                    assert_eq!(Json::parse(&"[".repeat(n)), None);
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn half_a_surrogate_pair_is_no_character() {
        // A string of escapes: each code as a backslash, 'u' and four hex digits.
        let text = |codes: &[&str], tail: &str| format!("\"{}{tail}\"", codes.iter().map(|c| format!("{}u{c}", char::from(92u8))).collect::<String>());
        assert_eq!(Json::parse(&text(&["d83d", "de00"], "")), Some(Json::str(char::from_u32(0x1f600).unwrap().to_string())));
        let lone = char::from_u32(0xfffd).unwrap();
        assert_eq!(Json::parse(&text(&["d83d", "0041"], "")), Some(Json::str(lone.to_string())));
        assert_eq!(Json::parse(&text(&["d83d", "ffff"], "")), Some(Json::str(lone.to_string())));
        assert_eq!(Json::parse(&text(&["d83d"], "x")), Some(Json::str(format!("{lone}x"))));
    }
}
