//! Lobbies: hosting, search, joins (by a lobby in hand, or by id with the answers
//! settled under one key), the owner's snapshots and what members make of them, host
//! election when an owner goes, and the owner check that keeps one broken link from
//! splitting a lobby (`OWNER_CHECK_WAIT`).

use super::*;

pub(super) const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Role {
    Hosting,
    Joining,
    Joined,
}

pub(super) struct Held {
    pub(super) lobby: Lobby,
    pub(super) role: Role,
    /// A snapshot from the member that would inherit the lobby, saying the owner has
    /// gone, received before we noticed it ourselves, and when: applied if we notice it
    /// soon after (`peer_lost`), so the heir's first State isn't lost.
    pub(super) heir_state: Option<(Lobby, Instant)>,
    /// Our member attributes as we last sent them to the owner, until a State shows
    /// them: if the lobby changes hands first, the old owner may never have had them,
    /// and they go to the new one (M-12).
    pub(super) wanted: Option<Vec<Attr>>,
    /// The lobby's secret key (see `crate::lobby::Claim`): made with the lobby, held by its
    /// members, so whoever owns it next signs its claim.
    pub(super) secret: Option<[u8; 32]>,
}

impl Held {
    pub(super) fn new(lobby: Lobby, role: Role) -> Held {
        Held { lobby, role, heir_state: None, wanted: None, secret: None }
    }

    /// The lobby's claim for a new owner, signed (one term on); unchanged without the
    /// secret.
    pub(super) fn sign_for(&mut self, owner: PeerId) {
        if let Some(secret) = &self.secret {
            self.lobby.claim = crate::lobby::Claim::make(secret, &self.lobby.id, owner, self.lobby.claim.term + 1);
        }
    }

    /// Whether a snapshot from the lobby's owner (or heir) is the lobby's word: a claim
    /// the lobby's key signed for the owner it names, the key we know (if we know one),
    /// and no older than the claim we have.
    pub(super) fn speaks_for(&self, new: &Lobby) -> bool {
        let have = &self.lobby.claim;
        new.claim.holds(&new.id, new.owner) && (have.is_none() || (new.claim.key == have.key && new.claim.term >= have.term))
    }

    /// Takes the secret a State came with, when it's this lobby's.
    pub(super) fn take_secret(&mut self, secret: &[u8; 32]) {
        if *secret != [0; 32] && crate::lobby::lobby_key(secret) == self.lobby.claim.key {
            self.secret = Some(*secret);
        }
    }
}

/// How long an heir's early snapshot is kept for the owner's loss to be noticed here.
pub(super) const HEIR_STATE_KEEP: Duration = Duration::from_secs(15);

/// How long a member that lost a lobby's owner waits for the other members' word
/// (`Msg::OwnerCheck`) before electing a new owner. A break of just its own link to the
/// owner (the owner and the others still reach each other) would otherwise split the
/// lobby once LINK_GRACE is up: the owner drops the member, and the member makes itself
/// or another the owner of a lobby the others still follow the old owner in. Asked,
/// a member that still reaches the owner passes the question on, and the owner's answer
/// (`Msg::OwnerHere`) comes back: the asker is out of the lobby instead. An owner that is
/// really gone doesn't answer, and the election goes ahead as before, this much later.
pub(super) const OWNER_CHECK_WAIT: Duration = Duration::from_secs(3);
/// Owner checks passed on for others at once, at most.
pub(super) const OWNER_CHECKS_RELAYED: usize = 64;

pub(super) struct Search {
    pub(super) query: Query,
    pub(super) awaiting: HashSet<PeerId>,
    pub(super) results: Vec<Lobby>,
    pub(super) deadline: Instant,
}

/// A front end's answer to a join of one of its lobbies that came with `secret` from
/// `joiner`: None lets it in (as far as the front end goes), else the answer it gets.
/// It runs under the engine's lock: it must not call the engine.
pub type JoinCheck = fn(lobby: &Lobby, joiner: PeerId, secret: &str) -> Option<JoinResult>;

/// A public player's member attributes, however often it sends them, go out to the
/// lobby at most this often: each change is a snapshot to every member, so a stranger
/// sending them fast would have us send many times as much.
pub(super) const STRANGER_CAST_EVERY: Duration = Duration::from_millis(250);

/// How long a lobby we handed on as we left it is still pointed to its new owner.
pub(super) const HANDED_KEEP: Duration = Duration::from_secs(30);

/// A join by id under way: see `settle`.
#[derive(Default)]
pub(super) struct ById {
    /// Peers asked that haven't answered yet.
    pub(super) asked: HashSet<PeerId>,
    /// Signed answers: (from, result, claim, whether a public player sent it).
    pub(super) answers: Vec<(PeerId, JoinResult, crate::lobby::Claim, bool)>,
    /// When it is settled with what came, if not everyone answered first.
    pub(super) until: Option<Instant>,
    /// The lobby as each answering player sent it, right behind its answer: applied
    /// once the join is settled on that player (it doesn't come again until something
    /// changes, and without it the lobby we joined is an empty one).
    pub(super) states: Vec<(PeerId, Msg)>,
}

/// How a join by id comes out.
#[derive(Debug, PartialEq)]
pub(super) enum Settled {
    /// Joined (or told "full", "not allowed") by this answer.
    Take(usize),
    /// Answered for under more than one key, by players trusted alike: one of them isn't
    /// its owner and nothing says which, so no answer is taken.
    Disputed,
    /// No signed answer.
    Nobody,
}

