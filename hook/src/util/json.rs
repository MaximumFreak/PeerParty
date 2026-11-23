//! The JSON of the pipe to the launcher and of the services' REST bodies: the protocol
//! crate's (protocol/src/json.rs), where integers stay exact (`Json::U64` / `Json::Int`).
//! Read a number with `as_u64` / `as_i64` (ids, counts) or `as_f64` (any number), never by
//! matching `Json::Num`, which holds only fractions and exponents.

pub use peerparty_protocol::json::*;
