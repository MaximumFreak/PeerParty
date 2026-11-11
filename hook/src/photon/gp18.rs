//! GpBinary v1.8 ("Protocol18"): little-endian, compressed integers, varint lengths.
//!
//! Port of Luxon's `src/ser/ser_gp_binary_v18.cpp` and its wire specification
//! `doc/gp_binary_v18.md` (BSD-3-Clause, see LICENSE-Luxon), checked against the
//! client's behaviour (dictionary values
//! declared as a dictionary carry their own header; values declared as a jagged array
//! are written typed).

use super::value::*;

const BOOL: u8 = 0x02;
const BYTE: u8 = 0x03;
const SHORT: u8 = 0x04;
const FLOAT: u8 = 0x05;
const DOUBLE: u8 = 0x06;
const STR: u8 = 0x07;
const NULL: u8 = 0x08;
const CINT: u8 = 0x09;
const CLONG: u8 = 0x0A;
const INT1: u8 = 0x0B;
const INT1N: u8 = 0x0C;
const INT2: u8 = 0x0D;
const INT2N: u8 = 0x0E;
const L1: u8 = 0x0F;
const L1N: u8 = 0x10;
const L2: u8 = 0x11;
const L2N: u8 = 0x12;
const CUSTOM: u8 = 0x13;
const DICT: u8 = 0x14;
const TABLE: u8 = 0x15;
const OBJECTS: u8 = 0x17;
const REQUEST: u8 = 0x18;
const RESPONSE: u8 = 0x19;
const EVENT: u8 = 0x1A;
const FALSE: u8 = 0x1B;
const TRUE: u8 = 0x1C;
const SHORT0: u8 = 0x1D;
const INT0: u8 = 0x1E;
const LONG0: u8 = 0x1F;
const FLOAT0: u8 = 0x20;
const DOUBLE0: u8 = 0x21;
const BYTE0: u8 = 0x22;
const ARRAY: u8 = 0x40;
const BOOLS: u8 = 0x42;
const BYTES: u8 = 0x43;
const SHORTS: u8 = 0x44;
const FLOATS: u8 = 0x45;
const DOUBLES: u8 = 0x46;
const STRS: u8 = 0x47;
const INTS: u8 = 0x49;
const LONGS: u8 = 0x4A;
const CUSTOMS: u8 = 0x53;
const DICTS: u8 = 0x54;
const TABLES: u8 = 0x55;
const SLIM: u8 = 0x80;

pub fn varint(w: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            w.push(b);
            return;
        }
        w.push(b | 0x80);
    }
}

fn zz32(n: i32) -> u64 {
    ((n << 1) ^ (n >> 31)) as u32 as u64
}