/// A join by id, with no key in hand (a typed code, a presence string): the answers of
/// LAN and room players count over public players' (a key is free to make, a stranger
/// can't answer for a friend's lobby; its own lobbies it can), and among the answers that
/// count, all must be under one key: someone answering for a lobby that isn't theirs
/// keeps the join from going through, but can't take it over. Under that key, the latest
/// term's owner (the lobby changing hands as we asked).
pub(super) fn settle(answers: &[(PeerId, JoinResult, crate::lobby::Claim, bool)]) -> Settled {
    let friends = answers.iter().any(|a| !a.3);
    let counted: Vec<usize> = (0..answers.len()).filter(|i| !(friends && answers[*i].3)).collect();
    let Some(&first) = counted.first() else { return Settled::Nobody };
    if counted.iter().any(|i| answers[*i].2.key != answers[first].2.key) {
        return Settled::Disputed;
    }
    let best = counted.iter().copied().fold(first, |b, i| if answers[i].2.term > answers[b].2.term { i } else { b });
    Settled::Take(best)
}

impl Lan {
    // ---- lobbies ----

    /// Creates a lobby we own. An empty id gets a fresh one.
    pub fn create_lobby(&self, mut lobby: Lobby) -> Result<Lobby, LobbyError> {
        let me = self.inner.me;
        let mut s = self.inner.state.lock().unwrap();
        if lobby.id.is_empty() {
            lobby.id = crate::lobby::new_lobby_id();
        }
        if s.lobbies.contains_key(&lobby.id) {
            return Err(LobbyError::AlreadyIn);
        }
        lobby.owner = me;
        let mine = lobby
            .member(me)
            .map(|m| m.attrs.clone())
            .unwrap_or_default();
        lobby.members = vec![Member {
            id: me,
            attrs: mine,
        }];
        let secret = crate::lobby::new_lobby_secret();
        lobby.claim = crate::lobby::Claim::make(&secret, &lobby.id, me, 1);
        s.lobbies.insert(lobby.id.clone(), Held { secret: Some(secret), ..Held::new(lobby.clone(), Role::Hosting) });
        Ok(lobby)
    }

    /// Our current copy of a lobby we're in.
    pub fn lobby(&self, id: &str) -> Option<Lobby> {
        let s = self.inner.state.lock().unwrap();
        s.lobbies
            .get(id)
            .filter(|h| h.role != Role::Joining)
            .map(|h| h.lobby.clone())
    }

    pub fn is_owner(&self, id: &str) -> bool {
        let s = self.inner.state.lock().unwrap();
        s.lobbies.get(id).is_some_and(|h| h.role == Role::Hosting)
    }

    /// Starts a search; `Event::SearchDone` carries the results. Lobbies we host count.
    pub fn search(&self, query: Query) -> u64 {
        let mut s = self.inner.state.lock().unwrap();
        s.next_search += 1;
        let id = s.next_search;
        let max = query.max_results.max(1) as usize;
        let mut results: Vec<Lobby> = s
            .lobbies
            .values()
            .filter(|h| h.role == Role::Hosting && crate::lobby::matches(&h.lobby, &query))
            .map(|h| h.lobby.public_view())
            .take(max)
            .collect();
        results.truncate(max);
        // Everyone is asked, also for one player's lobbies: a lobby is answered for by
        // its owner, and the lobby a player is in is as often someone else's as theirs
        // ("join my friend's game" found nothing when the friend had joined a host).
        // Not a peer whose connection dropped: its answer would come after the timeout.
        let awaiting: HashSet<PeerId> = s.peers.iter().filter(|(_, p)| p.connected()).map(|(id, _)| *id).collect();
        let msg = Msg::Search {
            search: id,
            query: query.clone(),
        };
        for p in &awaiting {
            if let Some(p) = s.peers.get_mut(p) {
                p.post(&msg);
            }
        }
        s.searches.insert(
            id,
            Search {
                query,
                awaiting,
                results,
                deadline: Instant::now() + SEARCH_TIMEOUT,
            },
        );
        if s.searches[&id].awaiting.is_empty() {
            s.finish_search(id);
        }
        id
    }

    /// Joins a lobby a search returned. `Event::JoinDone` says how it went.
    pub fn join(&self, target: &Lobby) -> Result<(), LobbyError> {
        self.join_with(target, "")
    }

    /// Joins with a secret the lobby's owner checks (its password, an invitation).
    pub fn join_with(&self, target: &Lobby, secret: &str) -> Result<(), LobbyError> {
        let mut s = self.inner.state.lock().unwrap();
        if s.lobbies.contains_key(&target.id) {
            return Err(LobbyError::AlreadyIn);
        }
        let Some(owner) = s.peers.get_mut(&target.owner) else {
            return Err(LobbyError::OwnerUnknown);
        };
        owner.post(&Msg::Join {
            lobby: target.id.clone(),
            secret: secret.into(),
        });
        s.remember_secret(&target.id, secret);
        s.lobbies.insert(
            target.id.clone(),
            Held::new(target.clone(), Role::Joining),
        );
        s.joins
            .insert(target.id.clone(), Instant::now() + JOIN_TIMEOUT);
        Ok(())
    }

    /// Joins by id alone: every peer is asked and the owner answers.
    pub fn join_by_id(&self, namespace: &str, id: &str) -> Result<(), LobbyError> {
        self.join_by_id_with(namespace, id, "")
    }

    /// `join_by_id` with a secret the owner checks.
    pub fn join_by_id_with(&self, namespace: &str, id: &str, secret: &str) -> Result<(), LobbyError> {
        let mut s = self.inner.state.lock().unwrap();
        if s.lobbies.contains_key(id) {
            return Err(LobbyError::AlreadyIn);
        }
        let msg = Msg::Join { lobby: id.into(), secret: secret.into() };
        s.remember_secret(id, secret);
        let mut asked = HashSet::new();
        for (pid, p) in &mut s.peers {
            p.post(&msg);
            if p.connected() {
                asked.insert(*pid);
            }
        }
        s.by_id.insert(id.into(), ById { asked, ..ById::default() });
        let mut placeholder = Lobby::new(namespace);
        placeholder.id = id.into();
        s.lobbies.insert(
            id.into(),
            Held::new(placeholder, Role::Joining),
        );
        s.joins.insert(id.into(), Instant::now() + JOIN_TIMEOUT);
        Ok(())
    }

