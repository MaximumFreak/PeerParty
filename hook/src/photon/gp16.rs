//! GpBinary v1.6 ("Protocol16"): big-endian, ASCII type codes.
//!
//! Port of Luxon's `src/ser/ser_gp_binary_v16.cpp` (BSD-3-Clause, see LICENSE-Luxon),
//! with the wire details checked against the behaviour of the Photon client a game
//! ships (int[] lengths are 32-bit
//! in 'n', int[] goes out as 'y' 'i', jagged arrays as 'y' 'y', typed arrays of
//! hashtables / dictionaries / custom types keep their element type).

use super::value::*;

const NULL: u8 = 42; // '*'
const BOOL: u8 = b'o';
const BYTE: u8 = b'b';
const SHORT: u8 = b'k';
const INT: u8 = b'i';
const LONG: u8 = b'l';
const FLOAT: u8 = b'f';
const DOUBLE: u8 = b'd';
const STR: u8 = b's';
const BYTES: u8 = b'x';
const INTS: u8 = b'n';
const STRS: u8 = b'a';
const ARRAY: u8 = b'y';
const OBJECTS: u8 = b'z';
const TABLE: u8 = b'h';
const DICT: u8 = b'D';
const CUSTOM: u8 = b'c';
const EVENT: u8 = b'e';
const REQUEST: u8 = b'q';
const RESPONSE: u8 = b'p';

fn len16(w: &mut Vec<u8>, n: usize) -> R<()> {
    if n > i16::MAX as usize {
        return Err("v1.6 length over 32767");
    }
    w.extend_from_slice(&(n as u16).to_be_bytes());
    Ok(())
}

fn w_str(w: &mut Vec<u8>, s: &str) -> R<()> {
    len16(w, s.len())?;
    w.extend_from_slice(s.as_bytes());
    Ok(())
}

pub fn write_value(w: &mut Vec<u8>, v: &Value) -> R<()> {
    write_typed(w, v, 0)
}

fn write_typed(w: &mut Vec<u8>, v: &Value, depth: usize) -> R<()> {
    if depth > MAX_DEPTH {
        return Err("value nested too deep");
    }
    match v {
        Value::Null => w.push(NULL),
        Value::Bool(_)
        | Value::Byte(_)
        | Value::Short(_)
        | Value::Int(_)
        | Value::Long(_)
        | Value::Float(_)
        | Value::Double(_)
        | Value::Str(_)
        | Value::Bytes(_)
        | Value::Strs(_)
        | Value::Objects(_)
        | Value::Hashtable(_)
        | Value::Custom(..) => {
            let code = scalar_code(v);
            w.push(code);
            payload(w, code, v, depth)?;
        }
        Value::Dict(ty, entries) => {
            w.push(DICT);
            let (k, val) = header(ty);
            w.push(k);
            w.push(val);
            dict_entries(w, k, val, entries, depth)?;
        }
        Value::Event(e) => {
            w.push(EVENT);
            w.push(e.code);
            write_params(w, &e.params)?;
        }
        Value::Request(q) => {
            w.push(REQUEST);
            w.push(q.code);
            write_params(w, &q.params)?;
        }
        Value::Response(r) => {
            w.push(RESPONSE);
            write_response_body(w, r)?;
        }
        _ => {
            w.push(ARRAY);
            array_payload(w, v, depth)?;
        }
    }
    Ok(())
}

/// The single type code of a value that has one ('y' arrays and dictionaries don't).
fn scalar_code(v: &Value) -> u8 {
    match v {
        Value::Bool(_) => BOOL,
        Value::Byte(_) => BYTE,
        Value::Short(_) => SHORT,
        Value::Int(_) => INT,
        Value::Long(_) => LONG,
        Value::Float(_) => FLOAT,
        Value::Double(_) => DOUBLE,
        Value::Str(_) => STR,
        Value::Bytes(_) => BYTES,
        Value::Strs(_) => STRS,
        Value::Objects(_) => OBJECTS,
        Value::Hashtable(_) => TABLE,
        Value::Custom(..) => CUSTOM,
        _ => 0,
    }
}