fn zz64(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

fn w_len(w: &mut Vec<u8>, n: usize) {
    varint(w, n as u64);
}

fn w_str(w: &mut Vec<u8>, s: &str) -> R<()> {
    if s.len() > 32767 {
        return Err("v1.8 string over 32767 bytes");
    }
    w_len(w, s.len());
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
        Value::Bool(b) => w.push(if *b { TRUE } else { FALSE }),
        Value::Byte(0) => w.push(BYTE0),
        Value::Byte(b) => w.extend_from_slice(&[BYTE, *b]),
        Value::Short(0) => w.push(SHORT0),
        Value::Short(x) => {
            w.push(SHORT);
            w.extend_from_slice(&x.to_le_bytes());
        }
        Value::Int(x) => compressed(w, *x as i64, [INT0, INT1, INT1N, INT2, INT2N], || {
            let mut t = vec![CINT];
            varint(&mut t, zz32(*x));
            t
        }),
        Value::Long(x) => compressed(w, *x, [LONG0, L1, L1N, L2, L2N], || {
            let mut t = vec![CLONG];
            varint(&mut t, zz64(*x));
            t
        }),
        Value::Float(x) => {
            w.push(FLOAT);
            w.extend_from_slice(&x.to_le_bytes());
        }
        Value::Double(x) => {
            w.push(DOUBLE);
            w.extend_from_slice(&x.to_le_bytes());
        }
        Value::Custom(code, data) => {
            if *code < 100 {
                w.push(SLIM + code);
            } else {
                w.extend_from_slice(&[CUSTOM, *code]);
            }
            w_len(w, data.len());
            w.extend_from_slice(data);
        }
        Value::Dict(ty, entries) => {
            w.push(DICT);
            let ty = norm(ty);
            header(w, &ty);
            dict_body(w, &ty, entries, depth)?;
        }
        Value::Dicts(ty, list) => {
            w.push(DICTS);
            let ty = norm(ty);
            header(w, &ty);
            w_len(w, list.len());
            for d in list {
                dict_body(w, &ty, d, depth + 1)?;
            }
        }
        Value::Jagged(list) => {
            w.push(ARRAY);
            w_len(w, list.len());
            for x in list {
                write_typed(w, x, depth + 1)?;
            }
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
        Value::Event(e) => {
            w.push(EVENT);
            w.push(e.code);
            write_params(w, &e.params)?;
        }
        _ => {
            let (code, ty) = typed_code(v);
            w.push(code);
            payload(w, &ty, v, depth)?;
        }
    }
    Ok(())
}

/// Ints and longs: zero, one or two magnitude bytes, else the zigzag varint.
fn compressed(w: &mut Vec<u8>, x: i64, codes: [u8; 5], general: impl FnOnce() -> Vec<u8>) {
    let [zero, p1, n1, p2, n2] = codes;
    match x {
        0 => w.push(zero),
        1..=0xFF => w.extend_from_slice(&[p1, x as u8]),
        -0xFF..=-1 => w.extend_from_slice(&[n1, (-x) as u8]),
        0x100..=0xFFFF => {
            w.push(p2);
            w.extend_from_slice(&(x as u16).to_le_bytes());
        }
        -0xFFFF..=-0x100 => {
            w.push(n2);
            w.extend_from_slice(&((-x) as u16).to_le_bytes());
        }
        _ => w.extend_from_slice(&general()),
    }
}

/// Type byte and declared type of values whose typed form is `[code][payload]`.
fn typed_code(v: &Value) -> (u8, Ty) {
    match v {
        Value::Str(_) => (STR, Ty::Str),
        Value::Bytes(_) => (BYTES, Ty::Bytes),
        Value::Bools(_) => (BOOLS, Ty::Bools),
        Value::Shorts(_) => (SHORTS, Ty::Shorts),
        Value::Ints(_) => (INTS, Ty::Ints),
        Value::Longs(_) => (LONGS, Ty::Longs),
        Value::Floats(_) => (FLOATS, Ty::Floats),
        Value::Doubles(_) => (DOUBLES, Ty::Doubles),
        Value::Strs(_) => (STRS, Ty::Strs),
        Value::Objects(_) => (OBJECTS, Ty::Objects),
        Value::Hashtable(_) => (TABLE, Ty::Hashtable),
        Value::Hashtables(_) => (TABLES, Ty::Hashtables),
        Value::Customs(..) => (CUSTOMS, Ty::Custom),
        _ => (NULL, Ty::Any),
    }
}

/// A value without its type byte, as the declared type says.
fn payload(w: &mut Vec<u8>, ty: &Ty, v: &Value, depth: usize) -> R<()> {
    match (ty, v) {
        (Ty::Any, _) | (Ty::Jagged(_), _) => write_typed(w, v, depth + 1)?,
        (Ty::Bool, Value::Bool(b)) => w.push(*b as u8),
        (Ty::Byte, Value::Byte(b)) => w.push(*b),
        (Ty::Short, Value::Short(x)) => w.extend_from_slice(&x.to_le_bytes()),
        (Ty::Int, Value::Int(x)) => varint(w, zz32(*x)),
        (Ty::Long, Value::Long(x)) => varint(w, zz64(*x)),
        (Ty::Float, Value::Float(x)) => w.extend_from_slice(&x.to_le_bytes()),
        (Ty::Double, Value::Double(x)) => w.extend_from_slice(&x.to_le_bytes()),
        (Ty::Str, Value::Str(s)) => w_str(w, s)?,
        (Ty::Custom, Value::Custom(code, data)) => {
            w.push(*code);
            w_len(w, data.len());
            w.extend_from_slice(data);
        }
        (Ty::Bytes, Value::Bytes(b)) => {
            w_len(w, b.len());
            w.extend_from_slice(b);
        }
        (Ty::Bools, Value::Bools(l)) => {
            w_len(w, l.len());
            for chunk in l.chunks(8) {
                let mut byte = 0u8;
                for (i, b) in chunk.iter().enumerate() {
                    if *b {
                        byte |= 1 << i;
                    }
                }
                w.push(byte);
            }
        }
        (Ty::Shorts, Value::Shorts(l)) => {
            w_len(w, l.len());
            l.iter().for_each(|x| w.extend_from_slice(&x.to_le_bytes()));
        }
        (Ty::Floats, Value::Floats(l)) => {
            w_len(w, l.len());
            l.iter().for_each(|x| w.extend_from_slice(&x.to_le_bytes()));
        }
        (Ty::Doubles, Value::Doubles(l)) => {
            w_len(w, l.len());
            l.iter().for_each(|x| w.extend_from_slice(&x.to_le_bytes()));
        }
        (Ty::Ints, Value::Ints(l)) => {
            w_len(w, l.len());
            l.iter().for_each(|x| varint(w, zz32(*x)));
        }
        (Ty::Longs, Value::Longs(l)) => {
            w_len(w, l.len());
            l.iter().for_each(|x| varint(w, zz64(*x)));
        }
        (Ty::Strs, Value::Strs(l)) => {
            w_len(w, l.len());
            for s in l {
                w_str(w, s)?;
            }
        }
        (Ty::Objects, Value::Objects(l)) => {
            w_len(w, l.len());
            for x in l {
                write_typed(w, x, depth + 1)?;
            }
        }
        (Ty::Hashtable, Value::Hashtable(t)) => table(w, t, depth)?,
        (Ty::Hashtables, Value::Hashtables(l)) => {
            w_len(w, l.len());
            for t in l {
                table(w, t, depth + 1)?;
            }
        }
        (Ty::Custom, Value::Customs(code, l)) => {
            w_len(w, l.len());
            w.push(*code);
            for d in l {
                w_len(w, d.len());
                w.extend_from_slice(d);
            }
        }
        // A value declared as a dictionary carries its own header.
        (Ty::Dict(_), Value::Dict(ty, entries)) => {
            let ty = norm(ty);
            header(w, &ty);
            dict_body(w, &ty, entries, depth)?;
        }
        _ => return Err("value doesn't match its declared v1.8 type"),
    }
    Ok(())
}

fn table(w: &mut Vec<u8>, t: &Table, depth: usize) -> R<()> {
    w_len(w, t.len());
    for (k, v) in t {
        write_typed(w, k, depth + 1)?;
        write_typed(w, v, depth + 1)?;
    }
    Ok(())
}

fn dict_body(w: &mut Vec<u8>, ty: &DictTy, entries: &[(Value, Value)], depth: usize) -> R<()> {
    w_len(w, entries.len());
    for (k, v) in entries {
        payload(w, &ty.key, k, depth + 1)?;
        payload(w, &ty.val, v, depth + 1)?;
    }
    Ok(())
}

/// Declared types v1.8 can carry; anything else becomes `object` (typed elements).
fn norm(ty: &DictTy) -> DictTy {
    let key = match ty.key {
        Ty::Bool | Ty::Byte | Ty::Short | Ty::Float | Ty::Double | Ty::Str | Ty::Int | Ty::Long => ty.key.clone(),
        _ => Ty::Any,
    };
    DictTy { key, val: norm_val(&ty.val) }
}

fn norm_val(t: &Ty) -> Ty {
    match t {
        Ty::Dict(d) => Ty::Dict(Box::new(norm(d))),
        Ty::Jagged(inner) => {
            let inner = norm_val(inner);
            if array_code(&inner).is_some() || matches!(inner, Ty::Jagged(_)) {
                Ty::Jagged(Box::new(inner))
            } else {
                Ty::Any
            }
        }
        other => other.clone(),
    }
}

fn scalar_ty_code(t: &Ty) -> Option<u8> {
    Some(match t {
        Ty::Any => 0,
        Ty::Bool => BOOL,
        Ty::Byte => BYTE,
        Ty::Short => SHORT,
        Ty::Float => FLOAT,
        Ty::Double => DOUBLE,
        Ty::Str => STR,
        Ty::Int => CINT,
        Ty::Long => CLONG,
        Ty::Custom => CUSTOM,
        Ty::Hashtable => TABLE,
        _ => return None,
    })
}

fn array_code(t: &Ty) -> Option<u8> {
    Some(match t {
        Ty::Bools => BOOLS,
        Ty::Bytes => BYTES,
        Ty::Shorts => SHORTS,
        Ty::Floats => FLOATS,
        Ty::Doubles => DOUBLES,
        Ty::Strs => STRS,
        Ty::Ints => INTS,
        Ty::Longs => LONGS,
        Ty::Objects => OBJECTS,
        Ty::Hashtables => TABLES,
        _ => return None,
    })
}

fn header(w: &mut Vec<u8>, ty: &DictTy) {
    w.push(scalar_ty_code(&ty.key).unwrap_or(0));
    val_ty(w, &ty.val);
}

fn val_ty(w: &mut Vec<u8>, t: &Ty) {
    match t {
        Ty::Dict(d) => {
            w.push(DICT);
            header(w, d);
        }
        Ty::Jagged(inner) => {
            w.push(ARRAY);
            let mut t = &**inner;
            while let Ty::Jagged(i) = t {
                w.push(ARRAY);
                t = i;
            }
            w.push(array_code(t).unwrap_or(OBJECTS));
        }
        other => w.push(scalar_ty_code(other).or_else(|| array_code(other)).unwrap_or(0)),
    }
}

pub fn write_params(w: &mut Vec<u8>, p: &Params) -> R<()> {
    if p.len() > 255 {
        return Err("more than 255 parameters");
    }
    w.push(p.len() as u8);
    for (k, v) in p {
        w.push(*k);
        write_typed(w, v, 1)?;
    }
    Ok(())
}

pub fn write_response_body(w: &mut Vec<u8>, r: &Response) -> R<()> {
    w.push(r.code);
    w.extend_from_slice(&r.ret.to_le_bytes());
    debug(w, r.debug.as_deref())?;
    write_params(w, &r.params)
}

fn debug(w: &mut Vec<u8>, d: Option<&str>) -> R<()> {
    match d {
        Some(s) if !s.is_empty() => {
            w.push(STR);
            w_str(w, s)
        }
        _ => {
            w.push(NULL);
            Ok(())
        }
    }
}

pub fn write_disconnect_body(w: &mut Vec<u8>, d: &Response) -> R<()> {
    w.extend_from_slice(&d.ret.to_le_bytes());
    debug(w, d.debug.as_deref())?;
    write_params(w, &d.params)
}

// ---- reading ----

pub fn read_varint(r: &mut Rd, max_bytes: usize) -> R<u64> {
    let mut v = 0u64;
    for i in 0..max_bytes {
        let b = r.u8()?;
        v |= ((b & 0x7F) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err("varint too long")
}

fn r_u32(r: &mut Rd) -> R<u32> {
    Ok(read_varint(r, 5)? as u32)
}

fn r_len(r: &mut Rd) -> R<usize> {
    Ok(r_u32(r)? as usize)
}

fn r_int(r: &mut Rd) -> R<i32> {
    let v = r_u32(r)?;
    Ok(((v >> 1) as i32) ^ -((v & 1) as i32))
}

fn r_long(r: &mut Rd) -> R<i64> {
    let v = read_varint(r, 10)?;
    Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
}

fn r_str(r: &mut Rd) -> R<String> {
    let n = r_len(r)?;
    String::from_utf8(r.take(n)?.to_vec()).map_err(|_| "string isn't UTF-8")
}

fn cap(n: usize, r: &Rd) -> usize {
    n.min(r.left())
}

pub fn read_value(r: &mut Rd) -> R<Value> {
    read_typed(r, 0)
}

fn read_typed(r: &mut Rd, depth: usize) -> R<Value> {
    let tc = r.u8()?;
    read_payload(r, tc, depth)
}

fn read_payload(r: &mut Rd, tc: u8, depth: usize) -> R<Value> {
    if depth > MAX_DEPTH {
        return Err("value nested too deep");
    }
    if (0x80..=0xE4).contains(&tc) {
        let n = r_len(r)?;
        return Ok(Value::Custom(tc - SLIM, r.take(n)?.to_vec()));
    }
    Ok(match tc {
        0 | NULL => Value::Null,
        BOOL => Value::Bool(r.u8()? != 0),
        TRUE => Value::Bool(true),
        FALSE => Value::Bool(false),
        BYTE => Value::Byte(r.u8()?),
        BYTE0 => Value::Byte(0),
        SHORT => Value::Short(i16::from_le_bytes(r.be()?)),
        SHORT0 => Value::Short(0),
        FLOAT => Value::Float(f32::from_le_bytes(r.be()?)),
        FLOAT0 => Value::Float(0.0),
        DOUBLE => Value::Double(f64::from_le_bytes(r.be()?)),
        DOUBLE0 => Value::Double(0.0),
        STR => Value::Str(r_str(r)?),
        CINT => Value::Int(r_int(r)?),
        INT0 => Value::Int(0),
        INT1 => Value::Int(r.u8()? as i32),
        INT1N => Value::Int(-(r.u8()? as i32)),
        INT2 => Value::Int(r.u16le()? as i32),
        INT2N => Value::Int(-(r.u16le()? as i32)),
        CLONG => Value::Long(r_long(r)?),
        LONG0 => Value::Long(0),
        L1 => Value::Long(r.u8()? as i64),
        L1N => Value::Long(-(r.u8()? as i64)),
        L2 => Value::Long(r.u16le()? as i64),
        L2N => Value::Long(-(r.u16le()? as i64)),
        CUSTOM => {
            let code = r.u8()?;
            let n = r_len(r)?;
            Value::Custom(code, r.take(n)?.to_vec())
        }
        DICT => {
            let ty = read_header(r)?;
            Value::Dict(ty.clone(), read_dict_body(r, &ty, depth)?)
        }
        TABLE => Value::Hashtable(read_table(r, depth)?),
        OBJECTS => {
            let n = r_len(r)?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(read_typed(r, depth + 1)?);
            }
            Value::Objects(v)
        }
        REQUEST => {
            let code = r.u8()?;
            Value::Request(Box::new(Request { code, params: params_at(r, depth + 1)? }))
        }
        RESPONSE => Value::Response(Box::new(response_at(r, depth + 1)?)),
        EVENT => {
            let code = r.u8()?;
            Value::Event(Box::new(EventMsg { code, params: params_at(r, depth + 1)? }))
        }
        ARRAY => {
            let n = r_len(r)?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(read_typed(r, depth + 1)?);
            }
            Value::Jagged(v)
        }
        BOOLS => {
            let n = r_len(r)?;
            let bytes = r.take(n.div_ceil(8))?;
            Value::Bools((0..n).map(|i| bytes[i / 8] & (1 << (i % 8)) != 0).collect())
        }
        BYTES => {
            let n = r_len(r)?;
            Value::Bytes(r.take(n)?.to_vec())
        }
        SHORTS => {
            let n = r_len(r)?;
            let b = r.take(n.checked_mul(2).ok_or("truncated")?)?;
            Value::Shorts(b.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect())
        }
        FLOATS => {
            let n = r_len(r)?;
            let b = r.take(n.checked_mul(4).ok_or("truncated")?)?;
            Value::Floats(b.chunks(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
        }
        DOUBLES => {
            let n = r_len(r)?;
            let b = r.take(n.checked_mul(8).ok_or("truncated")?)?;
            Value::Doubles(b.chunks(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect())
        }
        STRS => {
            let n = r_len(r)?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(r_str(r)?);
            }
            Value::Strs(v)
        }
        INTS => {
            let n = r_len(r)?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(r_int(r)?);
            }
            Value::Ints(v)
        }
        LONGS => {
            let n = r_len(r)?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(r_long(r)?);
            }
            Value::Longs(v)
        }
        TABLES => {
            let n = r_len(r)?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(read_table(r, depth + 1)?);
            }
            Value::Hashtables(v)
        }
        DICTS => {
            let ty = read_header(r)?;
            let n = r_len(r)?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                v.push(read_dict_body(r, &ty, depth + 1)?);
            }
            Value::Dicts(ty, v)
        }
        CUSTOMS => {
            let n = r_len(r)?;
            let code = r.u8()?;
            let mut v = Vec::with_capacity(cap(n, r));
            for _ in 0..n {
                let len = r_len(r)?;
                v.push(r.take(len)?.to_vec());
            }
            Value::Customs(code, v)
        }
        _ => return Err("unknown v1.8 type code"),
    })
}

