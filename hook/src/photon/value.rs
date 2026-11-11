//! Photon's data model: the values GpBinary v1.6 and v1.8 carry, and the operation,
//! response and event messages built from them.
//!
//! Port of Luxon's value types (Luxon `include/luxon/ser_types.hpp`, BSD-3-Clause,
//! see LICENSE-Luxon), widened so values keep the CLR type the client sent: typed
//! arrays of hashtables, dictionaries and custom types, and jagged arrays, keep their
//! element type instead of collapsing to `object[]`, because PUN casts what it gets
//! (`(int[])`, `(Hashtable)`). Checked against the Photon client's behaviour.

/// A Photon hashtable (`ExitGames.Client.Photon.Hashtable`): insertion-ordered pairs.
pub type Table = Vec<(Value, Value)>;

/// An operation's or event's parameters, by byte key, in insertion order.
pub type Params = Vec<(u8, Value)>;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Byte(u8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Str(String),
    Bytes(Vec<u8>),
    Bools(Vec<bool>),
    Shorts(Vec<i16>),
    Ints(Vec<i32>),
    Longs(Vec<i64>),
    Floats(Vec<f32>),
    Doubles(Vec<f64>),
    Strs(Vec<String>),
    /// `object[]`: each element carries its own type.
    Objects(Vec<Value>),
    Hashtable(Table),
    /// `Hashtable[]`.
    Hashtables(Vec<Table>),
    /// `Dictionary<K, V>` with its declared key and value types.
    Dict(DictTy, Vec<(Value, Value)>),
    /// `Dictionary<K, V>[]`.
    Dicts(DictTy, Vec<Vec<(Value, Value)>>),
    /// A registered custom type (PUN: Vector2 'W', Vector3 'V', Quaternion 'Q',
    /// Player 'P'): its code and its serialized bytes.
    Custom(u8, Vec<u8>),
    /// An array of one custom type.
    Customs(u8, Vec<Vec<u8>>),
    /// An array whose elements are arrays (`int[][]`, `byte[][]`, ...).
    Jagged(Vec<Value>),
    /// Messages nested inside a value (rare).
    Request(Box<Request>),
    Response(Box<Response>),
    Event(Box<EventMsg>),
}