/// A value without its type byte, as type `code` (the container named the type).
fn payload(w: &mut Vec<u8>, code: u8, v: &Value, depth: usize) -> R<()> {
    match (code, v) {
        (BOOL, Value::Bool(b)) => w.push(*b as u8),
        (BYTE, Value::Byte(b)) => w.push(*b),
        (SHORT, Value::Short(x)) => w.extend_from_slice(&x.to_be_bytes()),
        (INT, Value::Int(x)) => w.extend_from_slice(&x.to_be_bytes()),
        (LONG, Value::Long(x)) => w.extend_from_slice(&x.to_be_bytes()),
        (FLOAT, Value::Float(x)) => w.extend_from_slice(&x.to_be_bytes()),
        (DOUBLE, Value::Double(x)) => w.extend_from_slice(&x.to_be_bytes()),
        (STR, Value::Str(s)) => w_str(w, s)?,
        (BYTES, Value::Bytes(b)) => {
            w.extend_from_slice(&(b.len() as u32).to_be_bytes());
            w.extend_from_slice(b);
        }
        (INTS, Value::Ints(v)) => {
            w.extend_from_slice(&(v.len() as u32).to_be_bytes());
            for x in v {
                w.extend_from_slice(&x.to_be_bytes());
            }
        }
        (STRS, Value::Strs(v)) => {
            len16(w, v.len())?;
            for s in v {
                w_str(w, s)?;
            }
        }
        (OBJECTS, Value::Objects(v)) => {
            len16(w, v.len())?;
            for x in v {
                write_typed(w, x, depth + 1)?;
            }
        }
        (TABLE, Value::Hashtable(t)) => table(w, t, depth)?,
        (CUSTOM, Value::Custom(c, d)) => {
            w.push(*c);
            len16(w, d.len())?;
            w.extend_from_slice(d);
        }
        _ => return Err("value doesn't match its declared v1.6 type"),
    }
    Ok(())
}

fn table(w: &mut Vec<u8>, t: &Table, depth: usize) -> R<()> {
    len16(w, t.len())?;
    for (k, v) in t {
        write_typed(w, k, depth + 1)?;
        write_typed(w, v, depth + 1)?;
    }
    Ok(())
}

/// The v1.6 type code a dictionary side declares, or 0 (object) when v1.6 can't name
/// the type (its dictionaries don't nest and hold no 'y' arrays).
fn ty_code(t: &Ty) -> u8 {
    match t {
        Ty::Bool => BOOL,
        Ty::Byte => BYTE,
        Ty::Short => SHORT,
        Ty::Int => INT,
        Ty::Long => LONG,
        Ty::Float => FLOAT,
        Ty::Double => DOUBLE,
        Ty::Str => STR,
        Ty::Bytes => BYTES,
        Ty::Ints => INTS,
        Ty::Strs => STRS,
        Ty::Objects => OBJECTS,
        Ty::Hashtable => TABLE,
        Ty::Custom => CUSTOM,
        _ => 0,
    }
}

fn code_ty(c: u8) -> R<Ty> {
    Ok(match c {
        0 | NULL => Ty::Any,
        BOOL => Ty::Bool,
        BYTE => Ty::Byte,
        SHORT => Ty::Short,
        INT => Ty::Int,
        LONG => Ty::Long,
        FLOAT => Ty::Float,
        DOUBLE => Ty::Double,
        STR => Ty::Str,
        BYTES => Ty::Bytes,
        INTS => Ty::Ints,
        STRS => Ty::Strs,
        OBJECTS => Ty::Objects,
        TABLE => Ty::Hashtable,
        CUSTOM => Ty::Custom,
        _ => return Err("v1.6 dictionaries don't nest dictionaries or arrays"),
    })
}

fn header(ty: &DictTy) -> (u8, u8) {
    (ty_code(&ty.key), ty_code(&ty.val))
}