    /// A front end checks the secret joins of its lobbies (namespace `namespace`) come
    /// with, on the owner's PC: what only the owner may decide (a password, an
    /// invitation) isn't left to the joiner's own copy.
    pub fn set_join_check(&self, namespace: &str, check: JoinCheck) {
        self.inner.state.lock().unwrap().join_checks.insert(namespace.into(), check);
    }

    /// Leaves a lobby. The owner leaving hands it on, or closes it.
    pub fn leave(&self, id: &str) -> Result<(), LobbyError> {
        let me = self.inner.me;
        let mut s = self.inner.state.lock().unwrap();
        let Some(held) = s.lobbies.remove(id) else {
            return Err(LobbyError::NotFound);
        };
        self.log_links_as_left(&s, "leaves", id, &held.lobby);
        s.joins.remove(id);
        match held.role {
            Role::Hosting => s.close_hosted(me, held, true),
            _ => s.tell_left(&held.lobby),
        }
        Ok(())
    }

    /// The game leaves (or ends) a lobby with other members in it: how each one's link
    /// stood at that moment goes into the log. A game that gives up on a session itself
    /// (a timeout of its own, an online check that failed) leaves a link that was still
    /// carrying its packets; one that lost its peer leaves one that had gone quiet.
    fn log_links_as_left(&self, s: &State, verb: &str, id: &str, lobby: &Lobby) {
        let now = Instant::now();
        let others: Vec<String> = lobby.members.iter().filter(|m| m.id != self.inner.me).map(|m| s.link_text(m.id, now, false)).collect();
        if others.is_empty() {
            return;
        }
        (self.inner.log)(&format!("lan: the game {verb} lobby {id} with {} other member(s) in it; their links as it did:", others.len()));
        for o in others {
            (self.inner.log)(&format!("lan:   {o}"));
        }
    }

    /// Ends a lobby we own for everyone. For a lobby we only joined it's a leave.
    pub fn destroy(&self, id: &str) -> Result<(), LobbyError> {
        let me = self.inner.me;
        let mut s = self.inner.state.lock().unwrap();
        let Some(held) = s.lobbies.remove(id) else {
            return Err(LobbyError::NotFound);
        };
        self.log_links_as_left(&s, "ends", id, &held.lobby);
        match held.role {
            Role::Hosting => s.close_hosted(me, held, false),
            _ => s.send_to(held.lobby.owner, &Msg::Leave { lobby: id.into() }),
        }
        Ok(())
    }

    /// Applies an edit. The owner may change the lobby's settings and attributes (taken
    /// from `edited`); anyone may change their own member attributes.
    pub fn update(
        &self,
        id: &str,
        edited: Option<&Lobby>,
        member_set: &[Attr],
        member_remove: &[String],
    ) -> Result<(), LobbyError> {
        let me = self.inner.me;
        let mut s = self.inner.state.lock().unwrap();
        let Some(held) = s.lobbies.get_mut(id) else {
            return Err(LobbyError::NotFound);
        };
        let owner = held.role == Role::Hosting;
        if owner {
            if let Some(e) = edited {
                let l = &mut held.lobby;
                l.bucket = e.bucket.clone();
                l.max_members = e.max_members;
                l.permission = e.permission;
                l.allow_invites = e.allow_invites;
                l.attrs = e.attrs.clone();
            }
        }
        let Some(mine) = held.lobby.member_mut(me) else {
            return Err(LobbyError::NotMember);
        };
        for k in member_remove {
            crate::lobby::remove_attr(&mut mine.attrs, k);
        }
        for a in member_set {
            crate::lobby::set_attr(&mut mine.attrs, a.clone());
        }
        let attrs = mine.attrs.clone();
        held.wanted = (!owner).then(|| attrs.clone());
        let lobby = held.lobby.clone();
        if owner {
            s.broadcast(&lobby);
        } else {
            s.send_to(
                lobby.owner,
                &Msg::MemberAttrs {
                    lobby: id.into(),
                    attrs,
                },
            );
        }
        Ok(())
    }

    pub fn kick(&self, id: &str, target: PeerId) -> Result<(), LobbyError> {
        let mut s = self.inner.state.lock().unwrap();
        let held = s.lobbies.get_mut(id).ok_or(LobbyError::NotFound)?;
        if held.role != Role::Hosting {
            return Err(LobbyError::NotOwner);
        }
        if held.lobby.member(target).is_none() || target == self.inner.me {
            return Err(LobbyError::NotMember);
        }
        held.lobby.members.retain(|m| m.id != target);
        let lobby = held.lobby.clone();
        // Our invite doesn't outlast being shown the door.
        if let Some(p) = s.peers.get_mut(&target) {
            p.invited = None;
        }
        s.send_to(target, &Msg::Kicked { lobby: id.into() });
        s.broadcast(&lobby);
        s.events.push_back(Event::MemberStatus {
            lobby: id.into(),
            member: target,
            status: MemberStatus::Kicked,
        });
        Ok(())
    }

    pub fn promote(&self, id: &str, target: PeerId) -> Result<(), LobbyError> {
        let mut s = self.inner.state.lock().unwrap();
        let held = s.lobbies.get_mut(id).ok_or(LobbyError::NotFound)?;
        if held.role != Role::Hosting {
            return Err(LobbyError::NotOwner);
        }
        if held.lobby.member(target).is_none() {
            return Err(LobbyError::NotMember);
        }
        if target != self.inner.me {
            held.lobby.owner = target;
            held.sign_for(target);
            held.role = Role::Joined;
        }
        let lobby = held.lobby.clone();
        s.broadcast(&lobby);
        s.events.push_back(Event::MemberStatus {
            lobby: id.into(),
            member: target,
            status: MemberStatus::Promoted,
        });
        Ok(())
    }