/// The declared element type of a dictionary side (or of an array of dictionaries).
#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    /// `object`: each element carries its own type.
    Any,
    Bool,
    Byte,
    Short,
    Int,
    Long,
    Float,
    Double,
    Str,
    Bytes,
    Bools,
    Shorts,
    Ints,
    Longs,
    Floats,
    Doubles,
    Strs,
    Objects,
    Hashtable,
    Hashtables,
    /// A nested dictionary type (v1.8 headers nest; v1.6's don't).
    Dict(Box<DictTy>),
    /// A custom type; each element names its code.
    Custom,
    /// v1.8: an array of arrays of the inner type.
    Jagged(Box<Ty>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct DictTy {
    pub key: Ty,
    pub val: Ty,
}

impl DictTy {
    pub fn any() -> Self {
        DictTy { key: Ty::Any, val: Ty::Any }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Request {
    pub code: u8,
    pub params: Params,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Response {
    pub code: u8,
    pub ret: i16,
    pub debug: Option<String>,
    pub params: Params,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct EventMsg {
    pub code: u8,
    pub params: Params,
}

/// The connection's first message: which protocol and which app.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Init {
    pub proto_major: u8,
    pub proto_minor: u8,
    pub sdk: u8,
    pub version: [u8; 4],
    pub ipv6: bool,
    pub app_id: String,
}

/// One Photon message: `[0xF3][kind | 0x80 if encrypted][payload]`.
#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Init(Init),
    InitResponse,
    Request(Request),
    Response(Response),
    Event(EventMsg),
    InternalRequest(Request),
    InternalResponse(Response),
    /// The server's "you're disconnected, here's why" message.
    Disconnect(Response),
    /// Kinds this server doesn't act on (Message 8, RawMessage 9).
    Other(u8, Vec<u8>),
}

pub mod kind {
    pub const INIT: u8 = 0;
    pub const INIT_RESPONSE: u8 = 1;
    pub const OPERATION: u8 = 2;
    pub const OPERATION_RESPONSE: u8 = 3;
    pub const EVENT: u8 = 4;
    pub const DISCONNECT: u8 = 5;
    pub const INTERNAL_REQUEST: u8 = 6;
    pub const INTERNAL_RESPONSE: u8 = 7;
}

pub type R<T> = Result<T, &'static str>;

/// Values nest at most this deep (Luxon's MAX_DEPTH idea), so a hostile packet can't
/// blow the stack.
pub const MAX_DEPTH: usize = 32;

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Any integer type, widened (Photon clients send ids and flags as byte, short or
    /// int depending on the SDK).
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Byte(v) => Some(v as i64),
            Value::Short(v) => Some(v as i64),
            Value::Int(v) => Some(v as i64),
            Value::Long(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_i32(&self) -> Option<i32> {
        self.as_i64().and_then(|v| i32::try_from(v).ok())
    }

    pub fn as_u8(&self) -> Option<u8> {
        self.as_i64().and_then(|v| u8::try_from(v).ok())
    }

    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Value::Bool(b) => Some(b),
            Value::Byte(b) => Some(b != 0),
            _ => None,
        }
    }

    pub fn as_table(&self) -> Option<&Table> {
        match self {
            Value::Hashtable(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// An int list, from `int[]` or anything array-like of integers.
    pub fn as_ints(&self) -> Option<Vec<i32>> {
        match self {
            Value::Ints(v) => Some(v.clone()),
            Value::Bytes(v) => Some(v.iter().map(|b| *b as i32).collect()),
            Value::Shorts(v) => Some(v.iter().map(|b| *b as i32).collect()),
            Value::Objects(v) => v.iter().map(Value::as_i32).collect(),
            _ => None,
        }
    }

    /// Roughly the memory a value takes, for limits on what clients may have kept.
    pub fn weight(&self) -> usize {
        let params = |p: &Params| p.iter().map(|(_, v)| 1 + v.weight()).sum::<usize>();
        16 + match self {
            Value::Str(s) => s.len(),
            Value::Bytes(b) | Value::Custom(_, b) => b.len(),
            Value::Bools(v) => v.len(),
            Value::Shorts(v) => v.len() * 2,
            Value::Ints(v) => v.len() * 4,
            Value::Longs(v) => v.len() * 8,
            Value::Floats(v) => v.len() * 4,
            Value::Doubles(v) => v.len() * 8,
            Value::Strs(v) => v.iter().map(|s| 24 + s.len()).sum(),
            Value::Customs(_, v) => v.iter().map(|b| 24 + b.len()).sum(),
            Value::Objects(v) | Value::Jagged(v) => v.iter().map(Value::weight).sum(),
            Value::Hashtable(t) | Value::Dict(_, t) => table_weight(t),
            Value::Hashtables(v) | Value::Dicts(_, v) => v.iter().map(|t| 24 + table_weight(t)).sum(),
            Value::Request(q) => params(&q.params),
            Value::Event(e) => params(&e.params),
            Value::Response(r) => params(&r.params) + r.debug.as_ref().map_or(0, String::len),
            _ => 0,
        }
    }

    pub fn as_strs(&self) -> Option<Vec<String>> {
        match self {
            Value::Strs(v) => Some(v.clone()),
            Value::Objects(v) => v.iter().map(|x| x.as_str().map(str::to_owned)).collect(),
            _ => None,
        }
    }
}

/// Photon compares hashtable keys by value and type: byte 255 and int 255 are
/// different keys, as in .NET.
pub fn table_get<'a>(t: &'a Table, key: &Value) -> Option<&'a Value> {
    t.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

pub fn table_weight(t: &[(Value, Value)]) -> usize {
    t.iter().map(|(k, v)| k.weight() + v.weight()).sum()
}

pub fn table_set(t: &mut Table, key: Value, val: Value) {
    match t.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = val,
        None => t.push((key, val)),
    }
}

/// A hashtable being read off the wire: `table_set`'s rule (a key sent twice keeps its
/// place and takes the later value) without its cost, which is the table's length for
/// every key and so the square of it for a table, seconds for one message from anyone
/// who can connect. Past a few entries, keys of the kinds games use are found by hash.
#[derive(Default)]
pub struct TableIn {
    t: Table,
    seen: Option<std::collections::HashMap<Key, usize>>,
    odd: usize,
}

#[derive(PartialEq, Eq, Hash)]
enum Key {
    Bool(bool),
    Byte(u8),
    Short(i16),
    Int(i32),
    Long(i64),
    Str(String),
}

impl Key {
    fn of(v: &Value) -> Option<Key> {
        Some(match v {
            Value::Bool(b) => Key::Bool(*b),
            Value::Byte(b) => Key::Byte(*b),
            Value::Short(n) => Key::Short(*n),
            Value::Int(n) => Key::Int(*n),
            Value::Long(n) => Key::Long(*n),
            Value::Str(s) => Key::Str(s.clone()),
            _ => return None,
        })
    }
}

impl TableIn {
    const LINEAR: usize = 32;
    /// Keys of other kinds (floats, arrays, tables) are still looked for one by one;
    /// a big table may have this many.
    const ODD: usize = 64;

    pub fn with_capacity(n: usize) -> TableIn {
        TableIn { t: Vec::with_capacity(n.min(1024)), seen: None, odd: 0 }
    }

    pub fn set(&mut self, key: Value, val: Value) -> R<()> {
        if self.t.len() < Self::LINEAR {
            table_set(&mut self.t, key, val);
            return Ok(());
        }
        let seen = self.seen.get_or_insert_with(|| self.t.iter().enumerate().filter_map(|(i, (k, _))| Some((Key::of(k)?, i))).collect());
        match Key::of(&key) {
            Some(k) => match seen.entry(k) {
                std::collections::hash_map::Entry::Occupied(e) => self.t[*e.get()].1 = val,
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(self.t.len());
                    self.t.push((key, val));
                }
            },
            None => {
                self.odd += 1;
                if self.odd > Self::ODD {
                    return Err("too many hashtable keys of an unusual type");
                }
                table_set(&mut self.t, key, val);
            }
        }
        Ok(())
    }

    pub fn done(self) -> Table {
        self.t
    }
}

pub fn table_remove(t: &mut Table, key: &Value) {
    t.retain(|(k, _)| k != key);
}

pub fn param<'a>(p: &'a Params, key: u8) -> Option<&'a Value> {
    p.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}