fn dict_entries(w: &mut Vec<u8>, k: u8, v: u8, entries: &[(Value, Value)], depth: usize) -> R<()> {
    len16(w, entries.len())?;
    for (key, val) in entries {
        side(w, k, key, depth)?;
        side(w, v, val, depth)?;
    }
    Ok(())
}

fn side(w: &mut Vec<u8>, code: u8, v: &Value, depth: usize) -> R<()> {
    if code == 0 || code == NULL {
        write_typed(w, v, depth + 1)
    } else {
        payload(w, code, v, depth + 1)
    }
}

/// A 'y' array without the 'y': count, element type, elements.
fn array_payload(w: &mut Vec<u8>, v: &Value, depth: usize) -> R<()> {
    if depth > MAX_DEPTH {
        return Err("value nested too deep");
    }
    macro_rules! prim {
        ($list:expr, $code:expr, $put:expr) => {{
            len16(w, $list.len())?;
            w.push($code);
            for x in $list.iter() {
                $put(w, x);
            }
        }};
    }
    match v {
        Value::Bools(l) => prim!(l, BOOL, |w: &mut Vec<u8>, x: &bool| w.push(*x as u8)),
        Value::Shorts(l) => prim!(l, SHORT, |w: &mut Vec<u8>, x: &i16| w.extend_from_slice(&x.to_be_bytes())),
        Value::Ints(l) => prim!(l, INT, |w: &mut Vec<u8>, x: &i32| w.extend_from_slice(&x.to_be_bytes())),
        Value::Longs(l) => prim!(l, LONG, |w: &mut Vec<u8>, x: &i64| w.extend_from_slice(&x.to_be_bytes())),
        Value::Floats(l) => prim!(l, FLOAT, |w: &mut Vec<u8>, x: &f32| w.extend_from_slice(&x.to_be_bytes())),
        Value::Doubles(l) => prim!(l, DOUBLE, |w: &mut Vec<u8>, x: &f64| w.extend_from_slice(&x.to_be_bytes())),
        Value::Bytes(l) => prim!(l, BYTE, |w: &mut Vec<u8>, x: &u8| w.push(*x)),
        Value::Strs(l) => {
            len16(w, l.len())?;
            w.push(STR);
            for s in l {
                w_str(w, s)?;
            }
        }
        Value::Hashtables(l) => {
            len16(w, l.len())?;
            w.push(TABLE);
            for t in l {
                table(w, t, depth + 1)?;
            }
        }
        Value::Dicts(ty, l) => {
            len16(w, l.len())?;
            w.push(DICT);
            let (k, val) = header(ty);
            w.push(k);
            w.push(val);
            for d in l {
                dict_entries(w, k, val, d, depth + 1)?;
            }
        }
        Value::Customs(code, l) => {
            len16(w, l.len())?;
            w.push(CUSTOM);
            w.push(*code);
            for d in l {
                len16(w, d.len())?;
                w.extend_from_slice(d);
            }
        }
        Value::Jagged(l) => {
            len16(w, l.len())?;
            if !l.is_empty() && l.iter().all(|x| matches!(x, Value::Bytes(_))) {
                w.push(BYTES);
                for x in l {
                    payload(w, BYTES, x, depth + 1)?;
                }
            } else {
                w.push(ARRAY);
                for x in l {
                    array_payload(w, x, depth + 1)?;
                }
            }
        }
        _ => return Err("value can't be a v1.6 'y' array"),
    }
    Ok(())
}

pub fn write_params(w: &mut Vec<u8>, p: &Params) -> R<()> {
    len16(w, p.len())?;
    for (k, v) in p {
        w.push(*k);
        write_typed(w, v, 1)?;
    }
    Ok(())
}

pub fn write_response_body(w: &mut Vec<u8>, r: &Response) -> R<()> {
    w.push(r.code);
    w.extend_from_slice(&r.ret.to_be_bytes());
    match r.debug.as_deref() {
        Some(s) if !s.is_empty() => {
            w.push(STR);
            w_str(w, s)?;
        }
        _ => w.push(NULL),
    }
    write_params(w, &r.params)
}

