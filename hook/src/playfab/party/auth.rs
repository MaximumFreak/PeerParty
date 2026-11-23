//! Who may use a Party network: its owner's copy decides (the copy that owns the LAN
//! lobby), from the network's invitations, not each joiner's own copy.
//!
//! Party joins in two steps: `PartyConnectToNetwork` connects the device (the LAN join,
//! before any invitation is known), then `PartyNetworkAuthenticateLocalUser` brings a
//! user in with an invitation. So the LAN join itself isn't checked; what a member may
//! see and be seen in is:
//!
//! - Each invitation is a members-only attribute of the member whose user made it,
//!   `inv:<identifier>` = "<revocability>;<entity ids>" (PARTY_INVITATION_REVOCABILITY,
//!   0 creator or 1 anyone; the PlayFab entity ids it is for, comma-separated, empty:
//!   anyone). It goes when that member leaves, as Party revokes an invitation whose
//!   creator leaves, or when its creator revokes it.
//! - Anyone in may revoke an invitation made revocable by anyone: their copy publishes
//!   `rev:<identifier>`, the invitation stops counting at once, and the creator's copy
//!   drops it. Every copy tells its game when an invitation it shows is gone
//!   (`invitations_tick`).
//! - A joiner's copy publishes `auth` = "<entity>\n<invitation>" and waits.
//! - The owner's copy judges each attempt once (`judge`): the invitation must be one a
//!   member that is in made, for anyone or for that entity (with PeerParty's PlayFab,
//!   the entity a player's id gives, whatever it claims). Its verdict is a members-only
//!   lobby attribute, `v:<peer id>` = "ok\n<invitation>" or "no\n<invitation>". A member
//!   let in stays in; one refused may try again with another invitation.
//! - Every copy shows a remote member's devices, endpoints and chat controls only once
//!   it is in (`admitted`): the owner, or a member with an "ok" verdict. A modified copy
//!   that skips the step is just never shown, and its messages wait for an endpoint
//!   that never comes.
//! - A member still not in after UNADMITTED_KICK is kicked by the owner (Party drops a
//!   device without an authenticated user after a while too).
//!
//! The joiner's `PartyAuthenticateLocalUserCompleted` comes with the verdict: success, or
//! `UserNotAuthorized` followed by `PartyLocalUserRemoved` (Party.h's documented
//! failure). No verdict within AUTH_WAIT fails as a service error.

use super::*;

/// Member attribute prefix of an invitation a member's user made.
pub(super) const INVITATION_PREFIX: &str = "inv:";
/// Member attribute prefix of another member's invitation a member's user revoked.
const REVOKED_PREFIX: &str = "rev:";
/// Member attribute: what a member authenticates with, "<entity>\n<invitation>".
pub(super) const AUTH_KEY: &str = "auth";
/// Lobby attribute prefix of the owner's verdicts, by member.
const VERDICT_PREFIX: &str = "v:";
/// How long a joiner waits for the owner's verdict.
pub(super) const AUTH_WAIT: Duration = Duration::from_secs(10);
/// How long a member may stay connected without being let in.
pub(super) const UNADMITTED_KICK: Duration = Duration::from_secs(60);
/// The owner's checks run at most this often (each pump otherwise).
const JUDGE_EVERY: Duration = Duration::from_millis(250);

// PARTY_STATE_CHANGE_RESULT (Party.h): the user isn't allowed in (a wrong invitation,
// or one that isn't for them); the service failed to answer.
pub(super) const USER_NOT_AUTHORIZED: u32 = 6;
const PARTY_SERVICE_ERROR: u32 = 4;
// PARTY_INVITATION_REVOCABILITY
pub(super) const REVOCABLE_BY_ANYONE: u32 = 1;
// PARTY_INVITATION_DESTROYED_REASON
const INVITATION_REVOKED: u32 = 0;
// PARTY_LOCAL_USER_REMOVED_REASON
const REMOVED_AUTHENTICATION_FAILED: u32 = 0;

/// A local user's authentication waiting for the owner's verdict.
pub(super) struct Pending {
    pub user: usize,
    pub entity: String,
    pub invitation: String,
    pub async_id: usize,
    pub until: Instant,
    /// `auth` is out in our member attributes.
    pub published: bool,
}

pub(super) fn invitation_attr(id: &str, entities: &[String], revocability: u32) -> Attr {
    Attr {
        key: format!("{INVITATION_PREFIX}{id}"),
        value: Value::Str(format!("{};{}", revocability.min(REVOCABLE_BY_ANYONE), entities.join(","))),
        public: false,
    }
}

