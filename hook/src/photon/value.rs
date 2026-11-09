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

pub fn table_set(t: &mut Table, key: Value, val: Value) {
    match t.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = val,
        None => t.push((key, val)),
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
