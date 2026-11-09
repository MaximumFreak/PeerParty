//! Photon Chat on the LAN: public channels, private messages, friends' statuses.
//!
//! A ChatClient signs in on the name server like a Realtime client, then connects to
//! the "front end" address it got back (PeerParty's master server) with Init app
//! "chat" and authenticates there with the token. From then on its operations are
//! Chat's (codes 0-8), answered here on the master server's connection. Every copy's
//! chat clients meet on the one PeerParty that hosts Photon, so channels, private
//! messages and friends work across the LAN.
//!
//! Codes and payloads from Photon Chat's client (ChatOperationCode, ChatEventCode,
//! ChatParameterCode, ChatClient's event handlers), which reads:
//! - Subscribe (event 5): Channels (string[]), SubscribeResults (bool[]), and for a
//!   channel that publishes its subscribers, Properties and ChannelSubscribers.
//! - ChatMessages (0): Messages (object[]), Senders (string[]), Channel, MsgId (the
//!   last message's id).
//! - PrivateMessage (2): Message, Sender, MsgId; the sender's own copy also carries
//!   UserId (the receiver), which names the private channel on its side.
//! - StatusUpdate (4): Sender, Status, and Message unless the update skipped it.
//! - Unsubscribe (6): Channels. UserSubscribed / UserUnsubscribed (8 / 9): Channel,
//!   UserId.
//! Operation responses carry nothing the client reads (it waits for the events).
//! Not kept: web flags, channel properties other than MaxSubscribers and
//! PublishSubscribers, user properties, encrypted private messages' encryption (they
//! arrive as plain messages on the LAN).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use super::value::*;

pub mod op {
    pub const SUBSCRIBE: u8 = 0;
    pub const UNSUBSCRIBE: u8 = 1;
    pub const PUBLISH: u8 = 2;
    pub const SEND_PRIVATE: u8 = 3;
    pub const CHANNEL_HISTORY: u8 = 4;
    pub const UPDATE_STATUS: u8 = 5;
    pub const ADD_FRIENDS: u8 = 6;
    pub const REMOVE_FRIENDS: u8 = 7;
    pub const SET_PROPERTIES: u8 = 8;
}

pub mod ev {
    pub const CHAT_MESSAGES: u8 = 0;
    pub const PRIVATE_MESSAGE: u8 = 2;
    pub const STATUS_UPDATE: u8 = 4;
    pub const SUBSCRIBE: u8 = 5;
    pub const UNSUBSCRIBE: u8 = 6;
    pub const USER_SUBSCRIBED: u8 = 8;
    pub const USER_UNSUBSCRIBED: u8 = 9;
}

pub mod key {
    pub const CHANNELS: u8 = 0;
    pub const CHANNEL: u8 = 1;
    pub const MESSAGES: u8 = 2;
    pub const MESSAGE: u8 = 3;
    pub const SENDERS: u8 = 4;
    pub const SENDER: u8 = 5;
    pub const MSG_ID: u8 = 8;
    pub const MSG_IDS: u8 = 9;
    pub const STATUS: u8 = 10;
    pub const FRIENDS: u8 = 11;
    pub const SKIP_MESSAGE: u8 = 12;
    pub const HISTORY_LENGTH: u8 = 14;
    pub const SUBSCRIBE_RESULTS: u8 = 15;
    pub const PROPERTIES: u8 = 22;
    pub const CHANNEL_SUBSCRIBERS: u8 = 23;
    pub const USER_ID: u8 = 225;
}

/// ChannelWellKnownProperties.
const PROP_MAX_SUBSCRIBERS: u8 = 255;
const PROP_PUBLISH_SUBSCRIBERS: u8 = 254;
/// ChatUserStatus.Offline / Online.
pub const OFFLINE: i32 = 0;
pub const ONLINE: i32 = 2;
/// Messages kept per channel for history requests.
const HISTORY: usize = 100;

#[derive(Default)]
struct Channel {
    subscribers: BTreeSet<u64>,
    history: VecDeque<(i32, String, Value)>,
    last_id: i32,
    max_subscribers: i32,
    publish_subscribers: bool,
}

struct User {
    app: String,
    id: String,
    friends: BTreeSet<String>,
    status: i32,
    message: Option<Value>,
}

/// Events for connections, in order.
pub type Events = Vec<(u64, EventMsg)>;