/// An invitation as a member's attribute has it.
pub(super) struct Made {
    pub creator: PeerId,
    pub revocability: u32,
    pub entities: Vec<String>,
}

/// The invitation `id` as a member that is in made it, revoked or not.
fn made(l: &Lobby, id: &str) -> Option<Made> {
    let key = format!("{INVITATION_PREFIX}{id}");
    l.members.iter().filter(|m| admitted(l, m.id)).find_map(|m| {
        let v = find_attr(&m.attrs, &key)?.value.text();
        let (r, entities) = v.split_once(';')?;
        Some(Made {
            creator: m.id,
            revocability: if r == "1" { REVOCABLE_BY_ANYONE } else { 0 },
            entities: entities.split(',').filter(|e| !e.is_empty()).map(str::to_string).collect(),
        })
    })
}

/// Whether our own invitation `id` (in `attrs`, our member's) was revoked by another.
fn ours_revoked(l: &Lobby, attrs: &[Attr], id: &str) -> bool {
    find_attr(attrs, &format!("{INVITATION_PREFIX}{id}")).is_some_and(|a| {
        let v = a.value.text();
        let anyone = v.split_once(';').is_some_and(|(r, _)| r == "1");
        revoked(l, id, &Made { creator: PeerId([0; 16]), revocability: if anyone { REVOCABLE_BY_ANYONE } else { 0 }, entities: Vec::new() })
    })
}

/// Whether a member that is in revoked another's invitation that anyone may revoke.
fn revoked(l: &Lobby, id: &str, m: &Made) -> bool {
    let key = format!("{REVOKED_PREFIX}{id}");
    m.revocability == REVOCABLE_BY_ANYONE
        && l.members.iter().any(|r| admitted(l, r.id) && find_attr(&r.attrs, &key).is_some())
}

/// The invitation `id` while it counts: made by a member that is in, not revoked.
pub(super) fn live(l: &Lobby, id: &str) -> Option<Made> {
    made(l, id).filter(|m| !revoked(l, id, m))
}

fn verdict_key(p: PeerId) -> String {
    format!("{VERDICT_PREFIX}{}", p.hex())
}

/// The owner's verdict on a member: (let in, the invitation it was about).
fn verdict(l: &Lobby, p: PeerId) -> Option<(bool, String)> {
    let v = find_attr(&l.attrs, &verdict_key(p))?.value.text();
    let (word, invitation) = v.split_once('\n')?;
    Some((word == "ok", invitation.to_string()))
}

/// Whether a member is in: the owner, or a member the owner let in.
pub(super) fn admitted(l: &Lobby, p: PeerId) -> bool {
    p == l.owner || verdict(l, p).is_some_and(|(ok, _)| ok)
}

/// Whether `entity` gets in with `invitation`: one a member that is in made, for anyone
/// or for that entity.
pub(super) fn valid(l: &Lobby, entity: &str, invitation: &str) -> bool {
    if invitation.is_empty() {
        return false;
    }
    live(l, invitation).is_some_and(|m| m.entities.is_empty() || m.entities.iter().any(|e| e.eq_ignore_ascii_case(entity)))
}

/// The owner's verdicts on the lobby's members as they stand: the lobby's attributes
/// with each new attempt judged and the verdicts of members who left gone. None: no
/// change.
pub(super) fn verdicts(l: &Lobby, me: PeerId, real_entity: impl Fn(PeerId) -> Option<String>) -> Option<Vec<Attr>> {
    let mut attrs = l.attrs.clone();
    let mut changed = false;
    for m in &l.members {
        if m.id == me {
            continue;
        }
        let Some(auth) = find_attr(&m.attrs, AUTH_KEY).map(|a| a.value.text()) else { continue };
        let Some((claimed, invitation)) = auth.split_once('\n') else { continue };
        match verdict(l, m.id) {
            // Let in already, or this attempt judged.
            Some((true, _)) => continue,
            Some((false, judged)) if judged == invitation => continue,
            _ => {}
        }
        let entity = real_entity(m.id).unwrap_or_else(|| claimed.to_string());
        let ok = valid(l, &entity, invitation);
        log!(
            "party-lan {} authenticating in network {} with invitation {invitation}: {}",
            m.id,
            l.id,
            if ok { "let in" } else { "refused (not one of the network's invitations for them)" }
        );
        set_attr(&mut attrs, Attr { key: verdict_key(m.id), value: Value::Str(format!("{}\n{invitation}", if ok { "ok" } else { "no" })), public: false });
        changed = true;
    }
    let before = attrs.len();
    attrs.retain(|a| a.key.strip_prefix(VERDICT_PREFIX).is_none_or(|h| l.members.iter().any(|m| m.id.hex() == h)));
    changed |= attrs.len() != before;
    changed.then_some(attrs)
}

