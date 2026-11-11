//! Lobbies as the LAN sees them, independent of any SDK. A lobby lives on its owner's
//! copy; everyone else holds the owner's last snapshot. EOS lobbies and sessions and
//! Steam lobbies all map onto this, kept apart by `namespace`.

use crate::id::PeerId;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
}

impl Value {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Double(d) => Some(*d),
            _ => None,
        }
    }

    pub fn text(&self) -> String {
        match self {
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Double(d) => d.to_string(),
            Value::Str(s) => s.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub key: String,
    pub value: Value,
    /// Public attributes are visible to searchers; private ones only to members.
    pub public: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    /// Anyone may find it with an attribute search.
    Public = 0,
    /// Joinable through presence (a friend's "join game"), not listed in searches.
    Presence = 1,
    InviteOnly = 2,
    /// Found and joined like a public one by everyone the player plays with (the LAN,
    /// a room), and closed like the other two to public players, who are strangers:
    /// for an SDK's "friends" lobbies, and for what stands behind a lobby (a party
    /// network).
    Friends = 3,
}

impl Permission {
    pub fn from_u8(v: u8) -> Permission {
        match v {
            1 => Permission::Presence,
            2 => Permission::InviteOnly,
            3 => Permission::Friends,
            _ => Permission::Public,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub id: PeerId,
    pub attrs: Vec<Attr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lobby {
    pub id: String,
    /// Which front end and kind this is ("eos.lobby", "eos.session", "steam.lobby").
    pub namespace: String,
    pub owner: PeerId,
    /// EOS's bucket id: a partition searches must name. Empty for SDKs without one.
    pub bucket: String,
    pub max_members: u32,
    pub permission: Permission,
    pub allow_invites: bool,
    pub host_migration: bool,
    pub join_by_id: bool,
    pub attrs: Vec<Attr>,
    pub members: Vec<Member>,
}

impl Lobby {
    pub fn new(namespace: &str) -> Lobby {
        Lobby {
            id: String::new(),
            namespace: namespace.into(),
            owner: PeerId::default(),
            bucket: String::new(),
            max_members: 4,
            permission: Permission::Public,
            allow_invites: true,
            host_migration: true,
            join_by_id: false,
            attrs: Vec::new(),
            members: Vec::new(),
        }
    }

    pub fn open_slots(&self) -> u32 {
        self.max_members.saturating_sub(self.members.len() as u32)
    }

    pub fn member(&self, id: PeerId) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    pub fn member_mut(&mut self, id: PeerId) -> Option<&mut Member> {
        self.members.iter_mut().find(|m| m.id == id)
    }

    pub fn attr(&self, key: &str) -> Option<&Attr> {
        find_attr(&self.attrs, key)
    }

    /// What a searcher outside the lobby is allowed to see.
    pub fn public_view(&self) -> Lobby {
        let mut out = self.clone();
        out.attrs.retain(|a| a.public);
        for m in &mut out.members {
            m.attrs.retain(|a| a.public);
        }
        out
    }
}

/// EOS treats attribute keys case-insensitively, and nothing else relies on case.
pub fn find_attr<'a>(attrs: &'a [Attr], key: &str) -> Option<&'a Attr> {
    attrs.iter().find(|a| a.key.eq_ignore_ascii_case(key))
}

pub fn set_attr(attrs: &mut Vec<Attr>, attr: Attr) {
    match attrs
        .iter_mut()
        .find(|a| a.key.eq_ignore_ascii_case(&attr.key))
    {
        Some(a) => *a = attr,
        None => attrs.push(attr),
    }
}

pub fn remove_attr(attrs: &mut Vec<Attr>, key: &str) -> bool {
    let before = attrs.len();
    attrs.retain(|a| !a.key.eq_ignore_ascii_case(key));
    attrs.len() != before
}

/// Who inherits a lobby whose owner left: the lowest remaining id. Every member applies
/// the same rule to the same roster, so they agree without asking each other.
pub fn elect(members: &[Member], leaving: PeerId) -> Option<PeerId> {
    members
        .iter()
        .map(|m| m.id)
        .filter(|id| *id != leaving)
        .min()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    /// Sorts by closeness; never excludes.
    Distance,
    /// The value is a `;`-separated list; the attribute must be one of them.
    AnyOf,
    NotAnyOf,
    /// The attribute (a string) contains the value.
    Contains,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Filter {
    Attr { key: String, op: Op, value: Value },
    Bucket { op: Op, value: String },
    MinMembers(i64),
    MinSlots(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub namespace: String,
    pub lobby_id: Option<String>,
    pub member: Option<PeerId>,
    pub filters: Vec<Filter>,
    pub max_results: u32,
}

impl Query {
    pub fn new(namespace: &str) -> Query {
        Query {
            namespace: namespace.into(),
            lobby_id: None,
            member: None,
            filters: Vec::new(),
            max_results: 50,
        }
    }
}

pub fn matches(lobby: &Lobby, q: &Query) -> bool {
    if lobby.namespace != q.namespace {
        return false;
    }
    if let Some(id) = &q.lobby_id {
        if !lobby.id.eq_ignore_ascii_case(id) {
            return false;
        }
    }
    if let Some(member) = q.member {
        if lobby.member(member).is_none() {
            return false;
        }
    }
    // An open search only finds advertised lobbies. One aimed at an id or a player is
    // looking for that lobby in particular.
    if q.lobby_id.is_none() && q.member.is_none() && !matches!(lobby.permission, Permission::Public | Permission::Friends) {
        return false;
    }
    q.filters.iter().all(|f| match f {
        // Buckets double as join codes, which players type in any case.
        Filter::Bucket { op, value } => compare(
            &Value::Str(lobby.bucket.to_ascii_lowercase()),
            *op,
            &Value::Str(value.to_ascii_lowercase()),
        ),
        Filter::MinMembers(n) => lobby.members.len() as i64 >= *n,
        Filter::MinSlots(n) => lobby.open_slots() as i64 >= *n,
        Filter::Attr { key, op, value } => match lobby.attr(key) {
            Some(a) if a.public => compare(&a.value, *op, value),
            _ => *op == Op::NotAnyOf || *op == Op::Ne,
        },
    })
}

fn compare(have: &Value, op: Op, want: &Value) -> bool {
    use std::cmp::Ordering;
    let ord = match (have, want) {
        (Value::Str(a), Value::Str(b)) => Some(a.as_str().cmp(b.as_str())),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ => match (have.as_f64(), want.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b),
            _ => None,
        },
    };
    let list = || {
        want.text()
            .split(';')
            .map(|s| s.trim().to_string())
            .collect::<Vec<_>>()
    };
    match op {
        Op::Eq => ord == Some(Ordering::Equal),
        Op::Ne => ord != Some(Ordering::Equal),
        Op::Gt => ord == Some(Ordering::Greater),
        Op::Ge => matches!(ord, Some(Ordering::Greater | Ordering::Equal)),
        Op::Lt => ord == Some(Ordering::Less),
        Op::Le => matches!(ord, Some(Ordering::Less | Ordering::Equal)),
        Op::Distance => true,
        Op::AnyOf => list().iter().any(|v| *v == have.text()),
        Op::NotAnyOf => !list().iter().any(|v| *v == have.text()),
        Op::Contains => have.text().contains(&want.text()),
    }
}

/// 32 hex digits, the shape EOS uses for lobby ids.
pub fn new_lobby_id() -> String {
    PeerId::random().hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lobby() -> Lobby {
        let mut l = Lobby::new("eos.lobby");
        l.id = "abc".into();
        l.bucket = "us:1".into();
        l.max_members = 4;
        l.members.push(Member {
            id: PeerId([1; 16]),
            attrs: vec![],
        });
        l.attrs.push(Attr {
            key: "MODE".into(),
            value: Value::Int(3),
            public: true,
        });
        l.attrs.push(Attr {
            key: "secret".into(),
            value: Value::Str("x".into()),
            public: false,
        });
        l
    }

    #[test]
    fn filters_by_bucket_attributes_and_slots() {
        let l = lobby();
        let mut q = Query::new("eos.lobby");
        q.filters.push(Filter::Bucket {
            op: Op::Eq,
            value: "us:1".into(),
        });
        q.filters.push(Filter::Attr {
            key: "mode".into(),
            op: Op::Ge,
            value: Value::Int(2),
        });
        q.filters.push(Filter::MinSlots(3));
        assert!(matches(&l, &q));
        q.filters.push(Filter::MinSlots(4));
        assert!(!matches(&l, &q));

        let mut other = Query::new("eos.lobby");
        other.filters.push(Filter::Attr {
            key: "mode".into(),
            op: Op::AnyOf,
            value: Value::Str("1;3".into()),
        });
        assert!(matches(&l, &other));
        // Private attributes can't be searched on.
        other.filters = vec![Filter::Attr {
            key: "secret".into(),
            op: Op::Eq,
            value: Value::Str("x".into()),
        }];
        assert!(!matches(&l, &other));
        assert!(!matches(&l, &Query::new("eos.session")));
    }

    #[test]
    fn unlisted_lobbies_only_match_targeted_searches() {
        let mut l = lobby();
        l.permission = Permission::InviteOnly;
        assert!(!matches(&l, &Query::new("eos.lobby")));
        let mut by_id = Query::new("eos.lobby");
        by_id.lobby_id = Some("ABC".into());
        assert!(matches(&l, &by_id));
        // A friends lobby is listed like a public one (who may ask is the engine's part).
        l.permission = Permission::Friends;
        assert!(matches(&l, &Query::new("eos.lobby")));
        assert_eq!(Permission::from_u8(Permission::Friends as u8), Permission::Friends);
    }

    #[test]
    fn election_picks_the_lowest_remaining_id() {
        let m = |b| Member {
            id: PeerId([b; 16]),
            attrs: vec![],
        };
        assert_eq!(
            elect(&[m(5), m(2), m(9)], PeerId([2; 16])),
            Some(PeerId([5; 16]))
        );
        assert_eq!(elect(&[m(2)], PeerId([2; 16])), None);
    }

    #[test]
    fn public_view_hides_private_attributes() {
        let v = lobby().public_view();
        assert!(v.attr("secret").is_none());
        assert!(v.attr("mode").is_some());
    }
}