#[derive(Default)]
pub struct Chat {
    users: BTreeMap<u64, User>,
    /// (app id, channel name).
    channels: HashMap<(String, String), Channel>,
}

fn event(code: u8, params: Params) -> EventMsg {
    EventMsg { code, params }
}

impl Chat {
    /// A chat client signed in on connection `conn`.
    pub fn signed_in(&mut self, conn: u64, app: &str, user: &str) {
        self.users.insert(conn, User { app: app.into(), id: user.into(), friends: BTreeSet::new(), status: ONLINE, message: None });
    }

    pub fn is_chat(&self, conn: u64) -> bool {
        self.users.contains_key(&conn)
    }

    /// The connections of `user` in `app`.
    fn conns_of(&self, app: &str, user: &str) -> Vec<u64> {
        self.users.iter().filter(|(_, u)| u.app == app && u.id == user).map(|(c, _)| *c).collect()
    }

    /// The status event `of` sends to someone who has them as a friend.
    fn status_event(u: &User, status: i32, message: Option<&Value>) -> EventMsg {
        let mut p = vec![(key::SENDER, Value::Str(u.id.clone())), (key::STATUS, Value::Int(status))];
        if let Some(m) = message {
            p.push((key::MESSAGE, m.clone()));
        }
        event(ev::STATUS_UPDATE, p)
    }

    /// Everyone in the same app who listed `user` as a friend.
    fn watchers(&self, app: &str, user: &str) -> Vec<u64> {
        self.users.iter().filter(|(_, w)| w.app == app && w.friends.contains(user)).map(|(c, _)| *c).collect()
    }

    /// The connection went away: it leaves its channels and its friends see it offline.
    pub fn gone(&mut self, conn: u64) -> Events {
        let Some(u) = self.users.remove(&conn) else { return vec![] };
        let mut out = Vec::new();
        for ((app, name), ch) in self.channels.iter_mut() {
            if *app == u.app && ch.subscribers.remove(&conn) && ch.publish_subscribers {
                for s in &ch.subscribers {
                    out.push((*s, event(ev::USER_UNSUBSCRIBED, vec![(key::CHANNEL, Value::Str(name.clone())), (key::USER_ID, Value::Str(u.id.clone()))])));
                }
            }
        }
        self.channels.retain(|_, c| !c.subscribers.is_empty() || !c.history.is_empty());
        // Still online through another connection: no change for friends.
        if self.conns_of(&u.app, &u.id).is_empty() {
            for w in self.watchers(&u.app, &u.id) {
                out.push((w, Self::status_event(&u, OFFLINE, None)));
            }
        }
        out
    }