fn read_table(r: &mut Rd, depth: usize) -> R<Table> {
    let n = r_len(r)?;
    let mut t = TableIn::with_capacity(cap(n, r));
    for _ in 0..n {
        let k = read_typed(r, depth + 1)?;
        let v = read_typed(r, depth + 1)?;
        if !k.is_null() {
            t.set(k, v)?;
        }
    }
    Ok(t.done())
}

fn key_ty(c: u8) -> R<Ty> {
    Ok(match c {
        0 => Ty::Any,
        BOOL => Ty::Bool,
        BYTE => Ty::Byte,
        SHORT => Ty::Short,
        FLOAT => Ty::Float,
        DOUBLE => Ty::Double,
        STR => Ty::Str,
        CINT => Ty::Int,
        CLONG => Ty::Long,
        _ => return Err("unsupported v1.8 dictionary key type"),
    })
}

fn code_val_ty(c: u8) -> R<Ty> {
    Ok(match c {
        0 => Ty::Any,
        BOOL => Ty::Bool,
        BYTE => Ty::Byte,
        SHORT => Ty::Short,
        FLOAT => Ty::Float,
        DOUBLE => Ty::Double,
        STR => Ty::Str,
        CINT => Ty::Int,
        CLONG => Ty::Long,
        CUSTOM => Ty::Custom,
        TABLE => Ty::Hashtable,
        OBJECTS => Ty::Objects,
        BOOLS => Ty::Bools,
        BYTES => Ty::Bytes,
        SHORTS => Ty::Shorts,
        FLOATS => Ty::Floats,
        DOUBLES => Ty::Doubles,
        STRS => Ty::Strs,
        INTS => Ty::Ints,
        LONGS => Ty::Longs,
        TABLES => Ty::Hashtables,
        _ => return Err("unsupported v1.8 dictionary value type"),
    })
}