pub fn set_param(p: &mut Params, key: u8, val: Value) {
    match p.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = val,
        None => p.push((key, val)),
    }
}

/// Bounds-checked big- and little-endian reads over a byte slice.
pub struct Rd<'a> {
    pub b: &'a [u8],
    pub p: usize,
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Rd { b, p: 0 }
    }
    pub fn left(&self) -> usize {
        self.b.len() - self.p
    }
    pub fn take(&mut self, n: usize) -> R<&'a [u8]> {
        if self.left() < n {
            return Err("truncated");
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn be<const N: usize>(&mut self) -> R<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    pub fn u16be(&mut self) -> R<u16> {
        Ok(u16::from_be_bytes(self.be()?))
    }
    pub fn u32be(&mut self) -> R<u32> {
        Ok(u32::from_be_bytes(self.be()?))
    }
    pub fn u16le(&mut self) -> R<u16> {
        Ok(u16::from_le_bytes(self.be()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read by hash or one by one, a table comes out the same: first place, last value.
    #[test]
    fn a_table_read_by_hash_is_the_table_set_one() {
        let key = |i: u32| {
            let v = i / 6;
            match i % 6 {
                0 => Value::Str(format!("k{}", v % 90)),
                1 => Value::Byte((v % 50) as u8),
                2 => Value::Int((v % 40) as i32),
                3 => Value::Long((v % 40) as i64),
                4 => Value::Short((v % 30) as i16),
                _ => Value::Bool(v % 2 == 0),
            }
        };
        let (mut plain, mut fast) = (Table::new(), TableIn::with_capacity(0));
        for i in 0..2000u32 {
            let n = i.wrapping_mul(2_654_435_761) >> 7;
            table_set(&mut plain, key(n), Value::Int(i as i32));
            fast.set(key(n), Value::Int(i as i32)).unwrap();
            // A few keys of a kind that isn't hashed, each sent twice.
            if i % 100 == 0 {
                let odd = Value::Float((i / 200) as f32);
                table_set(&mut plain, odd.clone(), Value::Int(i as i32));
                fast.set(odd, Value::Int(i as i32)).unwrap();
            }
        }
        assert!(plain.len() > TableIn::LINEAR * 4);
        assert_eq!(fast.done(), plain);

        // Keys that can't be hashed are few in any real table; a big table of them is
        // refused rather than searched.
        let mut t = TableIn::with_capacity(0);
        let many = (0..10_000).map(|i| t.set(Value::Double(f64::from(i)), Value::Null)).collect::<R<Vec<()>>>();
        assert!(many.is_err());
        // A hundred thousand ordinary keys take no time worth measuring.
        let mut t = TableIn::with_capacity(0);
        for i in 0..100_000 {
            t.set(Value::Int(i), Value::Null).unwrap();
        }
        assert_eq!(t.done().len(), 100_000);
    }
}