impl State {
    /// The owner's side for the networks we own (verdicts, kicks) and the joiner's for
    /// ours waiting on a verdict. Runs from each pump, at most every JUDGE_EVERY.
    pub(super) fn auth_tick(&mut self, lan: &Lan) {
        let now = Instant::now();
        if self.auth_ticked.is_some_and(|t| now.duration_since(t) < JUDGE_EVERY) {
            return;
        }
        self.auth_ticked = Some(now);
        let nets: Vec<usize> = self.networks.iter().filter(|(h, n)| n.connected && !self.forgotten(**h)).map(|(h, _)| *h).collect();
        let me = lan.me();
        let mut present = HashSet::new();
        for net in nets {
            let guid = self.networks[&net].guid.clone();
            let Some(mut l) = lan.lobby(&guid) else { continue };
            if lan.is_owner(&guid) {
                let serving = crate::https::serving();
                if let Some(attrs) = verdicts(&l, me, |p| serving.then(|| crate::playfab::entity_of(p))) {
                    l.attrs = attrs;
                    if let Err(e) = lan.update(&guid, Some(&l), &[], &[]) {
                        log!("party-lan verdicts on {guid}: {e:?}");
                    }
                }
                for m in &l.members {
                    if m.id == me || admitted(&l, m.id) {
                        continue;
                    }
                    present.insert((guid.clone(), m.id));
                    let since = *self.unadmitted.entry((guid.clone(), m.id)).or_insert(now);
                    if now.duration_since(since) >= UNADMITTED_KICK {
                        log!("party-lan {} connected to network {guid} but wasn't let in within {}s: dropped", m.id, UNADMITTED_KICK.as_secs());
                        let _ = lan.kick(&guid, m.id);
                    }
                }
            }
            self.settle_auth(lan, net, &l);
            self.invitations_tick(lan, net, &l);
        }
        self.unadmitted.retain(|k, _| present.contains(k));
    }

    /// Revokes the invitation `id` in network `guid` for our user: one we made, or one
    /// anyone may revoke. The PARTY_STATE_CHANGE_RESULT.
    pub(super) fn revoke(&mut self, lan: &Lan, guid: &str, id: &str) -> u32 {
        let me = lan.me();
        let Some(l) = lan.lobby(guid) else { return NETWORK_NO_LONGER_EXISTS };
        let key = format!("{INVITATION_PREFIX}{id}");
        let (set, remove) = if l.members.iter().any(|m| m.id == me && find_attr(&m.attrs, &key).is_some()) {
            (Vec::new(), vec![key])
        } else {
            match live(&l, id) {
                Some(m) if m.revocability == REVOCABLE_BY_ANYONE && admitted(&l, me) => {
                    (vec![Attr { key: format!("{REVOKED_PREFIX}{id}"), value: Value::Str(String::new()), public: false }], Vec::new())
                }
                Some(_) => {
                    log!("party-lan revoking invitation {id} in network {guid}: not ours, and only its creator may revoke it");
                    return USER_NOT_AUTHORIZED;
                }
                // Gone already (its creator left, or someone revoked it).
                None => return SUCCEEDED,
            }
        };
        match lan.update(guid, None, &set, &remove) {
            Ok(()) => {
                log!("party-lan revoked invitation {id} in network {guid}");
                SUCCEEDED
            }
            Err(e) => {
                log!("party-lan revoking invitation {id} in network {guid}: {e:?}");
                PARTY_SERVICE_ERROR
            }
        }
    }

    /// Invitations in `net` as they stand: ours that someone revoked are dropped from our
    /// attributes, our revocations of invitations that are gone too, and the game is told
    /// of each invitation it holds that no longer counts (revoked, or its creator left).
    fn invitations_tick(&mut self, lan: &Lan, net: usize, l: &Lobby) {
        let me = lan.me();
        if let Some(mine) = l.members.iter().find(|m| m.id == me) {
            let stale: Vec<String> = mine
                .attrs
                .iter()
                .filter(|a| {
                    if let Some(id) = a.key.strip_prefix(INVITATION_PREFIX) {
                        ours_revoked(l, &mine.attrs, id)
                    } else if let Some(id) = a.key.strip_prefix(REVOKED_PREFIX) {
                        made(l, id).is_none()
                    } else {
                        false
                    }
                })
                .map(|a| a.key.clone())
                .collect();
            if !stale.is_empty() {
                if let Err(e) = lan.update(&l.id, None, &[], &stale) {
                    log!("party-lan dropping revoked invitations on {}: {e:?}", l.id);
                }
            }
        }
        let Some(n) = self.networks.get(&net) else { return };
        let mut gone = Vec::new();
        for h in n.invitations.clone() {
            let Some(i) = self.invitations.get_mut(&h) else { continue };
            if live(l, &i.id).is_some() {
                i.seen = true;
            } else if i.seen {
                gone.push(h);
            }
        }
        for h in gone {
            self.drop_invitation(net, h);
        }
    }