    /// Lobbies in `namespace` we're in (hosting or joined), most useful for "invite
    /// everyone to where I am".
    pub fn my_lobbies(&self, namespace: &str) -> Vec<Lobby> {
        let s = self.inner.state.lock().unwrap();
        s.lobbies
            .values()
            .filter(|h| h.role != Role::Joining && h.lobby.namespace == namespace)
            .map(|h| h.lobby.clone())
            .collect()
    }
}

impl State {
    /// Strangers' joins that waited (JOIN_WAIT): let in if something vouches for them by
    /// now, refused once their wait is up. One for a lobby we no longer host, or from a
    /// player who has gone, has nobody to answer: it goes, or it would hold one of the
    /// JOINS_WAITING places for good.
    pub(super) fn answer_waiting_joins(&mut self, me: PeerId, now: Instant) {
        for (peer, lobby, until) in std::mem::take(&mut self.waiting) {
            if !self.peers.contains_key(&peer) || !self.lobbies.get(&lobby).is_some_and(|h| h.role == Role::Hosting) {
                self.waiting_secrets.remove(&(peer, lobby));
                continue;
            }
            if self.known(peer) || until <= now {
                // Kept in `waiting` so the handler answers this time, one way or the other.
                self.waiting.push((peer, lobby.clone(), until));
                let secret = self.waiting_secrets.remove(&(peer, lobby.clone())).unwrap_or_default();
                self.handle(me, peer, Msg::Join { lobby, secret }, true);
            } else {
                self.waiting.push((peer, lobby, until));
            }
        }
    }

    /// We are out of `lobby` (one we joined, or were still asking to join): its owner
    /// hears it, or everyone does when we asked by id and don't know yet whose it is.
    /// Anyone but the owner of a lobby we're in ignores it.
    pub(super) fn tell_left(&mut self, lobby: &Lobby) {
        let msg = Msg::Leave { lobby: lobby.id.clone() };
        if lobby.owner != PeerId::default() {
            self.send_to(lobby.owner, &msg);
        } else {
            self.post_all(&msg);
        }
    }

