//! Serialization tests: hand-worked byte vectors from the GpBinary specifications
//! (Luxon doc/gp_binary_v18.md) plus round trips of
//! every value shape in both protocols.

use super::crypto::Aes256;
use super::message::*;
use super::value::*;
use super::{gp16, gp18};

fn enc(p: Proto, v: &Value) -> Vec<u8> {
    let mut w = Vec::new();
    p.write_value(&mut w, v).unwrap();
    w
}

fn dec(p: Proto, b: &[u8]) -> Value {
    let mut r = Rd::new(b);
    let v = p.read_value(&mut r).unwrap();
    assert_eq!(r.left(), 0, "trailing bytes");
    v
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

#[test]
fn v16_vectors() {
    let p = Proto::V16;
    assert_eq!(enc(p, &Value::Int(5)), [b'i', 0, 0, 0, 5]);
    assert_eq!(enc(p, &Value::Short(-2)), [b'k', 0xFF, 0xFE]);
    assert_eq!(enc(p, &s("ab")), [b's', 0, 2, b'a', b'b']);
    assert_eq!(enc(p, &Value::Null), [b'*']);
    assert_eq!(enc(p, &Value::Float(1.0)), [b'f', 0x3F, 0x80, 0, 0]);
    assert_eq!(enc(p, &Value::Bytes(vec![7, 8])), [b'x', 0, 0, 0, 2, 7, 8]);
    assert_eq!(enc(p, &Value::Ints(vec![1, 2])), [b'y', 0, 2, b'i', 0, 0, 0, 1, 0, 0, 0, 2]);
    assert_eq!(enc(p, &Value::Strs(vec!["a".into()])), [b'a', 0, 1, 0, 1, b'a']);
    assert_eq!(
        enc(p, &Value::Hashtable(vec![(Value::Byte(1), Value::Bool(true))])),
        [b'h', 0, 1, b'b', 1, b'o', 1]
    );
    assert_eq!(
        enc(p, &Value::Dict(DictTy { key: Ty::Str, val: Ty::Any }, vec![(s("a"), Value::Int(1))])),
        [b'D', b's', 0, 0, 1, 0, 1, b'a', b'i', 0, 0, 0, 1]
    );
    assert_eq!(enc(p, &Value::Custom(b'V', vec![1, 2])), [b'c', b'V', 0, 2, 1, 2]);
    // string[] is written as its own type code, as the client does. What the real client writes for int[]: 'n' with an i32 count is read too.
    assert_eq!(dec(p, &[b'n', 0, 0, 0, 1, 0, 0, 0, 9]), Value::Ints(vec![9]));
    let mut w = Vec::new();
    gp16::write_response_body(
        &mut w,
        &Response { code: 230, ret: -2, debug: Some("x".into()), params: vec![(1, Value::Null)] },
    )
    .unwrap();
    assert_eq!(w, [230, 0xFF, 0xFE, b's', 0, 1, b'x', 0, 1, 1, b'*']);
}

#[test]
fn v18_vectors() {
    let p = Proto::V18;
    assert_eq!(enc(p, &Value::Int(0)), [0x1E]);
    assert_eq!(enc(p, &Value::Int(5)), [0x0B, 5]);
    assert_eq!(enc(p, &Value::Int(-5)), [0x0C, 5]);
    assert_eq!(enc(p, &Value::Int(300)), [0x0D, 0x2C, 0x01]);
    // 70000 zigzags to 140000 = 0x222E0 -> varint E0 C5 08.
    assert_eq!(enc(p, &Value::Int(70000)), [0x09, 0xE0, 0xC5, 0x08]);
    assert_eq!(enc(p, &Value::Int(i32::MIN)), [0x09, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
    assert_eq!(enc(p, &Value::Long(0)), [0x1F]);
    assert_eq!(enc(p, &Value::Long(-70000)), [0x0A, 0xDF, 0xC5, 0x08]);
    assert_eq!(enc(p, &Value::Bool(true)), [0x1C]);
    assert_eq!(enc(p, &Value::Byte(0)), [0x22]);
    assert_eq!(enc(p, &Value::Byte(9)), [0x03, 9]);
    assert_eq!(enc(p, &Value::Short(-2)), [0x04, 0xFE, 0xFF]);
    assert_eq!(enc(p, &Value::Float(1.0)), [0x05, 0, 0, 0x80, 0x3F]);
    assert_eq!(enc(p, &s("ab")), [0x07, 2, b'a', b'b']);
    assert_eq!(enc(p, &Value::Null), [0x08]);
    assert_eq!(enc(p, &Value::Ints(vec![1, -1])), [0x49, 2, 2, 1]);
    assert_eq!(enc(p, &Value::Bools(vec![true, false, true])), [0x42, 3, 0b101]);
    assert_eq!(enc(p, &Value::Bytes(vec![7])), [0x43, 1, 7]);
    assert_eq!(enc(p, &Value::Custom(1, vec![9])), [0x81, 1, 9]);
    assert_eq!(enc(p, &Value::Custom(150, vec![9])), [0x13, 150, 1, 9]);
    assert_eq!(enc(p, &Value::Hashtable(vec![(Value::Byte(1), Value::Bool(true))])), [0x15, 1, 0x03, 1, 0x1C]);
    assert_eq!(
        enc(p, &Value::Dict(DictTy { key: Ty::Str, val: Ty::Any }, vec![(s("a"), Value::Int(1))])),
        [0x14, 0x07, 0x00, 1, 1, b'a', 0x0B, 1]
    );
    assert_eq!(
        enc(p, &Value::Dict(DictTy { key: Ty::Byte, val: Ty::Int }, vec![(Value::Byte(2), Value::Int(-1))])),
        [0x14, 0x03, 0x09, 1, 2, 1]
    );
    assert_eq!(enc(p, &Value::Jagged(vec![Value::Ints(vec![1])])), [0x40, 1, 0x49, 1, 2]);
    assert_eq!(enc(p, &Value::Objects(vec![Value::Int(1), Value::Null])), [0x17, 2, 0x0B, 1, 0x08]);
    let mut w = Vec::new();
    gp18::write_response_body(
        &mut w,
        &Response { code: 230, ret: -2, debug: Some("x".into()), params: vec![(1, Value::Null)] },
    )
    .unwrap();
    assert_eq!(w, [230, 0xFE, 0xFF, 0x07, 1, b'x', 1, 1, 0x08]);
    let mut w = Vec::new();
    gp18::write_response_body(&mut w, &Response { code: 1, ret: 0, debug: Some(String::new()), params: vec![] })
        .unwrap();
    assert_eq!(w, [1, 0, 0, 0x08, 0]);
}

#[test]
fn v18_nested_dictionary_values_carry_their_header() {
    let inner = DictTy { key: Ty::Byte, val: Ty::Any };
    let v = Value::Dict(
        DictTy { key: Ty::Str, val: Ty::Dict(Box::new(inner.clone())) },
        vec![(s("k"), Value::Dict(inner, vec![(Value::Byte(1), Value::Int(2))]))],
    );
    let b = enc(Proto::V18, &v);
    assert_eq!(b, [0x14, 0x07, 0x14, 0x03, 0x00, 1, 1, b'k', 0x03, 0x00, 1, 1, 0x0B, 2]);
    assert_eq!(dec(Proto::V18, &b), v);
}

fn every_shape() -> Value {
    let dt = DictTy { key: Ty::Str, val: Ty::Int };
    let nested = Value::Hashtable(vec![
        (Value::Byte(255), Value::Byte(4)),
        (s("name"), s("room")),
        (Value::Int(-1), Value::Bool(false)),
    ]);
    Value::Hashtable(vec![
        (Value::Byte(1), Value::Bool(true)),
        (Value::Byte(2), Value::Short(-300)),
        (Value::Byte(3), Value::Int(i32::MAX)),
        (Value::Byte(4), Value::Long(i64::MIN)),
        (Value::Byte(5), Value::Float(-2.5)),
        (Value::Byte(6), Value::Double(1e300)),
        (Value::Byte(7), s("héllo")),
        (Value::Byte(8), Value::Bytes(vec![0, 1, 255])),
        (Value::Byte(9), Value::Bools(vec![true; 11])),
        (Value::Byte(10), Value::Shorts(vec![1, -1])),
        (Value::Byte(11), Value::Ints(vec![0, 70000, -70000])),
        (Value::Byte(12), Value::Longs(vec![1 << 40, -3])),
        (Value::Byte(13), Value::Floats(vec![0.5])),
        (Value::Byte(14), Value::Doubles(vec![0.25, -0.0])),
        (Value::Byte(15), Value::Strs(vec!["a".into(), String::new()])),
        (Value::Byte(16), Value::Objects(vec![Value::Null, Value::Int(1), nested.clone()])),
        (Value::Byte(17), nested.clone()),
        (Value::Byte(18), Value::Hashtables(vec![vec![], vec![(Value::Byte(1), Value::Null)]])),
        (Value::Byte(19), Value::Dict(dt.clone(), vec![(s("x"), Value::Int(-9))])),
        (Value::Byte(20), Value::Dict(DictTy::any(), vec![(Value::Int(1), s("y"))])),
        (Value::Byte(21), Value::Dicts(dt.clone(), vec![vec![(s("z"), Value::Int(3))], vec![]])),
        (Value::Byte(22), Value::Custom(b'W', vec![1, 2, 3])),
        (Value::Byte(23), Value::Customs(b'P', vec![vec![1], vec![2, 3]])),
        (Value::Byte(24), Value::Jagged(vec![Value::Bytes(vec![1]), Value::Bytes(vec![])])),
        (Value::Byte(25), Value::Jagged(vec![Value::Ints(vec![1]), Value::Ints(vec![2, 3])])),
        (
            Value::Byte(26),
            Value::Event(Box::new(EventMsg { code: 200, params: vec![(245, Value::Objects(vec![Value::Int(1)]))] })),
        ),
        (Value::Byte(27), Value::Null),
    ])
}

#[test]
fn every_shape_round_trips_in_both_protocols() {
    let v = every_shape();
    for p in [Proto::V16, Proto::V18] {
        let b = enc(p, &v);
        assert_eq!(dec(p, &b), v, "{p:?}");
    }
}

#[test]
fn truncated_and_hostile_input_fails_cleanly() {
    for p in [Proto::V16, Proto::V18] {
        let b = enc(p, &every_shape());
        for n in 0..b.len() {
            assert!(p.read_value(&mut Rd::new(&b[..n])).is_err(), "{p:?} prefix {n} decoded");
        }
    }
    // Huge declared counts mustn't allocate.
    assert!(Proto::V16.read_value(&mut Rd::new(&[b'y', 0xFF, 0xFF, b's'])).is_err());
    assert!(Proto::V18.read_value(&mut Rd::new(&[0x47, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F])).is_err());
    // Deep nesting is refused instead of overflowing the stack.
    let deep: Vec<u8> = std::iter::repeat_n([0x17u8, 1], 10_000).flatten().collect();
    assert!(Proto::V18.read_value(&mut Rd::new(&deep)).is_err());
    let deep: Vec<u8> = std::iter::repeat_n([b'z', 0, 1], 10_000).flatten().collect();
    assert!(Proto::V16.read_value(&mut Rd::new(&deep)).is_err());
}

#[test]
fn messages_round_trip_plain_and_encrypted() {
    let aes = Aes256::new(&[3u8; 32]);
    let msgs = vec![
        Msg::Request(Request { code: 230, params: vec![(220, s("1.0")), (224, s("app"))] }),
        Msg::Response(Response { code: 230, ret: 0, debug: None, params: vec![(221, s("tok"))] }),
        Msg::Event(EventMsg { code: 255, params: vec![(254, Value::Int(1))] }),
        Msg::InternalRequest(Request { code: 0, params: vec![(1, Value::Bytes(vec![1; 96]))] }),
        Msg::InternalResponse(Response { code: 0, ret: 0, debug: None, params: vec![(1, Value::Bytes(vec![2]))] }),
        Msg::Disconnect(Response { code: 0, ret: 32767, debug: Some("bye".into()), params: vec![] }),
        Msg::InitResponse,
    ];
    for p in [Proto::V16, Proto::V18] {
        for m in &msgs {
            let plain = p.encode(m, None).unwrap();
            assert_eq!(plain[0], MAGIC);
            assert_eq!(p.decode(&plain, None).unwrap(), (m.clone(), false));
            let sealed = p.encode(m, Some(&aes)).unwrap();
            let never = matches!(m, Msg::Disconnect(_) | Msg::InitResponse);
            assert_eq!(sealed[1] & 0x80 != 0, !never);
            assert_eq!(p.decode(&sealed, Some(&aes)).unwrap(), (m.clone(), !never));
            if !never {
                assert!(p.decode(&sealed, None).is_err());
            }
        }
    }
    assert_eq!(Proto::V16.encode(&Msg::InitResponse, None).unwrap(), [0xF3, 1]);
    assert_eq!(Proto::V18.encode(&Msg::InitResponse, None).unwrap(), [0xF3, 1, 0]);
}

#[test]
fn classic_init_parses() {
    let init = Init {
        proto_major: 1,
        proto_minor: 8,
        sdk: 15,
        version: [4, 1, 8, 4],
        ipv6: false,
        app_id: "NameServer".into(),
    };
    let b = Proto::V18.encode(&Msg::Init(init.clone()), None).unwrap();
    assert_eq!(b.len(), 41);
    assert_eq!(&b[..9], &[0xF3, 0, 1, 8, 30, 0x41, 8, 4, 0]);
    assert_eq!(parse_hello(&b).unwrap(), Hello { init, token: None });
    assert!(parse_hello(&b[..40]).is_err());
}

#[test]
fn http_init_with_token_parses() {
    let mut body = Vec::new();
    Proto::V18.write_value(&mut body, &s("TOKEN123")).unwrap();
    let mut req = format!(
        "POST /?init&app=abc-123&clientversion=4.1.8.4&protocol=GpBinaryV18&sid=7&IPv6 HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(&body);
    let h = parse_hello(&req).unwrap();
    assert_eq!(h.token.as_deref(), Some("TOKEN123"));
    assert_eq!(h.init.app_id, "abc-123");
    assert_eq!((h.init.proto_major, h.init.proto_minor), (1, 8));
    assert_eq!(h.init.version, [4, 1, 8, 4]);
    assert!(h.init.ipv6);
}