    /// Tells the game an invitation it holds is gone, and forgets it once the change is
    /// returned.
    pub(super) fn drop_invitation(&mut self, net: usize, h: usize) {
        if let Some(n) = self.networks.get_mut(&net) {
            n.invitations.retain(|i| *i != h);
        }
        if let Some(i) = self.invitations.get(&h) {
            log!("party-lan invitation {} gone from network {net:#x}", i.id);
        }
        self.push(InvitationDestroyed { ty: INVITATION_DESTROYED, network: net, invitation: h, reason: INVITATION_REVOKED, detail: 0 });
        self.forget.push(h);
    }

    /// Our user's authentication in `net`, once its `auth` can go out and once the owner
    /// has judged it.
    fn settle_auth(&mut self, lan: &Lan, net: usize, l: &Lobby) {
        let me = lan.me();
        let Some(n) = self.networks.get_mut(&net) else { return };
        let Some(p) = n.auth.as_mut() else { return };
        if !p.published {
            let a = Attr { key: AUTH_KEY.into(), value: Value::Str(format!("{}\n{}", p.entity, p.invitation)), public: false };
            match lan.update(&n.guid, None, &[a], &[]) {
                Ok(()) => p.published = true,
                Err(e) => log!("party-lan authenticating on {}: {e:?}", n.guid),
            }
        }
        let outcome = if l.owner == me {
            // Ours now (made by us, or handed to us): our own invitations decide.
            Some(if valid(l, &p.entity, &p.invitation) { SUCCEEDED } else { USER_NOT_AUTHORIZED })
        } else {
            match verdict(l, me) {
                Some((true, _)) => Some(SUCCEEDED),
                Some((false, judged)) if judged == p.invitation => Some(USER_NOT_AUTHORIZED),
                _ if Instant::now() >= p.until => Some(PARTY_SERVICE_ERROR),
                _ => None,
            }
        };
        if let Some(result) = outcome {
            let p = n.auth.take().unwrap();
            self.auth_done(lan, net, p, result);
        }
    }