    /// Snapshots a public player's attribute changes held back (`STRANGER_CAST_EVERY`),
    /// once their time comes; lobbies no longer ours, and quiet ones, are forgotten.
    pub(super) fn send_owed_casts(&mut self, now: Instant) {
        let due: Vec<String> = self
            .stranger_casts
            .iter()
            .filter(|(_, (at, owed))| *owed && now.duration_since(*at) >= STRANGER_CAST_EVERY)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            if let Some(lobby) = self.lobbies.get(&id).filter(|h| h.role == Role::Hosting).map(|h| h.lobby.clone()) {
                self.broadcast(&lobby);
                self.stranger_casts.insert(id, (now, false));
            }
        }
        let lobbies = &self.lobbies;
        self.stranger_casts.retain(|id, (at, owed)| {
            lobbies.get(id).is_some_and(|h| h.role == Role::Hosting) && (*owed || now.duration_since(*at) < Duration::from_secs(10))
        });
    }

    /// The secret a join of ours goes with, kept while it's under way (bounded: a game
    /// that keeps joining keeps a few).
    pub(super) fn remember_secret(&mut self, id: &str, secret: &str) {
        if secret.is_empty() {
            self.join_secrets.remove(id);
            return;
        }
        if self.join_secrets.len() >= 64 {
            let joins = &self.joins;
            self.join_secrets.retain(|k, _| joins.contains_key(k));
        }
        self.join_secrets.insert(id.into(), secret.into());
    }

    /// The owner's snapshot to every other member.
    /// The lobby's snapshot to its members, with its secret (see `crate::lobby::Claim`).
    pub(super) fn broadcast(&mut self, lobby: &Lobby) {
        let secret = self.lobbies.get(&lobby.id).and_then(|h| h.secret);
        self.broadcast_with(lobby, secret);
    }

    pub(super) fn broadcast_with(&mut self, lobby: &Lobby, secret: Option<[u8; 32]>) {
        let msg = Msg::State { lobby: lobby.clone(), secret: secret.unwrap_or([0; 32]) };
        for m in &lobby.members {
            self.send_to(m.id, &msg);
        }
    }

    pub(super) fn finish_search(&mut self, id: u64) {
        if let Some(search) = self.searches.remove(&id) {
            self.events.push_back(Event::SearchDone {
                search: id,
                results: search.results,
            });
        }
    }

    /// We owned `lobby` and are leaving it: hand it to an heir, or close it.
    /// A lobby we're in changed hands: member attributes we sent that its State doesn't
    /// show yet went to the old owner, which may never have read them. They go to the
    /// new owner, or apply here if that's us. Shown ones need nothing.
    pub(super) fn resend_wanted(&mut self, me: PeerId, id: &str) {
        let Some(held) = self.lobbies.get_mut(id) else { return };
        let Some(want) = held.wanted.take() else { return };
        let Some(mine) = held.lobby.member_mut(me) else { return };
        if mine.attrs == want {
            return;
        }
        if held.role == Role::Hosting {
            mine.attrs = want;
            let lobby = held.lobby.clone();
            self.broadcast(&lobby);
            self.events.push_back(Event::MemberUpdated { lobby: id.into(), member: me });
        } else {
            let owner = held.lobby.owner;
            held.wanted = Some(want.clone());
            self.send_to(owner, &Msg::MemberAttrs { lobby: id.into(), attrs: want });
        }
    }

    pub(super) fn close_hosted(&mut self, me: PeerId, mut held: Held, migrate: bool) {
        let heir = if migrate && held.lobby.host_migration {
            crate::lobby::elect(&held.lobby.members, me)
        } else {
            None
        };
        held.lobby.members.retain(|m| m.id != me);
        match heir {
            Some(h) => {
                held.lobby.owner = h;
                held.sign_for(h);
                let lobby = held.lobby;
                self.handed.insert(lobby.id.clone(), (h, Instant::now() + HANDED_KEEP));
                self.broadcast_with(&lobby, held.secret);
            }
            None => {
                let msg = Msg::Closed {
                    lobby: held.lobby.id.clone(),
                };
                for m in &held.lobby.members {
                    self.send_to(m.id, &msg);
                }
            }
        }
    }

    /// The answer to a join that counts: from the owner we asked, or (asked by id) the
    /// peer taken for the owner.
    /// Settles a join by id with the answers that came (`settle`). Every other player
    /// that let us in hears we aren't coming (or it keeps a member who never arrives).
    /// What it returns is the taken owner's lobby state, for the caller to handle now.
    #[must_use]
    pub(super) fn settle_by_id(&mut self, id: &str) -> Option<(PeerId, Msg)> {
        let Some(mut b) = self.by_id.remove(id) else { return None };
        let asking = self.joins.contains_key(id) && self.lobbies.get(id).is_some_and(|h| h.role == Role::Joining && h.lobby.owner == PeerId::default());
        let answers: Vec<_> = b.answers.into_iter().filter(|a| self.peers.contains_key(&a.0)).collect();
        let settled = if asking { settle(&answers) } else { Settled::Nobody };
        let taken = match settled {
            Settled::Take(i) => Some(answers[i].0),
            _ => None,
        };
        for (from, result, _, _) in &answers {
            if *result == JoinResult::Ok && Some(*from) != taken {
                self.send_to(*from, &Msg::Leave { lobby: id.into() });
            }
        }
        if !asking {
            return None;
        }
        match settled {
            Settled::Take(i) => {
                let (from, result, claim, _) = answers[i];
                self.join_answered(id.into(), from, result, claim);
                if result == JoinResult::Ok {
                    return b.states.iter().position(|(p, _)| *p == from).map(|i| b.states.swap_remove(i));
                }
            }
            Settled::Disputed => {
                self.join_answered(id.into(), PeerId::default(), JoinResult::NotFound, crate::lobby::Claim::default());
            }
            // Everyone said it isn't theirs.
            Settled::Nobody if b.asked.is_empty() => {
                self.join_answered(id.into(), PeerId::default(), JoinResult::NotFound, crate::lobby::Claim::default());
            }
            // Nobody yet: the rest may still answer.
            Settled::Nobody => {
                self.by_id.insert(id.into(), ById { asked: b.asked, states: b.states, ..ById::default() });
            }
        }
        None
    }

    pub(super) fn join_answered(&mut self, id: String, from: PeerId, result: JoinResult, claim: crate::lobby::Claim) {
        self.joins.remove(&id);
        self.join_secrets.remove(&id);
        let Some(held) = self.lobbies.get_mut(&id) else {
            return;
        };
        if result == JoinResult::Ok {
            held.role = Role::Joined;
            held.lobby.owner = from;
            held.lobby.claim = claim;
        } else {
            self.lobbies.remove(&id);
        }
        self.events.push_back(Event::JoinDone { lobby: id, result });
    }

    /// The owner closed the lobby or kicked us out of it.
    pub(super) fn removed(&mut self, me: PeerId, from: PeerId, id: String, status: MemberStatus) {
        let ours = self
            .lobbies
            .get(&id)
            .is_some_and(|h| h.role != Role::Hosting && h.lobby.owner == from);
        if ours {
            self.lobbies.remove(&id);
            self.joins.remove(&id);
            self.events.push_back(Event::MemberStatus {
                lobby: id,
                member: me,
                status,
            });
        }
    }

    /// Events for the difference between two snapshots of a lobby.
    pub(super) fn diff(&mut self, old: &Lobby, new: &Lobby) {
        for m in &new.members {
            match old.member(m.id) {
                None => self.events.push_back(Event::MemberStatus {
                    lobby: new.id.clone(),
                    member: m.id,
                    status: MemberStatus::Joined,
                }),
                Some(was) if was.attrs != m.attrs => self.events.push_back(Event::MemberUpdated {
                    lobby: new.id.clone(),
                    member: m.id,
                }),
                _ => {}
            }
        }
        for m in &old.members {
            if new.member(m.id).is_none() && m.id != old.owner {
                self.events.push_back(Event::MemberStatus {
                    lobby: new.id.clone(),
                    member: m.id,
                    status: MemberStatus::Left,
                });
            }
        }
        if new.owner != old.owner && old.owner != PeerId::default() {
            if new.member(old.owner).is_none() {
                self.events.push_back(Event::MemberStatus {
                    lobby: new.id.clone(),
                    member: old.owner,
                    status: MemberStatus::Left,
                });
            }
            self.events.push_back(Event::MemberStatus {
                lobby: new.id.clone(),
                member: new.owner,
                status: MemberStatus::Promoted,
            });
        }
        self.events.push_back(Event::LobbyUpdated {
            lobby: new.id.clone(),
        });
    }

    pub(super) fn peer_lost(&mut self, me: PeerId, peer: PeerId) {
        let ids: Vec<String> = self.lobbies.keys().cloned().collect();
        for id in ids {
            let Some(held) = self.lobbies.get_mut(&id) else {
                continue;
            };
            if held.role != Role::Hosting && held.lobby.owner == peer {
                if held.role == Role::Joining {
                    self.lobbies.remove(&id);
                    self.joins.remove(&id);
                    self.events.push_back(Event::JoinDone {
                        lobby: id,
                        result: JoinResult::NotFound,
                    });
                    continue;
                }
                // Others we still reach may still reach the owner: they have their say
                // first (OWNER_CHECK_WAIT).
                let others: Vec<PeerId> = held
                    .lobby
                    .members
                    .iter()
                    .map(|m| m.id)
                    .filter(|m| *m != me && *m != peer && self.peers.get(m).is_some_and(Peer::connected))
                    .collect();
                if held.lobby.host_migration && !others.is_empty() {
                    for m in others {
                        self.send_to(m, &Msg::OwnerCheck { lobby: id.clone(), owner: peer });
                    }
                    self.owner_checks.insert(id, (peer, Instant::now() + OWNER_CHECK_WAIT));
                    continue;
                }
                self.owner_gone(me, id, peer);
            } else if held.role == Role::Hosting && held.lobby.member(peer).is_some() {
                held.lobby.members.retain(|m| m.id != peer);
                let lobby = held.lobby.clone();
                self.broadcast(&lobby);
                self.events.push_back(Event::MemberStatus {
                    lobby: id,
                    member: peer,
                    status: MemberStatus::Disconnected,
                });
            }
        }
        self.peer_gone(peer);
    }

    /// A lobby we joined lost its owner `peer` for good: a new owner is elected (or the
    /// lobby closes for us).
    pub(super) fn owner_gone(&mut self, me: PeerId, id: String, peer: PeerId) {
        {
            let Some(held) = self.lobbies.get_mut(&id) else {
                return;
            };
            if held.role != Role::Joined || held.lobby.owner != peer {
                return;
            }
            {
                held.lobby.members.retain(|m| m.id != peer);
                let heir = if held.lobby.host_migration {
                    crate::lobby::elect(&held.lobby.members, peer)
                } else {
                    None
                };
                match heir {
                    None => {
                        self.lobbies.remove(&id);
                        self.events.push_back(Event::MemberStatus {
                            lobby: id,
                            member: me,
                            status: MemberStatus::Closed,
                        });
                    }
                    Some(h) => {
                        held.lobby.owner = h;
                        // The heir's snapshot, if it got here first.
                        if let Some((new, at)) = held.heir_state.take()
                            && new.owner == h
                            && at.elapsed() < HEIR_STATE_KEEP
                            && h != me
                        {
                            if new.member(me).is_some() {
                                let old = std::mem::replace(&mut held.lobby, new.clone());
                                self.events.push_back(Event::MemberStatus {
                                    lobby: id.clone(),
                                    member: peer,
                                    status: MemberStatus::Disconnected,
                                });
                                self.events.push_back(Event::MemberStatus {
                                    lobby: id.clone(),
                                    member: h,
                                    status: MemberStatus::Promoted,
                                });
                                // Both name `h` as the owner: what else changed.
                                self.diff(&old, &new);
                                self.resend_wanted(me, &id);
                                return;
                            }
                        }
                        if h == me {
                            held.role = Role::Hosting;
                            held.sign_for(me);
                            let lobby = held.lobby.clone();
                            self.broadcast(&lobby);
                        }
                        self.events.push_back(Event::MemberStatus {
                            lobby: id.clone(),
                            member: peer,
                            status: MemberStatus::Disconnected,
                        });
                        self.events.push_back(Event::MemberStatus {
                            lobby: id.clone(),
                            member: h,
                            status: MemberStatus::Promoted,
                        });
                        self.resend_wanted(me, &id);
                        self.events.push_back(Event::LobbyUpdated { lobby: id });
                    }
                }
            }
        }
    }

    /// Searches stop waiting for a peer's answer (it's gone, or its connection dropped
    /// and the question with it).
    pub(super) fn unawait(&mut self, peer: PeerId) {
        let searches: Vec<u64> = self
            .searches
            .iter_mut()
            .filter_map(|(id, s)| (s.awaiting.remove(&peer) && s.awaiting.is_empty()).then_some(*id))
            .collect();
        for id in searches {
            self.finish_search(id);
        }
    }
}

