//! SQL lobby filters (`LobbyType.SqlLobby`): the WHERE clause (and optional ORDER BY) a
//! client sends with JoinRandomGame or GetGameList, over the room properties a game
//! lists in the lobby (`C0` .. `C9` by Photon's convention, any name works here).
//!
//! Photon evaluates these with SQLite; this is the part of SQLite's expression language
//! such filters use: numbers and quoted strings, `NULL`, `AND` / `OR` / `NOT`,
//! comparisons (`=`, `==`, `!=`, `<>`, `<`, `<=`, `>`, `>=`), `[NOT] LIKE` (`%`, `_`,
//! ASCII case ignored), `[NOT] IN (...)`, `[NOT] BETWEEN .. AND ..`, `IS [NOT] NULL`,
//! arithmetic, `ABS`, `MIN`, `MAX`, `LOWER`, `UPPER`, `LENGTH`. As in SQLite a missing
//! property is NULL, a comparison with NULL isn't true, numbers sort before text, and
//! text compares byte by byte.

use super::value::Value;
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq)]
pub enum V {
    Null,
    Num(f64),
    Text(String),
}

impl V {
    fn of(v: &Value) -> V {
        match v {
            Value::Str(s) => V::Text(s.clone()),
            Value::Bool(b) => V::Num(*b as u8 as f64),
            Value::Float(f) => V::Num(*f as f64),
            Value::Double(d) => V::Num(*d),
            Value::Null => V::Null,
            other => other.as_i64().map_or(V::Null, |n| V::Num(n as f64)),
        }
    }

    fn truth(&self) -> Option<bool> {
        match self {
            V::Null => None,
            V::Num(n) => Some(*n != 0.0),
            V::Text(t) => Some(t.trim().parse::<f64>().is_ok_and(|n| n != 0.0)),
        }
    }

    fn num(&self) -> Option<f64> {
        match self {
            V::Num(n) => Some(*n),
            V::Text(t) => t.trim().parse().ok(),
            V::Null => None,
        }
    }