    /// Tells the game how its authentication went.
    pub(super) fn auth_done(&mut self, lan: &Lan, net: usize, p: Pending, result: u32) {
        let guid = self.networks.get(&net).map(|n| n.guid.clone()).unwrap_or_default();
        if result == SUCCEEDED {
            log!("party-lan authenticated in network {guid}");
        } else {
            log!("party-lan authentication in network {guid} failed ({result}): invitation {}", p.invitation);
            if let Some(n) = self.networks.get_mut(&net) {
                if n.user == p.user {
                    n.user = 0;
                }
            }
            let _ = lan.update(&guid, None, &[], &[AUTH_KEY.to_string()]);
        }
        self.push_with(
            AuthenticateCompleted {
                ty: AUTHENTICATE_LOCAL_USER_COMPLETED,
                result,
                detail: 0,
                network: net,
                user: p.user,
                invitation: 0,
                async_id: p.async_id,
            },
            Vec::new(),
            Vec::new(),
            Some(cstring(&p.invitation)),
            |c, _, _, t| c.invitation = t,
        );
        if result != SUCCEEDED {
            self.push(LocalUserRemoved { ty: LOCAL_USER_REMOVED, network: net, user: p.user, reason: REMOVED_AUTHENTICATION_FAILED });
        } else {
            self.add_joined_invitation(lan, net, &p.invitation);
            self.reconcile(lan, net);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerparty_core::Member;

    fn peer(b: u8) -> PeerId {
        PeerId([b; 16])
    }

    fn network() -> Lobby {
        let mut l = Lobby::new(NS);
        l.id = "g".into();
        l.owner = peer(1);
        l.members = vec![
            Member { id: peer(1), attrs: vec![invitation_attr("open", &[], 0)] },
            Member { id: peer(2), attrs: vec![invitation_attr("for-3", &["E3".into()], REVOCABLE_BY_ANYONE)] },
        ];
        l
    }

    fn join(l: &mut Lobby, b: u8, entity: &str, invitation: &str) {
        let attrs = vec![Attr { key: AUTH_KEY.into(), value: Value::Str(format!("{entity}\n{invitation}")), public: false }];
        match l.members.iter_mut().find(|m| m.id == peer(b)) {
            Some(m) => m.attrs = attrs,
            None => l.members.push(Member { id: peer(b), attrs }),
        }
    }

    fn judged(l: &mut Lobby, real: impl Fn(PeerId) -> Option<String>) {
        if let Some(a) = verdicts(l, peer(1), real) {
            l.attrs = a;
        }
    }

    #[test]
    fn only_the_networks_invitations_let_a_user_in() {
        let mut l = network();
        assert!(admitted(&l, peer(1)), "the owner is in");
        assert!(!admitted(&l, peer(2)), "a member isn't until let in");
        join(&mut l, 3, "E3", "made-up");
        judged(&mut l, |_| None);
        assert!(!admitted(&l, peer(3)));
        assert_eq!(verdict(&l, peer(3)), Some((false, "made-up".into())));
        // Judged once per attempt: nothing new, no change.
        assert!(verdicts(&l, peer(1), |_| None).is_none());
        // Another try with the owner's invitation gets in.
        join(&mut l, 3, "E3", "open");
        judged(&mut l, |_| None);
        assert!(admitted(&l, peer(3)));
    }

    #[test]
    fn an_invitation_counts_only_from_a_member_that_is_in_and_for_whom_it_is() {
        let mut l = network();
        // Member 2 isn't in yet: its invitation lets nobody in.
        join(&mut l, 3, "E3", "for-3");
        judged(&mut l, |_| None);
        assert!(!admitted(&l, peer(3)));
        join(&mut l, 2, "E2", "open");
        judged(&mut l, |_| None);
        assert!(admitted(&l, peer(2)));
        // Now it does, for the entity it names only.
        join(&mut l, 4, "E4", "for-3");
        judged(&mut l, |_| None);
        assert!(!admitted(&l, peer(4)));
        // A copy claiming E3 for player 5 is judged by the entity its id gives.
        join(&mut l, 5, "E3", "for-3");
        judged(&mut l, |p| Some(format!("E{}", p.0[0])));
        assert!(!admitted(&l, peer(5)));
        assert!(!valid(&l, "E3", ""), "no invitation lets nobody in");
    }

    #[test]
    fn a_member_let_in_stays_in_and_verdicts_go_with_their_member() {
        let mut l = network();
        join(&mut l, 3, "E3", "open");
        judged(&mut l, |_| None);
        join(&mut l, 3, "E3", "made-up");
        judged(&mut l, |_| None);
        assert!(admitted(&l, peer(3)), "a second attempt doesn't undo the first");
        l.members.retain(|m| m.id != peer(3));
        judged(&mut l, |_| None);
        assert!(verdict(&l, peer(3)).is_none());
    }

    #[test]
    fn a_revoked_invitation_lets_nobody_else_in() {
        let rev = |id: &str| Attr { key: format!("rev:{id}"), value: Value::Str(String::new()), public: false };
        let mut l = network();
        join(&mut l, 2, "E2", "open");
        judged(&mut l, |_| None);
        l.members[1].attrs.push(invitation_attr("for-3", &["E3".into()], REVOCABLE_BY_ANYONE));
        assert!(valid(&l, "E3", "for-3"));
        // Member 3 (not in) revoking counts for nothing.
        l.members.push(Member { id: peer(3), attrs: vec![rev("for-3")] });
        assert!(valid(&l, "E3", "for-3"));
        // The owner, in, may revoke it: anyone may.
        l.members[0].attrs.push(rev("for-3"));
        assert!(!valid(&l, "E3", "for-3"));
        assert!(live(&l, "for-3").is_none());
        assert!(ours_revoked(&l, &l.members[1].attrs.clone(), "for-3"), "its creator's copy drops it");
        // The owner's own invitation is the creator's to revoke: another's `rev:` is ignored.
        l.members[1].attrs.push(rev("open"));
        assert!(valid(&l, "E9", "open"));
        assert!(!ours_revoked(&l, &l.members[0].attrs.clone(), "open"));
        let m = live(&l, "open").unwrap();
        assert_eq!((m.creator, m.revocability, m.entities.len()), (peer(1), 0, 0));
    }
}