    /// A Chat operation from a signed-in connection: the result code and the events.
    pub fn op(&mut self, conn: u64, req: &Request) -> (i16, Events) {
        let Some(u) = self.users.get(&conn) else { return (-3, vec![]) };
        let (app, me) = (u.app.clone(), u.id.clone());
        let p = &req.params;
        let mut out = Events::new();
        match req.code {
            op::SUBSCRIBE => {
                let names = param(p, key::CHANNELS).and_then(Value::as_strs).unwrap_or_default();
                let history = param(p, key::HISTORY_LENGTH).and_then(Value::as_i32).unwrap_or(0);
                let after = param(p, key::MSG_IDS).and_then(Value::as_ints);
                let props = param(p, key::PROPERTIES).and_then(props_of);
                let mut results = Vec::new();
                for (i, name) in names.iter().enumerate() {
                    let ch = self.channels.entry((app.clone(), name.clone())).or_default();
                    if ch.subscribers.is_empty() && names.len() == 1 {
                        if let Some((max, publish)) = props {
                            ch.max_subscribers = max;
                            ch.publish_subscribers = publish;
                        }
                    }
                    let full = ch.max_subscribers > 0 && ch.subscribers.len() as i32 >= ch.max_subscribers && !ch.subscribers.contains(&conn);
                    results.push(!full);
                    if full {
                        continue;
                    }
                    let newly = ch.subscribers.insert(conn);
                    if newly && ch.publish_subscribers {
                        for s in ch.subscribers.iter().filter(|s| **s != conn) {
                            out.push((*s, event(ev::USER_SUBSCRIBED, vec![(key::CHANNEL, Value::Str(name.clone())), (key::USER_ID, Value::Str(me.clone()))])));
                        }
                    }
                    // History: the last n (-1 all), or what came after the given id.
                    let since = after.as_ref().and_then(|a| a.get(i).copied());
                    let past: Vec<&(i32, String, Value)> = match (since, history) {
                        (Some(id), _) => ch.history.iter().filter(|m| m.0 > id).collect(),
                        (None, n) if n < 0 => ch.history.iter().collect(),
                        (None, n) => ch.history.iter().rev().take(n as usize).rev().collect(),
                    };
                    if !past.is_empty() {
                        out.push((conn, messages_event(name, &past)));
                    }
                }
                let mut params = vec![(key::CHANNELS, Value::Strs(names.clone())), (key::SUBSCRIBE_RESULTS, Value::Bools(results))];
                if names.len() == 1 {
                    if let Some(ch) = self.channels.get(&(app.clone(), names[0].clone())) {
                        if ch.publish_subscribers {
                            params.push((key::PROPERTIES, props_value(ch)));
                            let who: Vec<String> = ch.subscribers.iter().filter_map(|s| self.users.get(s)).map(|u| u.id.clone()).collect();
                            params.push((key::CHANNEL_SUBSCRIBERS, Value::Strs(who)));
                        }
                    }
                }
                // The subscribe event comes before any history for it.
                out.insert(0, (conn, event(ev::SUBSCRIBE, params)));
            }
            op::UNSUBSCRIBE => {
                let names = param(p, key::CHANNELS).and_then(Value::as_strs).unwrap_or_default();
                for name in &names {
                    if let Some(ch) = self.channels.get_mut(&(app.clone(), name.clone())) {
                        if ch.subscribers.remove(&conn) && ch.publish_subscribers {
                            for s in &ch.subscribers {
                                out.push((*s, event(ev::USER_UNSUBSCRIBED, vec![(key::CHANNEL, Value::Str(name.clone())), (key::USER_ID, Value::Str(me.clone()))])));
                            }
                        }
                    }
                }
                self.channels.retain(|_, c| !c.subscribers.is_empty() || !c.history.is_empty());
                out.push((conn, event(ev::UNSUBSCRIBE, vec![(key::CHANNELS, Value::Strs(names))])));
            }
            op::PUBLISH => {
                let Some(name) = param(p, key::CHANNEL).and_then(Value::as_str).map(str::to_owned) else { return (-6, out) };
                let message = param(p, key::MESSAGE).cloned().unwrap_or(Value::Null);
                let ch = self.channels.entry((app.clone(), name.clone())).or_default();
                ch.last_id += 1;
                let id = ch.last_id;
                ch.history.push_back((id, me.clone(), message));
                while ch.history.len() > HISTORY {
                    ch.history.pop_front();
                }
                let last = ch.history.back().unwrap();
                let e = messages_event(&name, &[last]);
                for s in &ch.subscribers {
                    out.push((*s, e.clone()));
                }
            }
            op::SEND_PRIVATE => {
                let Some(to) = param(p, key::USER_ID).and_then(Value::as_str).map(str::to_owned) else { return (-6, out) };
                let message = param(p, key::MESSAGE).cloned().unwrap_or(Value::Null);
                let ch = self.channels.entry((app.clone(), private_channel(&me, &to))).or_default();
                ch.last_id += 1;
                let id = ch.last_id;
                let base = vec![(key::SENDER, Value::Str(me.clone())), (key::MESSAGE, message), (key::MSG_ID, Value::Int(id))];
                for c in self.conns_of(&app, &to) {
                    out.push((c, event(ev::PRIVATE_MESSAGE, base.clone())));
                }
                // The sender's copy names the receiver.
                if to != me {
                    let mut mine = base;
                    mine.push((key::USER_ID, Value::Str(to.clone())));
                    for c in self.conns_of(&app, &me) {
                        out.push((c, event(ev::PRIVATE_MESSAGE, mine.clone())));
                    }
                }
            }
            op::UPDATE_STATUS => {
                let status = param(p, key::STATUS).and_then(Value::as_i32).unwrap_or(ONLINE);
                let skip = param(p, key::SKIP_MESSAGE).and_then(Value::as_bool).unwrap_or(false);
                let message = if skip { None } else { Some(param(p, key::MESSAGE).cloned().unwrap_or(Value::Null)) };
                for c in self.conns_of(&app, &me) {
                    let u = self.users.get_mut(&c).unwrap();
                    u.status = status;
                    if !skip {
                        u.message = message.clone();
                    }
                }
                let u = &self.users[&conn];
                let e = Self::status_event(u, status, message.as_ref());
                for w in self.watchers(&app, &me) {
                    out.push((w, e.clone()));
                }
            }
            op::ADD_FRIENDS => {
                let friends = param(p, key::FRIENDS).and_then(Value::as_strs).unwrap_or_default();
                self.users.get_mut(&conn).unwrap().friends.extend(friends.iter().cloned());
                // Friends already online say so at once.
                for f in &friends {
                    if let Some(c) = self.conns_of(&app, f).first() {
                        let fu = &self.users[c];
                        out.push((conn, Self::status_event(fu, fu.status, fu.message.as_ref())));
                    }
                }
            }
            op::REMOVE_FRIENDS => {
                let friends = param(p, key::FRIENDS).and_then(Value::as_strs).unwrap_or_default();
                let u = self.users.get_mut(&conn).unwrap();
                for f in &friends {
                    u.friends.remove(f);
                }
            }
            op::CHANNEL_HISTORY => {
                let Some(name) = param(p, key::CHANNEL).and_then(Value::as_str).map(str::to_owned) else { return (-6, out) };
                let n = param(p, key::HISTORY_LENGTH).and_then(Value::as_i32).unwrap_or(-1);
                if let Some(ch) = self.channels.get(&(app.clone(), name.clone())) {
                    let past: Vec<&(i32, String, Value)> =
                        if n < 0 { ch.history.iter().collect() } else { ch.history.iter().rev().take(n as usize).rev().collect() };
                    if !past.is_empty() {
                        out.push((conn, messages_event(&name, &past)));
                    }
                }
            }
            op::SET_PROPERTIES => {}
            _ => return (-2, out),
        }
        (0, out)
    }
}