    /// SQLite's ordering: NULL, then numbers, then text.
    fn order(&self, other: &V) -> Ordering {
        match (self, other) {
            (V::Null, V::Null) => Ordering::Equal,
            (V::Null, _) => Ordering::Less,
            (_, V::Null) => Ordering::Greater,
            (V::Num(a), V::Num(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (V::Num(_), V::Text(_)) => Ordering::Less,
            (V::Text(_), V::Num(_)) => Ordering::Greater,
            (V::Text(a), V::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
        }
    }
}

fn bool_v(b: Option<bool>) -> V {
    b.map_or(V::Null, |b| V::Num(b as u8 as f64))
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Word(String),
    Op(&'static str),
}

fn tokens(s: &str) -> Result<Vec<Tok>, String> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || (c == '.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == '.') {
                i += 1;
            }
            let text: String = b[start..i].iter().collect();
            out.push(Tok::Num(text.parse().map_err(|_| format!("bad number {text}"))?));
        } else if c == '\'' || c == '"' {
            let mut text = String::new();
            i += 1;
            loop {
                match b.get(i) {
                    None => return Err("unterminated string".into()),
                    Some(&q) if q == c && b.get(i + 1) == Some(&c) => {
                        text.push(c);
                        i += 2;
                    }
                    Some(&q) if q == c => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        text.push(ch);
                        i += 1;
                    }
                }
            }
            // SQLite reads a double-quoted word as a column name when one exists; Photon's
            // examples use both quotes for text, so they're text here.
            out.push(Tok::Str(text));
        } else if c.is_alphanumeric() || c == '_' || c == '[' || c == '`' {
            let quoted = matches!(c, '[' | '`');
            let close = if c == '[' { ']' } else { '`' };
            if quoted {
                i += 1;
            }
            let start = i;
            while i < b.len() && if quoted { b[i] != close } else { b[i].is_alphanumeric() || b[i] == '_' } {
                i += 1;
            }
            out.push(Tok::Word(b[start..i].iter().collect()));
            if quoted {
                i += 1;
            }
        } else {
            let two: String = b[i..(i + 2).min(b.len())].iter().collect();
            let op = ["<=", ">=", "<>", "!=", "=="].into_iter().find(|o| *o == two);
            if let Some(op) = op {
                out.push(Tok::Op(op));
                i += 2;
            } else {
                let op = ["(", ")", ",", "=", "<", ">", "+", "-", "*", "/", "%"].into_iter().find(|o| o.starts_with(c)).ok_or(format!("unexpected '{c}'"))?;
                out.push(Tok::Op(op));
                i += 1;
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Debug)]
enum Expr {
    Lit(V),
    Col(String),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp(&'static str, Box<Expr>, Box<Expr>),
    Arith(&'static str, Box<Expr>, Box<Expr>),
    Like(Box<Expr>, Box<Expr>, bool),
    In(Box<Expr>, Vec<Expr>, bool),
    Between(Box<Expr>, Box<Expr>, Box<Expr>, bool),
    IsNull(Box<Expr>, bool),
    Call(String, Vec<Expr>),
}

struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w))
    }
    fn eat_word(&mut self, w: &str) -> bool {
        let yes = self.word(w);
        self.i += yes as usize;
        yes
    }
    fn op(&self, o: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(x)) if *x == o)
    }
    fn eat_op(&mut self, o: &str) -> bool {
        let yes = self.op(o);
        self.i += yes as usize;
        yes
    }
    fn expect_op(&mut self, o: &str) -> Result<(), String> {
        if self.eat_op(o) { Ok(()) } else { Err(format!("expected '{o}'")) }
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut e = self.and()?;
        while self.eat_word("OR") {
            e = Expr::Or(Box::new(e), Box::new(self.and()?));
        }
        Ok(e)
    }
    fn and(&mut self) -> Result<Expr, String> {
        let mut e = self.not()?;
        while self.eat_word("AND") {
            e = Expr::And(Box::new(e), Box::new(self.not()?));
        }
        Ok(e)
    }
    fn not(&mut self) -> Result<Expr, String> {
        if self.eat_word("NOT") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.cmp()
    }
    fn cmp(&mut self) -> Result<Expr, String> {
        let left = self.add()?;
        for o in ["=", "==", "!=", "<>", "<", "<=", ">", ">="] {
            if self.eat_op(o) {
                let o = match o {
                    "==" => "=",
                    "<>" => "!=",
                    o => o,
                };
                return Ok(Expr::Cmp(o, Box::new(left), Box::new(self.add()?)));
            }
        }
        if self.eat_word("IS") {
            let negated = self.eat_word("NOT");
            if !self.eat_word("NULL") {
                return Err("expected NULL after IS".into());
            }
            return Ok(Expr::IsNull(Box::new(left), negated));
        }
        let negated = self.eat_word("NOT");
        if self.eat_word("LIKE") {
            return Ok(Expr::Like(Box::new(left), Box::new(self.add()?), negated));
        }
        if self.eat_word("IN") {
            self.expect_op("(")?;
            let mut items = Vec::new();
            if !self.op(")") {
                loop {
                    items.push(self.add()?);
                    if !self.eat_op(",") {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
            return Ok(Expr::In(Box::new(left), items, negated));
        }
        if self.eat_word("BETWEEN") {
            let lo = self.add()?;
            if !self.eat_word("AND") {
                return Err("expected AND in BETWEEN".into());
            }
            let hi = self.add()?;
            return Ok(Expr::Between(Box::new(left), Box::new(lo), Box::new(hi), negated));
        }
        if negated {
            return Err("expected LIKE, IN or BETWEEN after NOT".into());
        }
        Ok(left)
    }
    fn add(&mut self) -> Result<Expr, String> {
        let mut e = self.mul()?;
        loop {
            let o = if self.eat_op("+") { "+" } else if self.eat_op("-") { "-" } else { break };
            e = Expr::Arith(o, Box::new(e), Box::new(self.mul()?));
        }
        Ok(e)
    }
    fn mul(&mut self) -> Result<Expr, String> {
        let mut e = self.unary()?;
        loop {
            let o = if self.eat_op("*") { "*" } else if self.eat_op("/") { "/" } else if self.eat_op("%") { "%" } else { break };
            e = Expr::Arith(o, Box::new(e), Box::new(self.unary()?));
        }
        Ok(e)
    }
    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat_op("-") {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        if self.eat_op("+") {
            return self.unary();
        }
        self.primary()
    }
    fn primary(&mut self) -> Result<Expr, String> {
        let t = self.peek().cloned().ok_or("unexpected end")?;
        self.i += 1;
        match t {
            Tok::Num(n) => Ok(Expr::Lit(V::Num(n))),
            Tok::Str(s) => Ok(Expr::Lit(V::Text(s))),
            Tok::Op("(") => {
                let e = self.or()?;
                self.expect_op(")")?;
                Ok(e)
            }
            Tok::Word(w) if w.eq_ignore_ascii_case("NULL") => Ok(Expr::Lit(V::Null)),
            Tok::Word(w) if w.eq_ignore_ascii_case("TRUE") => Ok(Expr::Lit(V::Num(1.0))),
            Tok::Word(w) if w.eq_ignore_ascii_case("FALSE") => Ok(Expr::Lit(V::Num(0.0))),
            Tok::Word(w) if self.op("(") => {
                self.i += 1;
                let mut args = Vec::new();
                if !self.op(")") {
                    loop {
                        args.push(self.or()?);
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                }
                self.expect_op(")")?;
                Ok(Expr::Call(w.to_ascii_uppercase(), args))
            }
            Tok::Word(w) => Ok(Expr::Col(w)),
            Tok::Op(o) => Err(format!("unexpected '{o}'")),
        }
    }
}

fn like(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    // Iterative wildcard match with backtracking on the last '%'.
    let (mut ti, mut pi, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '%')
}

fn eval(e: &Expr, col: &dyn Fn(&str) -> V) -> V {
    match e {
        Expr::Lit(v) => v.clone(),
        Expr::Col(name) => col(name),
        Expr::Not(x) => bool_v(eval(x, col).truth().map(|b| !b)),
        Expr::Neg(x) => eval(x, col).num().map_or(V::Null, |n| V::Num(-n)),
        // Three-valued logic, as SQL's.
        Expr::And(a, b) => match (eval(a, col).truth(), eval(b, col).truth()) {
            (Some(false), _) | (_, Some(false)) => V::Num(0.0),
            (Some(true), Some(true)) => V::Num(1.0),
            _ => V::Null,
        },
        Expr::Or(a, b) => match (eval(a, col).truth(), eval(b, col).truth()) {
            (Some(true), _) | (_, Some(true)) => V::Num(1.0),
            (Some(false), Some(false)) => V::Num(0.0),
            _ => V::Null,
        },
        Expr::Cmp(o, a, b) => {
            let (a, b) = (eval(a, col), eval(b, col));
            if a == V::Null || b == V::Null {
                return V::Null;
            }
            let ord = a.order(&b);
            bool_v(Some(match *o {
                "=" => ord == Ordering::Equal,
                "!=" => ord != Ordering::Equal,
                "<" => ord == Ordering::Less,
                "<=" => ord != Ordering::Greater,
                ">" => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            }))
        }
        Expr::Arith(o, a, b) => match (eval(a, col).num(), eval(b, col).num()) {
            (Some(x), Some(y)) => match *o {
                "+" => V::Num(x + y),
                "-" => V::Num(x - y),
                "*" => V::Num(x * y),
                "/" if y != 0.0 => V::Num(x / y),
                "%" if y != 0.0 => V::Num(x % y),
                _ => V::Null,
            },
            _ => V::Null,
        },
        Expr::Like(a, p, negated) => match (eval(a, col), eval(p, col)) {
            (V::Null, _) | (_, V::Null) => V::Null,
            (a, p) => bool_v(Some(like(&text(&a), &text(&p)) != *negated)),
        },
        Expr::In(a, items, negated) => {
            let a = eval(a, col);
            if a == V::Null {
                return V::Null;
            }
            let found = items.iter().any(|i| a.order(&eval(i, col)) == Ordering::Equal && eval(i, col) != V::Null);
            bool_v(Some(found != *negated))
        }
        Expr::Between(a, lo, hi, negated) => {
            let (a, lo, hi) = (eval(a, col), eval(lo, col), eval(hi, col));
            if a == V::Null || lo == V::Null || hi == V::Null {
                return V::Null;
            }
            let inside = a.order(&lo) != Ordering::Less && a.order(&hi) != Ordering::Greater;
            bool_v(Some(inside != *negated))
        }
        Expr::IsNull(a, negated) => bool_v(Some((eval(a, col) == V::Null) != *negated)),
        Expr::Call(f, args) => {
            let vals: Vec<V> = args.iter().map(|a| eval(a, col)).collect();
            match (f.as_str(), vals.as_slice()) {
                ("ABS", [x]) => x.num().map_or(V::Null, |n| V::Num(n.abs())),
                ("LOWER", [x]) => if *x == V::Null { V::Null } else { V::Text(text(x).to_ascii_lowercase()) },
                ("UPPER", [x]) => if *x == V::Null { V::Null } else { V::Text(text(x).to_ascii_uppercase()) },
                ("LENGTH", [x]) => if *x == V::Null { V::Null } else { V::Num(text(x).chars().count() as f64) },
                ("MIN", v) if v.len() > 1 => v.iter().cloned().min_by(|a, b| a.order(b)).unwrap_or(V::Null),
                ("MAX", v) if v.len() > 1 => v.iter().cloned().max_by(|a, b| a.order(b)).unwrap_or(V::Null),
                _ => V::Null,
            }
        }
    }
}

fn text(v: &V) -> String {
    match v {
        V::Null => String::new(),
        V::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", *n as i64),
        V::Num(n) => n.to_string(),
        V::Text(t) => t.clone(),
    }
}

/// A parsed filter: its WHERE part and its ORDER BY terms (true = descending).
pub struct Filter {
    filter: Option<Expr>,
    order: Vec<(Expr, bool)>,
}

impl Filter {
    pub fn parse(text: &str) -> Result<Filter, String> {
        let toks = tokens(text)?;
        let mut p = P { t: toks, i: 0 };
        let filter = if p.peek().is_none() || p.word("ORDER") { None } else { Some(p.or()?) };
        let mut order = Vec::new();
        if p.eat_word("ORDER") {
            if !p.eat_word("BY") {
                return Err("expected BY after ORDER".into());
            }
            loop {
                let e = p.add()?;
                let desc = if p.eat_word("DESC") {
                    true
                } else {
                    p.eat_word("ASC");
                    false
                };
                order.push((e, desc));
                if !p.eat_op(",") {
                    break;
                }
            }
        }
        if let Some(t) = p.peek() {
            return Err(format!("unexpected {t:?}"));
        }
        Ok(Filter { filter, order })
    }

    /// Whether a room with these properties passes (`prop` finds one by name).
    pub fn matches(&self, prop: &dyn Fn(&str) -> Option<Value>) -> bool {
        let col = |n: &str| prop(n).map_or(V::Null, |v| V::of(&v));
        self.filter.as_ref().is_none_or(|f| eval(f, &col).truth() == Some(true))
    }

    pub fn has_order(&self) -> bool {
        !self.order.is_empty()
    }

    /// Compares two rooms by the ORDER BY terms.
    pub fn compare(&self, a: &dyn Fn(&str) -> Option<Value>, b: &dyn Fn(&str) -> Option<Value>) -> Ordering {
        let ca = |n: &str| a(n).map_or(V::Null, |v| V::of(&v));
        let cb = |n: &str| b(n).map_or(V::Null, |v| V::of(&v));
        for (e, desc) in &self.order {
            let o = eval(e, &ca).order(&eval(e, &cb));
            let o = if *desc { o.reverse() } else { o };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room<'a>(pairs: &'a [(&'a str, Value)]) -> impl Fn(&str) -> Option<Value> + 'a {
        move |n: &str| pairs.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone())
    }

    #[test]
    fn photons_documented_filters() {
        let r = [("C0", Value::Int(400)), ("C1", Value::Str("Map2".into())), ("C2", Value::Byte(3))];
        let r = room(&r);
        for (f, want) in [
            ("C0 BETWEEN 345 AND 475", true),
            ("C1 = 'Map2'", true),
            ("C1 = \"Map2\"", true),
            ("C0 > 500 OR C1 = 'Map2'", true),
            ("C0 > 500 AND C1 = 'Map2'", false),
            ("NOT (C0 < 100)", true),
            ("C1 LIKE 'map%'", true),
            ("C1 NOT LIKE '%3'", true),
            ("C1 LIKE 'M_p2'", true),
            ("C2 IN (1, 2, 3)", true),
            ("C2 NOT IN (1, 2, 3)", false),
            ("C5 = 1", false),
            ("C5 IS NULL", true),
            ("C0 IS NOT NULL AND C0 % 2 = 0", true),
            ("ABS(C0 - 410) < 20", true),
            ("C0 <> 400", false),
            ("", true),
        ] {
            assert_eq!(Filter::parse(f).unwrap().matches(&r), want, "{f}");
        }
        assert!(Filter::parse("C0 = ").is_err());
        assert!(Filter::parse("C0 = 1 extra").is_err());
    }

    #[test]
    fn order_by_sorts_rooms() {
        let f = Filter::parse("C0 > 0 ORDER BY ABS(C0 - 10), C1 DESC").unwrap();
        assert!(f.has_order());
        let a = [("C0", Value::Int(12)), ("C1", Value::Int(1))];
        let b = [("C0", Value::Int(9)), ("C1", Value::Int(1))];
        let c = [("C0", Value::Int(11)), ("C1", Value::Int(5))];
        assert_eq!(f.compare(&room(&b), &room(&a)), Ordering::Less, "closer to 10 first");
        assert_eq!(f.compare(&room(&c), &room(&b)), Ordering::Less, "equal distance: C1 descending");
        assert!(Filter::parse("ORDER BY C0").unwrap().matches(&room(&a)));
    }

    #[test]
    fn like_wildcards() {
        assert!(like("abc", "a%"));
        assert!(like("abc", "%c"));
        assert!(like("abc", "%b%"));
        assert!(like("aXbXc", "a%b%c"));
        assert!(!like("abc", "a_"));
        assert!(like("ABC", "abc"));
        assert!(like("", "%"));
    }
}