fn read_header(r: &mut Rd) -> R<DictTy> {
    let key = key_ty(r.u8()?)?;
    let val = read_val_ty(r, 0)?;
    Ok(DictTy { key, val })
}

fn read_val_ty(r: &mut Rd, depth: usize) -> R<Ty> {
    if depth > MAX_DEPTH {
        return Err("type nested too deep");
    }
    let c = r.u8()?;
    Ok(match c {
        DICT => {
            let key = key_ty(r.u8()?)?;
            let val = read_val_ty(r, depth + 1)?;
            Ty::Dict(Box::new(DictTy { key, val }))
        }
        ARRAY => Ty::Jagged(Box::new(read_val_ty(r, depth + 1)?)),
        _ => code_val_ty(c)?,
    })
}

fn read_dict_body(r: &mut Rd, ty: &DictTy, depth: usize) -> R<Vec<(Value, Value)>> {
    let n = r_len(r)?;
    let mut out = Vec::with_capacity(cap(n, r));
    for _ in 0..n {
        let k = read_declared(r, &ty.key, depth + 1)?;
        let v = read_declared(r, &ty.val, depth + 1)?;
        if !k.is_null() {
            out.push((k, v));
        }
    }
    Ok(out)
}

/// A value without its type byte, as the declared type says.
fn read_declared(r: &mut Rd, ty: &Ty, depth: usize) -> R<Value> {
    let code = match ty {
        Ty::Any | Ty::Jagged(_) => return read_typed(r, depth),
        // A value declared as a dictionary carries its own header.
        Ty::Dict(_) => DICT,
        Ty::Custom => CUSTOM,
        Ty::Int => {
            return Ok(Value::Int(r_int(r)?));
        }
        Ty::Long => {
            return Ok(Value::Long(r_long(r)?));
        }
        other => scalar_ty_code(other).or_else(|| array_code(other)).ok_or("bad declared type")?,
    };
    read_payload(r, code, depth)
}