/// The private channel between two users, the same whichever one sends.
fn private_channel(a: &str, b: &str) -> String {
    if a <= b { format!("{a}:{b}") } else { format!("{b}:{a}") }
}

fn messages_event(channel: &str, msgs: &[&(i32, String, Value)]) -> EventMsg {
    event(
        ev::CHAT_MESSAGES,
        vec![
            (key::MESSAGES, Value::Objects(msgs.iter().map(|m| m.2.clone()).collect())),
            (key::SENDERS, Value::Strs(msgs.iter().map(|m| m.1.clone()).collect())),
            (key::CHANNEL, Value::Str(channel.into())),
            (key::MSG_ID, Value::Int(msgs.last().map_or(0, |m| m.0))),
        ],
    )
}

/// MaxSubscribers and PublishSubscribers from a Subscribe's channel properties.
fn props_of(v: &Value) -> Option<(i32, bool)> {
    let pairs: Vec<(Value, Value)> = match v {
        Value::Hashtable(t) => t.clone(),
        Value::Dict(_, d) => d.clone(),
        _ => return None,
    };
    let get = |k: u8| pairs.iter().find(|(key, _)| key.as_u8() == Some(k)).map(|(_, v)| v.clone());
    Some((
        get(PROP_MAX_SUBSCRIBERS).and_then(|v| v.as_i32()).unwrap_or(0),
        get(PROP_PUBLISH_SUBSCRIBERS).and_then(|v| v.as_bool()).unwrap_or(false),
    ))
}

