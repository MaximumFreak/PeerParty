//! Small shared primitives that belong to no one SDK: the stand-ins (photon/, ugs/,
//! playfab/, demonware/, xboxlive/, eos/, galaxy/) all build on these, and none of them
//! reaches into another stand-in for one.
//!
//! - crypto.rs: SHA-256, SHA-1, MD5, AES (CBC, GCM), HMAC-SHA256, random bytes.
//! - encoding.rs: base64 / base64url, GUID-shaped text, random text.
//! - http.rs: HTTP/1.1 message pieces (chunked bodies, status reasons).
//! - json.rs: the protocol crate's JSON, for the pipe and the services' REST bodies.
//! - local_store.rs: where a stand-in keeps a game's data on disk, and safe file names.
//! - time.rs: Unix time and ISO 8601 dates.
//! - ws.rs: WebSocket (RFC 6455) handshake and framing.

pub(crate) mod crypto;
pub(crate) mod encoding;
pub(crate) mod http;
pub(crate) mod json;
pub(crate) mod local_store;
pub(crate) mod time;
pub(crate) mod ws;