pub fn read_params(r: &mut Rd) -> R<Params> {
    params_at(r, 1)
}

/// A message's parameters, `depth` values down. A request, response or event can sit
/// inside a value, so its parameters count towards the same limit as any other nesting:
/// started again at 1 each time, messages inside messages nested until the stack ran out.
fn params_at(r: &mut Rd, depth: usize) -> R<Params> {
    let n = r.u8()? as usize;
    let mut p = Vec::with_capacity(n);
    for _ in 0..n {
        let k = r.u8()?;
        let v = read_typed(r, depth)?;
        set_param(&mut p, k, v);
    }
    Ok(p)
}

pub fn read_response_body(r: &mut Rd) -> R<Response> {
    response_at(r, 1)
}

fn response_at(r: &mut Rd, depth: usize) -> R<Response> {
    let code = r.u8()?;
    let ret = r.u16le()? as i16;
    let debug = match read_typed(r, depth)? {
        Value::Str(s) => Some(s),
        _ => None,
    };
    Ok(Response { code, ret, debug, params: params_at(r, depth)? })
}

pub fn read_disconnect_body(r: &mut Rd) -> R<Response> {
    let ret = r.u16le()? as i16;
    let debug = match read_typed(r, 1)? {
        Value::Str(s) => Some(s),
        _ => None,
    };
    Ok(Response { code: 0, ret, debug, params: read_params(r)? })
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    /// One level: a message as a value, holding one parameter whose value comes next.
    fn level(outer: u8) -> Vec<u8> {
        match outer {
            RESPONSE => vec![RESPONSE, 1, 0, 0, NULL, 1, 9],
            _ => vec![outer, 1, 1, 9],
        }
    }

    /// A request inside a parameter inside a request, and so on: each started the depth
    /// count again, so a few kilobytes from anyone who could connect ran the stack out,
    /// which ends the game. On a small stack here, so it would end the test run instead.
    #[test]
    fn messages_nested_in_values_count_as_nesting() {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                for outer in [REQUEST, EVENT, RESPONSE] {
                    let nested = |levels: usize| {
                        let mut b = Vec::new();
                        for _ in 0..levels {
                            b.extend_from_slice(&level(outer));
                        }
                        b.push(NULL);
                        b
                    };
                    assert!(read_value(&mut Rd::new(&nested(MAX_DEPTH - 1))).is_ok());
                    for levels in [MAX_DEPTH + 2, 3000, 100_000] {
                        assert_eq!(read_value(&mut Rd::new(&nested(levels))), Err("value nested too deep"));
                        // As the parameter of a message, which is where a client puts it.
                        let mut params = [1u8, 9].to_vec();
                        params.extend_from_slice(&nested(levels));
                        assert_eq!(read_params(&mut Rd::new(&params)), Err("value nested too deep"));
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