fn props_value(ch: &Channel) -> Value {
    Value::Dict(
        DictTy { key: Ty::Any, val: Ty::Any },
        vec![
            (Value::Byte(PROP_MAX_SUBSCRIBERS), Value::Int(ch.max_subscribers)),
            (Value::Byte(PROP_PUBLISH_SUBSCRIBERS), Value::Bool(ch.publish_subscribers)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(code: u8, params: Params) -> Request {
        Request { code, params }
    }

    fn codes(e: &Events, to: u64) -> Vec<u8> {
        e.iter().filter(|(c, _)| *c == to).map(|(_, e)| e.code).collect()
    }

    #[test]
    fn channels_private_messages_and_friends() {
        let mut chat = Chat::default();
        chat.signed_in(1, "app", "ann");
        chat.signed_in(2, "app", "bob");
        chat.signed_in(3, "other", "bob");
        // Ann subscribes to a channel that publishes its subscribers, then Bob joins.
        let props = Value::Dict(DictTy { key: Ty::Any, val: Ty::Any }, vec![(Value::Byte(254), Value::Bool(true))]);
        let (r, e) = chat.op(1, &req(op::SUBSCRIBE, vec![(key::CHANNELS, Value::Strs(vec!["lobby".into()])), (key::PROPERTIES, props)]));
        assert_eq!(r, 0);
        assert_eq!(codes(&e, 1), vec![ev::SUBSCRIBE]);
        let (_, e) = chat.op(2, &req(op::SUBSCRIBE, vec![(key::CHANNELS, Value::Strs(vec!["lobby".into()]))]));
        assert_eq!(codes(&e, 1), vec![ev::USER_SUBSCRIBED]);
        let sub = &e.iter().find(|(c, e)| *c == 2 && e.code == ev::SUBSCRIBE).unwrap().1;
        assert_eq!(param(&sub.params, key::CHANNEL_SUBSCRIBERS), Some(&Value::Strs(vec!["ann".into(), "bob".into()])));
        // A message reaches both subscribers; history comes back to a late subscriber.
        let (_, e) = chat.op(1, &req(op::PUBLISH, vec![(key::CHANNEL, Value::Str("lobby".into())), (key::MESSAGE, Value::Str("hi".into()))]));
        assert_eq!(codes(&e, 1), vec![ev::CHAT_MESSAGES]);
        assert_eq!(codes(&e, 2), vec![ev::CHAT_MESSAGES]);
        assert_eq!(param(&e[0].1.params, key::SENDERS), Some(&Value::Strs(vec!["ann".into()])));
        chat.signed_in(4, "app", "cat");
        let (_, e) = chat.op(4, &req(op::SUBSCRIBE, vec![(key::CHANNELS, Value::Strs(vec!["lobby".into()])), (key::HISTORY_LENGTH, Value::Int(-1))]));
        assert_eq!(codes(&e, 4), vec![ev::SUBSCRIBE, ev::CHAT_MESSAGES]);
        // A private message: to Bob in this app only, and a copy back naming Bob.
        let (_, e) = chat.op(1, &req(op::SEND_PRIVATE, vec![(key::USER_ID, Value::Str("bob".into())), (key::MESSAGE, Value::Str("psst".into()))]));
        assert_eq!(codes(&e, 2), vec![ev::PRIVATE_MESSAGE]);
        assert!(codes(&e, 3).is_empty());
        let mine = &e.iter().find(|(c, _)| *c == 1).unwrap().1;
        assert_eq!(param(&mine.params, key::USER_ID), Some(&Value::Str("bob".into())));
        // Friends: Bob is online at once; his status change and his leaving reach Ann.
        let (_, e) = chat.op(1, &req(op::ADD_FRIENDS, vec![(key::FRIENDS, Value::Strs(vec!["bob".into(), "nobody".into()]))]));
        assert_eq!(codes(&e, 1), vec![ev::STATUS_UPDATE]);
        let (_, e) = chat.op(2, &req(op::UPDATE_STATUS, vec![(key::STATUS, Value::Int(3)), (key::MESSAGE, Value::Str("brb".into()))]));
        assert_eq!(param(&e[0].1.params, key::STATUS), Some(&Value::Int(3)));
        assert_eq!(e[0].0, 1);
        let e = chat.gone(2);
        assert!(e.iter().any(|(c, e)| *c == 1 && e.code == ev::USER_UNSUBSCRIBED));
        assert!(e.iter().any(|(c, e)| *c == 1 && e.code == ev::STATUS_UPDATE && param(&e.params, key::STATUS) == Some(&Value::Int(OFFLINE))));
    }

    #[test]
    fn full_channels_refuse_and_unknown_ops_fail() {
        let mut chat = Chat::default();
        for (c, u) in [(1, "a"), (2, "b")] {
            chat.signed_in(c, "app", u);
        }
        let props = Value::Hashtable(vec![(Value::Byte(255), Value::Int(1))]);
        chat.op(1, &req(op::SUBSCRIBE, vec![(key::CHANNELS, Value::Strs(vec!["x".into()])), (key::PROPERTIES, props)]));
        let (_, e) = chat.op(2, &req(op::SUBSCRIBE, vec![(key::CHANNELS, Value::Strs(vec!["x".into()]))]));
        assert_eq!(param(&e[0].1.params, key::SUBSCRIBE_RESULTS), Some(&Value::Bools(vec![false])));
        assert_eq!(chat.op(1, &req(77, vec![])).0, -2);
    }
}