// ---- reading ----

pub fn read_value(r: &mut Rd) -> R<Value> {
    read_typed(r, 0)
}

fn read_typed(r: &mut Rd, depth: usize) -> R<Value> {
    let tc = r.u8()?;
    read_payload(r, tc, depth)
}

fn count16(r: &mut Rd) -> R<usize> {
    let n = r.u16be()? as i16;
    if n < 0 {
        return Err("negative v1.6 count");
    }
    Ok(n as usize)
}

fn r_str(r: &mut Rd) -> R<String> {
    let n = count16(r)?;
    let b = r.take(n)?;
    String::from_utf8(b.to_vec()).map_err(|_| "string isn't UTF-8")
}

fn cap(n: usize, r: &Rd) -> usize {
    n.min(r.left())
}

fn read_payload(r: &mut Rd, tc: u8, depth: usize) -> R<Value> {
    if depth > MAX_DEPTH {
        return Err("value nested too deep");
    }
    Ok(match tc {
        0 | NULL => Value::Null,
        BOOL => Value::Bool(r.u8()? != 0),
        BYTE => Value::Byte(r.u8()?),
        SHORT => Value::Short(i16::from_be_bytes(r.be()?)),
        INT => Value::Int(i32::from_be_bytes(r.be()?)),
        LONG => Value::Long(i64::from_be_bytes(r.be()?)),
        FLOAT => Value::Float(f32::from_be_bytes(r.be()?)),
        DOUBLE => Value::Double(f64::from_be_bytes(r.be()?)),
        STR => Value::Str(r_str(r)?),
        BYTES => {
            let n = r.u32be()? as usize;
            Value::Bytes(r.take(n)?.to_vec())
        }
        INTS => {
            let n = r.u32be()? as usize;
            if n > r.left() / 4 {
                return Err("truncated");
            }
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(i32::from_be_bytes(r.be()?));
            }
            Value::Ints(v)
        }
        STRS => {
            let n = count16(r)?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(r_str(r)?);
            }
            Value::Strs(v)
        }
        OBJECTS => {
            let n = count16(r)?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(read_typed(r, depth + 1)?);
            }
            Value::Objects(v)
        }
        TABLE => Value::Hashtable(read_table(r, depth)?),
        DICT => {
            let k = r.u8()?;
            let v = r.u8()?;
            let ty = DictTy { key: code_ty(k)?, val: code_ty(v)? };
            Value::Dict(ty, read_entries(r, k, v, depth)?)
        }
        CUSTOM => {
            let code = r.u8()?;
            let n = count16(r)?;
            Value::Custom(code, r.take(n)?.to_vec())
        }
        ARRAY => read_array(r, depth)?,
        EVENT => {
            let code = r.u8()?;
            Value::Event(Box::new(EventMsg { code, params: read_params(r)? }))
        }
        REQUEST => {
            let code = r.u8()?;
            Value::Request(Box::new(Request { code, params: read_params(r)? }))
        }
        RESPONSE => Value::Response(Box::new(read_response_body(r)?)),
        _ => return Err("unknown v1.6 type code"),
    })
}

fn read_table(r: &mut Rd, depth: usize) -> R<Table> {
    let n = count16(r)?;
    let mut t = Vec::with_capacity(cap(n, r));
    for _ in 0..n {
        let k = read_typed(r, depth + 1)?;
        let v = read_typed(r, depth + 1)?;
        // The client drops null keys too.
        if !k.is_null() {
            table_set(&mut t, k, v);
        }
    }
    Ok(t)
}

fn read_entries(r: &mut Rd, k: u8, v: u8, depth: usize) -> R<Vec<(Value, Value)>> {
    let n = count16(r)?;
    let mut out = Vec::with_capacity(cap(n, r));
    for _ in 0..n {
        let key = read_side(r, k, depth)?;
        let val = read_side(r, v, depth)?;
        if !key.is_null() {
            out.push((key, val));
        }
    }
    Ok(out)
}