impl State {
    /// A lobby message from `from` (`State::handle` passes them on).
    pub(super) fn handle_lobby(&mut self, me: PeerId, from: PeerId, msg: Msg) {
        match msg {
            Msg::Search { search, query } => {
                let max = query.max_results.max(1) as usize;
                // A search by id or by member finds lobbies that aren't public, for a
                // friend following an invite or a friend's game. Not for a stranger:
                // every public player has every other's id.
                let known = self.known(from);
                let lobbies: Vec<Lobby> = self
                    .lobbies
                    .values()
                    .filter(|h| h.role == Role::Hosting && (known || h.lobby.permission.open_to_strangers()) && crate::lobby::matches(&h.lobby, &query))
                    .map(|h| h.lobby.public_view())
                    .take(max)
                    .collect();
                self.send_to(from, &Msg::SearchReply { search, lobbies });
            }
            Msg::SearchReply { search, lobbies } => {
                let Some(s) = self.searches.get_mut(&search) else {
                    return;
                };
                if !s.awaiting.remove(&from) {
                    return;
                }
                let max = s.query.max_results.max(1) as usize;
                for l in lobbies {
                    // A peer only speaks for lobbies it owns, signed (see `crate::lobby::Claim`).
                    if l.owner == from && l.claim.holds(&l.id, from) && crate::lobby::matches(&l, &s.query) && s.results.len() < max {
                        s.results.push(l);
                    }
                }
                if s.awaiting.is_empty() {
                    self.finish_search(search);
                }
            }
            Msg::Join { lobby: id, secret } => {
                // What the game made the lobby (friends only, invite only) holds for
                // strangers: one gets in when invited, or from a lobby we share.
                let known = self.known(from);
                // Not ours (any more): the asker had an old owner. Pointed to the one we
                // know, so it needn't wait out its join (M-12).
                let elsewhere = match self.lobbies.get(&id) {
                    Some(held) if held.role == Role::Joined => Some(held.lobby.owner),
                    Some(_) => None,
                    None => self.handed.get(&id).map(|h| h.0),
                };
                if let Some(owner) = elsewhere {
                    if owner != from && owner != me && owner != PeerId::default() {
                        self.send_to(from, &Msg::JoinElsewhere { lobby: id, owner });
                    }
                    return;
                }
                // Not ours: said so (unsigned), so a join by id needn't wait for us.
                if self.lobbies.get(&id).is_none_or(|h| h.role != Role::Hosting) {
                    self.send_to(from, &Msg::JoinReply { lobby: id, result: JoinResult::NotFound, claim: crate::lobby::Claim::default() });
                    return;
                }
                let Some(held) = self.lobbies.get_mut(&id) else {
                    return;
                };
                // Asked again by a member (its JoinReply was lost): answered, but the
                // others aren't told it joined twice.
                if held.lobby.member(from).is_some() {
                    let claim = held.lobby.claim;
                    self.send_to(from, &Msg::JoinReply { lobby: id, result: JoinResult::Ok, claim });
                    return;
                }
                let allowed = known || held.lobby.permission.open_to_strangers();
                if !allowed && !self.waiting.iter().any(|(p, l, _)| *p == from && *l == id) && self.waiting.len() < JOINS_WAITING {
                    self.waiting_secrets.insert((from, id.clone()), secret);
                    self.waiting.push((from, id, Instant::now() + JOIN_WAIT));
                    return;
                }
                // The front end's say (a password, an invitation), on this PC alone.
                let checked = self.join_checks.get(&held.lobby.namespace).and_then(|check| check(&held.lobby, from, &secret));
                self.waiting.retain(|(p, l, _)| !(*p == from && *l == id));
                let held = self.lobbies.get_mut(&id).unwrap();
                let result = if !allowed {
                    JoinResult::NotAllowed
                } else if let Some(r) = checked {
                    r
                } else if held.lobby.open_slots() == 0 {
                    JoinResult::Full
                } else {
                    held.lobby.members.push(Member {
                        id: from,
                        attrs: Vec::new(),
                    });
                    JoinResult::Ok
                };
                let lobby = held.lobby.clone();
                self.send_to(
                    from,
                    &Msg::JoinReply {
                        lobby: id.clone(),
                        result,
                        claim: lobby.claim,
                    },
                );
                if result == JoinResult::Ok {
                    self.broadcast(&lobby);
                    self.events.push_back(Event::MemberStatus {
                        lobby: id,
                        member: from,
                        status: MemberStatus::Joined,
                    });
                }
            }
            Msg::JoinElsewhere { lobby: id, owner } => {
                // Only from the player we asked, about a join still waiting: asked again
                // there, within the same deadline (a chain of these can't keep it going).
                // To a join by id: a member pointing at the owner, which was asked too.
                if let Some(b) = self.by_id.get_mut(&id) {
                    b.asked.remove(&from);
                    if b.asked.is_empty()
                        && let Some((f, state)) = self.settle_by_id(&id)
                    {
                        self.handle(me, f, state, true);
                    }
                    return;
                }
                let Some(held) = self.lobbies.get_mut(&id) else { return };
                if held.role != Role::Joining || !self.joins.contains_key(&id) || held.lobby.owner != from || owner == me {
                    return;
                }
                let Some(p) = self.peers.get_mut(&owner) else { return };
                held.lobby.owner = owner;
                let secret = self.join_secrets.get(&id).cloned().unwrap_or_default();
                p.post(&Msg::Join { lobby: id, secret });
            }
            Msg::OwnerCheck { lobby: id, owner } => {
                let Some(held) = self.lobbies.get(&id) else { return };
                if held.role == Role::Hosting {
                    // Asked about us by a member that reaches us, for one that doesn't:
                    // we're here.
                    if owner == me && held.lobby.member(from).is_some() {
                        self.send_to(from, &Msg::OwnerHere { lobby: id });
                    }
                    return;
                }
                // A member lost the owner we still reach (as far as we know): the owner
                // answers it through us. An owner that is gone doesn't, and the asker's
                // election goes ahead. The asker may be off our roster already (the
                // owner dropped it as it lost it), so it needn't be on it.
                if held.role != Role::Joined || held.lobby.owner != owner || owner == from {
                    return;
                }
                if !self.peers.get(&owner).is_some_and(Peer::connected) || self.relayed_checks.len() >= OWNER_CHECKS_RELAYED {
                    return;
                }
                if !self.relayed_checks.iter().any(|(l, p, _)| *l == id && *p == from) {
                    self.relayed_checks.push((id.clone(), from, Instant::now() + OWNER_CHECK_WAIT));
                }
                self.send_to(owner, &Msg::OwnerCheck { lobby: id, owner });
            }
            Msg::OwnerHere { lobby: id } => {
                let Some(held) = self.lobbies.get(&id) else { return };
                // The owner's answer to questions we passed on: back to whoever asked.
                if held.role == Role::Joined && held.lobby.owner == from {
                    // Or to ours, asked of it once it was back: we're still in.
                    if self.owners_asked.remove(&id) {
                        self.owner_checks.remove(&id);
                    }
                    let now = Instant::now();
                    let askers: Vec<PeerId> =
                        self.relayed_checks.iter().filter(|(l, _, until)| *l == id && *until > now).map(|(_, p, _)| *p).collect();
                    self.relayed_checks.retain(|(l, _, _)| *l != id);
                    for a in askers {
                        self.send_to(a, &Msg::OwnerHere { lobby: id.clone() });
                    }
                    return;
                }
                // Our own question, answered by a member: the owner is still there for
                // the others and only our link to it broke. The owner has dropped us
                // already (its own LINK_GRACE), so we're out rather than in a lobby of
                // our own.
                let Some((owner, _)) = self.owner_checks.get(&id) else { return };
                if held.role != Role::Joined || held.lobby.owner != *owner || from == *owner || held.lobby.member(from).is_none() {
                    return;
                }
                self.owner_checks.remove(&id);
                self.lobbies.remove(&id);
                self.events.push_back(Event::MemberStatus { lobby: id, member: me, status: MemberStatus::Closed });
            }
            Msg::JoinReply { lobby: id, result, claim } => {
                if !self.joins.contains_key(&id) {
                    // Let into a lobby we stopped asking for (we left, gave up waiting,
                    // or joined by id under another answer): the owner is told, or we'd
                    // stay in it as a ghost.
                    if result == JoinResult::Ok && self.lobbies.get(&id).is_none_or(|h| h.lobby.owner != from) {
                        self.send_to(from, &Msg::Leave { lobby: id });
                    }
                    return;
                }
                let Some(held) = self.lobbies.get(&id) else {
                    return;
                };
                let unknown_owner = held.lobby.owner == PeerId::default();
                if held.role != Role::Joining || !(unknown_owner || held.lobby.owner == from) {
                    return;
                }
                let signed = claim.holds(&id, from);
                // Asked by id: every peer answers, the owner under the lobby's key, the
                // others that it isn't theirs; settled once all have (`settle`).
                if unknown_owner {
                    let Some(b) = self.by_id.get_mut(&id) else { return };
                    b.asked.remove(&from);
                    if signed && !b.answers.iter().any(|a| a.0 == from) {
                        let stranger = self.peers.get(&from).is_none_or(|p| p.stranger);
                        b.answers.push((from, result, claim, stranger));
                        b.until.get_or_insert(Instant::now() + JOIN_CLAIM_WAIT);
                    }
                    if b.asked.is_empty()
                        && let Some((f, state)) = self.settle_by_id(&id)
                    {
                        self.handle(me, f, state, true);
                    }
                    return;
                }
                // Only the lobby's owner answers, under the lobby's key (see
                // `crate::lobby::Claim`). A lobby we know the key of (from a search, an invite)
                // is answered for by its key's holder alone, at once.
                if !signed {
                    return;
                }
                let pinned = held.lobby.claim;
                if pinned.is_none() || (claim.key == pinned.key && claim.term >= pinned.term) {
                    self.join_answered(id, from, result, claim);
                }
            }
            Msg::State { lobby: new, secret } => {
                if self.lobbies.get(&new.id).is_some_and(|h| h.role == Role::Joining && h.lobby.owner == PeerId::default())
                    && let Some(b) = self.by_id.get_mut(&new.id)
                {
                    if b.answers.iter().any(|a| a.0 == from) {
                        b.states.retain(|(p, _)| *p != from);
                        b.states.push((from, Msg::State { lobby: new, secret }));
                    }
                    return;
                }
                let Some(held) = self.lobbies.get_mut(&new.id) else {
                    // Its owner still counts us in a lobby we left: our Leave went to an
                    // owner that handed the lobby on before reading it (M-12).
                    if new.owner == from && new.member(me).is_some() {
                        self.send_to(from, &Msg::Leave { lobby: new.id });
                    }
                    return;
                };
                // The member that inherits the lobby when its owner goes may say so
                // before we notice the owner has gone: kept for when we do (`peer_lost`),
                // never applied while the owner is still here.
                if held.role == Role::Joined
                    && held.lobby.owner != from
                    && held.lobby.host_migration
                    && new.owner == from
                    && new.member(held.lobby.owner).is_none()
                    && crate::lobby::elect(&held.lobby.members, held.lobby.owner) == Some(from)
                    && held.speaks_for(&new)
                {
                    held.heir_state = Some((new, Instant::now()));
                    return;
                }
                // Only the owner we know speaks for the lobby, which also lets it name
                // its successor, and only in the lobby's own name (see `crate::lobby::Claim`).
                if held.role == Role::Hosting || held.lobby.owner != from || !held.speaks_for(&new) {
                    return;
                }
                held.heir_state = None;
                let old_claim = held.lobby.claim;
                let old = std::mem::replace(&mut held.lobby, new.clone());
                if old_claim.is_none() || old_claim.key == new.claim.key {
                    held.take_secret(&secret);
                }
                if held.role == Role::Joined && new.owner == me {
                    held.role = Role::Hosting;
                }
                if new.member(me).is_none() && held.role == Role::Joined {
                    // We were dropped from the roster without a kick or close.
                    self.lobbies.remove(&new.id);
                    self.events.push_back(Event::MemberStatus {
                        lobby: new.id,
                        member: me,
                        status: MemberStatus::Left,
                    });
                    return;
                }
                self.diff(&old, &new);
                if old.owner != new.owner {
                    self.resend_wanted(me, &new.id);
                }
            }
            Msg::Leave { lobby: id } => {
                let Some(held) = self.lobbies.get_mut(&id) else {
                    return;
                };
                if held.role != Role::Hosting || held.lobby.member(from).is_none() {
                    return;
                }
                held.lobby.members.retain(|m| m.id != from);
                let lobby = held.lobby.clone();
                self.broadcast(&lobby);
                self.events.push_back(Event::MemberStatus {
                    lobby: id,
                    member: from,
                    status: MemberStatus::Left,
                });
            }
            Msg::MemberAttrs { lobby: id, attrs } => {
                let Some(held) = self.lobbies.get_mut(&id) else {
                    return;
                };
                if held.role != Role::Hosting {
                    return;
                }
                let Some(m) = held.lobby.member_mut(from) else {
                    return;
                };
                m.attrs = attrs;
                let lobby = held.lobby.clone();
                let stranger = self.peers.get(&from).is_some_and(|p| p.stranger);
                let now = Instant::now();
                match self.stranger_casts.get_mut(&id) {
                    // Sent for a stranger a moment ago: this one waits for `tick`.
                    Some((at, owed)) if stranger && now.duration_since(*at) < STRANGER_CAST_EVERY => *owed = true,
                    _ => {
                        if stranger {
                            self.stranger_casts.insert(id.clone(), (now, false));
                        }
                        self.broadcast(&lobby);
                    }
                }
                self.events.push_back(Event::MemberUpdated {
                    lobby: id,
                    member: from,
                });
            }
            Msg::Closed { lobby: id } => self.removed(me, from, id, MemberStatus::Closed),
            Msg::Kicked { lobby: id } => self.removed(me, from, id, MemberStatus::Kicked),
            _ => {}
        }
    }
}