fn read_side(r: &mut Rd, code: u8, depth: usize) -> R<Value> {
    if code == 0 || code == NULL {
        read_typed(r, depth + 1)
    } else {
        read_payload(r, code, depth + 1)
    }
}

/// A 'y' array after its 'y'.
fn read_array(r: &mut Rd, depth: usize) -> R<Value> {
    if depth > MAX_DEPTH {
        return Err("value nested too deep");
    }
    let n = count16(r)?;
    let tc = r.u8()?;
    macro_rules! prim {
        ($variant:ident, $read:expr) => {{
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push($read(&mut *r)?);
            }
            Value::$variant(v)
        }};
    }
    Ok(match tc {
        BOOL => prim!(Bools, |r: &mut Rd| r.u8().map(|b| b != 0)),
        BYTE => Value::Bytes(r.take(n)?.to_vec()),
        SHORT => prim!(Shorts, |r: &mut Rd| r.be().map(i16::from_be_bytes)),
        INT => prim!(Ints, |r: &mut Rd| r.be().map(i32::from_be_bytes)),
        LONG => prim!(Longs, |r: &mut Rd| r.be().map(i64::from_be_bytes)),
        FLOAT => prim!(Floats, |r: &mut Rd| r.be().map(f32::from_be_bytes)),
        DOUBLE => prim!(Doubles, |r: &mut Rd| r.be().map(f64::from_be_bytes)),
        STR => prim!(Strs, r_str),
        TABLE => {
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(read_table(r, depth + 1)?);
            }
            Value::Hashtables(v)
        }
        DICT => {
            let k = r.u8()?;
            let val = r.u8()?;
            let ty = DictTy { key: code_ty(k)?, val: code_ty(val)? };
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(read_entries(r, k, val, depth + 1)?);
            }
            Value::Dicts(ty, v)
        }
        CUSTOM => {
            let code = r.u8()?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                let len = count16(r)?;
                v.push(r.take(len)?.to_vec());
            }
            Value::Customs(code, v)
        }
        ARRAY => {
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(read_array(r, depth + 1)?);
            }
            Value::Jagged(v)
        }
        BYTES | INTS | STRS | OBJECTS => {
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(read_payload(r, tc, depth + 1)?);
            }
            Value::Jagged(v)
        }
        _ => return Err("unsupported v1.6 array element type"),
    })
}

pub fn read_params(r: &mut Rd) -> R<Params> {
    let n = count16(r)?;
    let mut p = Vec::with_capacity(cap(n, r));
    for _ in 0..n {
        let k = r.u8()?;
        let v = read_typed(r, 1)?;
        set_param(&mut p, k, v);
    }
    Ok(p)
}

pub fn read_response_body(r: &mut Rd) -> R<Response> {
    let code = r.u8()?;
    let ret = i16::from_be_bytes(r.be()?);
    let debug = match read_typed(r, 1)? {
        Value::Str(s) => Some(s),
        _ => None,
    };
    Ok(Response { code, ret, debug, params: read_params(r)? })
}

/// A disconnect message: code, debug message, parameters (no operation code).
pub fn read_disconnect_body(r: &mut Rd) -> R<Response> {
    let ret = i16::from_be_bytes(r.be()?);
    let debug = match read_typed(r, 1)? {
        Value::Str(s) => Some(s),
        _ => None,
    };
    Ok(Response { code: 0, ret, debug, params: read_params(r)? })
}

pub fn write_disconnect_body(w: &mut Vec<u8>, d: &Response) -> R<()> {
    w.extend_from_slice(&d.ret.to_be_bytes());
    match d.debug.as_deref() {
        Some(s) if !s.is_empty() => {
            w.push(STR);
            w_str(w, s)?;
        }
        _ => w.push(NULL),
    }
    write_params(w, &d.params)
}
