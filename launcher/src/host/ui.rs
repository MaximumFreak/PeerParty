//! The overlay's pages, drawn with egui into a software canvas (raster.rs).
//!
//! Built for controllers first: every page is a list of rows, one selected at a time.
//! Up / down (d-pad, stick, arrow keys) moves, south (A / Cross, Enter, Space) does the
//! row's main action, west (X / Square, the Delete key) its second one, left / right
//! changes a setting, the shoulders (Q / E, Page Up / Down) switch pages, east (B /
//! Circle, Escape) goes back and closes. The mouse works too: hovering selects a row,
//! clicking acts.
//!
//! Four pages: Home (what needs you now: what keeps players out, invites, inviting
//! everyone, who's here, how you're connected), Players, Network (this PC's firewall,
//! the home network, the internet) and Settings. A player's profile, the connection
//! details and the opening hotkeys open from them, and B goes back.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Stroke, Vec2};
use serde_json::Value;

use super::pad::Style;
use crate::t;
use crate::ui::fx;
use crate::ui::icons::{self, Glyph, Icon};
use crate::ui::store::Prefs;
use crate::ui::theme::{self, ACCENT, ACCENT_HI, GOOD, TEXT, TEXT2};

const PANEL: Color32 = Color32::from_rgb(19, 21, 28);
const PANEL_TOP: Color32 = Color32::from_rgb(30, 33, 44);
const ROW: Color32 = Color32::from_rgb(30, 34, 44);
const ROW_SEL: Color32 = Color32::from_rgb(52, 88, 172);
const DIM: Color32 = Color32::from_rgb(150, 158, 175);

/// The game's side, from the hook's `state` messages.
#[derive(Default, Clone)]
pub struct GameState {
    pub me: String,
    pub peers: Vec<Peer>,
    pub lobby: Option<LobbyInfo>,
    pub pending: Vec<PendingInvite>,
    pub can_invite: bool,
    /// The game joins rooms by code (Photon): copy and paste codes are offered.
    pub codes: bool,
    /// Why there's nothing to invite to (empty when inviting works).
    pub invite_note: String,
    pub sdks: Vec<String>,
    /// Other stores' overlays loaded in the game ("steam", "epic", "ea"): their hotkeys
    /// open theirs too.
    pub foreign: Vec<String>,
    /// Internet play: a room and / or public sessions (the Internet page).
    pub internet: Vec<NetInfo>,
    /// The internet switch: whether internet play is on, and whether there's a room code
    /// or public sessions for it to switch.
    pub internet_on: bool,
    pub internet_configured: bool,
    /// Players on our network we hear but can't connect to (their firewall, most likely).
    /// None: the game's PeerParty engine isn't running (an older hook doesn't say either).
    pub lan_unreachable: Option<u64>,
}

/// Windows Firewall keeps other players out of the game (or hasn't been told to let
/// them in): the Internet page's top row, which fixes it.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Firewall {
    pub title: String,
    pub detail: String,
}

/// How one kind of internet play is doing. No addresses: the hook sends none, and
/// `no_ip` scrubs any that slipped into a text field.
#[derive(Clone, Default)]
pub struct NetInfo {
    /// "room" or "public".
    pub mode: String,
    /// We know our internet address (STUN or the DHT answered).
    pub online: bool,
    pub nat: String,
    pub port_map: String,
    pub dht_nodes: u64,
    pub found: u64,
    pub blocked: u64,
    /// Connected players: name, round trip in ms (None: not measured yet), resent packets,
    /// the stand-in address games that ask for an IP reach them at, and whether the
    /// tunnel runs over direct IPv6.
    pub peers: Vec<(String, Option<u64>, u64, String, bool)>,
}

/// Replaces anything in `s` that reads as an IP address on the internet with "hidden".
fn no_ip(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let core = w.trim_matches(|c: char| !(c.is_ascii_hexdigit() || c == '.' || c == ':'));
            let ip = core.parse::<std::net::IpAddr>().ok().or_else(|| core.parse::<std::net::SocketAddr>().ok().map(|a| a.ip()));
            match ip {
                Some(ip) if !peerparty_core::inet::redact::is_local(ip) => w.replace(core, "hidden"),
                _ => w.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub status: String,
    pub with_me: bool,
    /// A LAN address, only for a peer on the LAN (empty otherwise; never an internet
    /// address, see `shown_ip`).
    pub ip: String,
    /// Runs the same version of the game.
    pub same_build: bool,
    /// A dedicated server, not a player: listed by its server name, with Join only.
    pub server: bool,
    /// Another copy on this PC (the player's own second copy or server).
    pub this_pc: bool,
    /// How they reach us: "lan", "room" (a room code) or "public" (public sessions).
    pub via: String,
    /// A public player's key fingerprint.
    pub fp: String,
}

impl Peer {
    fn public(&self) -> bool {
        self.via == "public"
    }

    /// The address to show: a LAN peer's private address only. Anything else, or
    /// anything that isn't a private address, shows nothing (belt and braces: the hook
    /// already leaves it empty).
    fn shown_ip(&self) -> Option<String> {
        if !(self.via.is_empty() || self.via == "lan") {
            return None;
        }
        let ip: std::net::IpAddr = self.ip.parse().ok()?;
        peerparty_core::inet::redact::is_local(ip).then(|| ip.to_string())
    }

    /// Where they play from, in a few words (the profile's first row, the "playing too" toast).
    pub fn reach(&self) -> &'static str {
        match self.via.as_str() {
            "public" => t!("overlay.reach_public"),
            "room" => t!("overlay.reach_room"),
            _ => t!("overlay.reach_lan"),
        }
    }

    fn connection(&self) -> String {
        match self.via.as_str() {
            "public" => t!("overlay.conn_public", key = if self.fp.is_empty() { t!("overlay.unknown") } else { &self.fp }),
            "room" => t!("overlay.conn_room").into(),
            _ => match self.shown_ip() {
                Some(ip) => t!("overlay.conn_lan_at", ip = ip),
                None => t!("overlay.conn_lan").into(),
            },
        }
    }
}

#[derive(Clone)]
pub struct LobbyInfo {
    /// The lobby's id (a Photon room's code).
    pub id: String,
    pub kind: String,
    pub max: u32,
    pub members: Vec<(String, String, bool)>,
}

#[derive(Clone)]
pub struct PendingInvite {
    pub id: u64,
    pub name: String,
    pub kind: String,
    /// A room code to type into the game (an invite to a game that joins by code).
    pub code: String,
}

impl GameState {
    /// The invite rows' subtitle when there's nothing to invite to.
    pub fn why_no_invite(&self) -> String {
        if self.invite_note.is_empty() { t!("overlay.host_first").into() } else { self.invite_note.clone() }
    }

    pub fn from_json(v: &Value) -> GameState {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let arr = |v: &Value, k: &str| v.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        let internet_list = arr(v, "internet");
        GameState {
            // An older hook sends neither: on, and set up when it reports any.
            internet_on: v.get("internet_on").and_then(Value::as_bool).unwrap_or(true),
            internet_configured: v.get("internet_configured").and_then(Value::as_bool).unwrap_or(!internet_list.is_empty()),
            lan_unreachable: v.get("lan_unreachable").and_then(Value::as_f64).map(|n| n.max(0.0) as u64),
            me: v.get("me").map(|m| s(m, "name")).unwrap_or_default(),
            peers: arr(v, "peers")
                .iter()
                .map(|p| Peer {
                    id: s(p, "id"),
                    name: s(p, "name"),
                    status: s(p, "status"),
                    with_me: p.get("with_me").and_then(Value::as_bool).unwrap_or(false),
                    ip: s(p, "ip"),
                    same_build: p.get("same_build").and_then(Value::as_bool).unwrap_or(true),
                    server: p.get("server").and_then(Value::as_bool).unwrap_or(false),
                    this_pc: p.get("this_pc").and_then(Value::as_bool).unwrap_or(false),
                    via: s(p, "via"),
                    fp: s(p, "fp"),
                })
                .collect(),
            lobby: v.get("lobby").filter(|l| l.is_object()).map(|l| LobbyInfo {
                id: s(l, "id"),
                kind: s(l, "kind"),
                max: l.get("max").and_then(Value::as_u64).unwrap_or(0) as u32,
                members: arr(l, "members")
                    .iter()
                    .map(|m| (s(m, "id"), s(m, "name"), m.get("owner").and_then(Value::as_bool).unwrap_or(false)))
                    .collect(),
            }),
            pending: arr(v, "pending")
                .iter()
                .map(|p| PendingInvite {
                    id: p.get("id").and_then(Value::as_u64).unwrap_or(0),
                    name: s(p, "name"),
                    kind: s(p, "kind"),
                    code: s(p, "code"),
                })
                .collect(),
            can_invite: v.get("can_invite").and_then(Value::as_bool).unwrap_or(false),
            codes: v.get("codes").and_then(Value::as_bool).unwrap_or(false),
            invite_note: s(v, "invite_note"),
            sdks: arr(v, "sdks").iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            foreign: arr(v, "foreign").iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            internet: internet_list
                .iter()
                .map(|n| {
                    let num = |k: &str| n.get(k).and_then(Value::as_f64).unwrap_or(0.0).max(0.0) as u64;
                    NetInfo {
                        mode: s(n, "mode"),
                        online: n.get("online").and_then(Value::as_bool).unwrap_or(false),
                        nat: no_ip(&s(n, "nat")),
                        port_map: no_ip(&s(n, "port_map")),
                        dht_nodes: num("dht_nodes"),
                        found: num("found"),
                        blocked: num("blocked"),
                        peers: arr(n, "peers")
                            .iter()
                            .map(|p| {
                                let rtt = p.get("rtt_ms").and_then(Value::as_f64).filter(|r| *r >= 0.0).map(|r| r as u64);
                                (
                                    no_ip(&s(p, "name")),
                                    rtt,
                                    p.get("resent").and_then(Value::as_f64).unwrap_or(0.0) as u64,
                                    no_ip(&s(p, "address")),
                                    p.get("ipv6").and_then(Value::as_bool).unwrap_or(false),
                                )
                            })
                            .collect(),
                    }
                })
                .collect(),
        }
    }
}


#[derive(Clone, PartialEq, Debug)]
pub enum Page {
    Home,
    Players,
    Network,
    Settings,
    Profile(String),
    /// Network's connection details: the router, port forwarding, ping per player.
    Details,
    /// Settings' list of what opens the overlay.
    Hotkeys,
}

impl Page {
    /// The tab a page sits under.
    fn tab(&self) -> Page {
        match self {
            Page::Profile(_) => Page::Players,
            Page::Details => Page::Network,
            Page::Hotkeys => Page::Settings,
            p => p.clone(),
        }
    }
}

const TABS: [Page; 4] = [Page::Home, Page::Players, Page::Network, Page::Settings];

fn tab_name(p: &Page) -> &'static str {
    match p {
        Page::Home => t!("overlay.tab_home"),
        Page::Players => t!("overlay.tab_players"),
        Page::Network => t!("overlay.tab_network"),
        _ => t!("overlay.tab_settings"),
    }
}

fn tab_icon(p: &Page) -> Icon {
    match p {
        Page::Home => Icon::Home,
        Page::Players => Icon::People,
        Page::Network => Icon::Wifi,
        _ => Icon::Settings,
    }
}

/// One frame's navigation, as edges (pressed this frame).
#[derive(Default, Clone, Copy, Debug)]
pub struct Nav {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub select: bool,
    pub second: bool,
    pub back: bool,
    pub prev_tab: bool,
    pub next_tab: bool,
    pub close: bool,
}

impl Nav {
    pub fn any(&self) -> bool {
        self.up || self.down || self.left || self.right || self.select || self.second || self.back || self.prev_tab || self.next_tab || self.close
    }
}

/// What the overlay asks of the host.
pub enum Cmd {
    /// A command for the hook (ipc.rs).
    Hook(Value),
    Close,
    /// The global settings changed: save them and tell the hook what it needs.
    Prefs,
    /// Record a controller chord from the buttons held next (mod.rs).
    RecordChord,
    /// Let the game through Windows Firewall (on public networks too, when true).
    FixFirewall(bool),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Action {
    InviteAll,
    /// Copy the room code we're in; type the copied one into the game.
    CopyCode,
    PasteCode,
    Invite(usize),
    Join(usize),
    Profile(usize),
    Block(usize),
    Accept(usize),
    Decline(usize),
    ToggleAutoAccept,
    CycleInviteButtons(i32),
    ToggleToasts,
    ToggleInternet,
    Scale(i32),
    Glyphs(i32),
    Hotkey(usize),
    RecordChord,
    ClearChord,
    FixFirewall(bool),
    Open(PageRef),
    Close,
    None,
}

/// A page a row opens (`Page` holds a String, so isn't Copy).
#[derive(Clone, Copy, PartialEq, Debug)]
enum PageRef {
    Players,
    Network,
    Details,
    Hotkeys,
}

/// How a row reads: plainly, as a problem, or as good news.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tone {
    Plain,
    Warn,
    Good,
}

struct Row {
    title: String,
    detail: String,
    /// A value shown on the right ("On", "Off", ...).
    value: String,
    main: Action,
    second: Action,
    /// left / right
    change: Option<(Action, Action)>,
    main_label: &'static str,
    second_label: &'static str,
    selectable: bool,
    /// A small heading over the rows after it.
    header: bool,
    icon: Option<Icon>,
    tone: Tone,
    /// What the row is about, when its title doesn't say it alone (two invites or two
    /// players can read the same): see `key`.
    id: String,
}

impl Row {
    fn info(title: impl Into<String>, detail: impl Into<String>) -> Row {
        Row {
            title: title.into(),
            detail: detail.into(),
            value: String::new(),
            main: Action::None,
            second: Action::None,
            change: None,
            main_label: "",
            second_label: "",
            selectable: false,
            header: false,
            icon: None,
            tone: Tone::Plain,
            id: String::new(),
        }
    }

    /// The row across state updates: the selection stays on it while rows come and go
    /// round it. Its action can't be the key (`Accept(1)` is another invite once the
    /// first one is gone); its id, or else its title and button, can.
    fn key(&self) -> String {
        if self.id.is_empty() { format!("{}\u{1f}{}", self.title, self.main_label) } else { self.id.clone() }
    }

    fn header(title: impl Into<String>) -> Row {
        Row { header: true, ..Row::info(title, "") }
    }

    fn action(title: impl Into<String>, detail: impl Into<String>, main: Action, main_label: &'static str) -> Row {
        Row { main, main_label, selectable: true, ..Row::info(title, detail) }
    }

    fn setting(title: impl Into<String>, detail: impl Into<String>, value: impl Into<String>, main: Action, change: (Action, Action)) -> Row {
        Row { value: value.into(), main, main_label: t!("overlay.change"), change: Some(change), selectable: true, ..Row::info(title, detail) }
    }

    /// An On / Off setting: Select flips it, and left / right stay free for switching pages.
    fn toggle(title: impl Into<String>, detail: impl Into<String>, on: bool, main: Action) -> Row {
        Row { value: if on { t!("common.on") } else { t!("common.off") }.into(), main, main_label: t!("overlay.change"), selectable: true, ..Row::info(title, detail) }
    }

    fn icon(mut self, icon: Icon, tone: Tone) -> Row {
        self.icon = Some(icon);
        self.tone = tone;
        self
    }

    fn second(mut self, a: Action, label: &'static str) -> Row {
        self.second = a;
        self.second_label = label;
        self
    }

    fn is_on(&self) -> bool {
        self.value == t!("common.on")
    }
}

/// How long Select waits after the selected row went away (`Overlay::sel_moved`).
const MOVED_HOLD: Duration = Duration::from_millis(300);

/// A small left (`dir` -1) or right (1) pointing triangle centred on `c`.
fn arrow(p: &egui::Painter, c: Pos2, dir: f32) {
    let (w, h) = (4.0, 5.0);
    let pts = vec![Pos2::new(c.x + dir * w, c.y), Pos2::new(c.x - dir * w, c.y - h), Pos2::new(c.x - dir * w, c.y + h)];
    p.add(egui::Shape::convex_polygon(pts, TEXT, Stroke::NONE));
}

pub struct Toast {
    pub title: String,
    pub detail: String,
    pub until: Instant,
}

pub struct Overlay {
    pub page: Page,
    /// Opened from the game's invite dialog: the Players page invites.
    pub invite_mode: bool,
    sel: usize,
    scroll_to_sel: bool,
    /// The selected row's `Row::key`, to find it again after a state update.
    sel_key: Option<String>,
    /// When the selected row went away and the selection moved to its neighbour: Select
    /// waits a moment (`MOVED_HOLD`), a press meant for the row that went doesn't act on
    /// the one that took its place.
    sel_moved: Option<Instant>,
    pub state: GameState,
    pub game_name: String,
    pub toasts: Vec<Toast>,
    /// What closes the overlay besides B (the footer).
    pub hotkey_hint: String,
    pub style: Style,
    pub tip: Option<String>,
    /// What just happened (an invite that went or couldn't), shown under the panel.
    pub notice: Option<(String, Instant)>,
    /// The firewall warning (mod.rs checks it), if any.
    pub firewall: Option<Firewall>,
    /// Where the panel was drawn last, in points.
    panel: Option<Rect>,
    /// Whether `frame` paints the panel's background itself (the host draws it once
    /// and keeps it instead: see `background`).
    pub paints_background: bool,
}

/// The notice for someone new in the game: a player "is playing too", a dedicated
/// server "is running", and nothing for another copy on this PC (the player's own).
pub fn arrival_text(p: &Peer) -> Option<String> {
    if p.this_pc && !p.server {
        return None;
    }
    Some(if p.server { t!("toast.server_running", name = p.name) } else { t!("toast.playing_too", name = p.name) })
}

/// An invite's wording from the hook ("invited you", "asks to join you"), translated.
pub fn invite_text(name: &str, kind: &str) -> String {
    match kind {
        "invited you" => t!("overlay.invited_you", name = name),
        "asks to join you" => t!("overlay.asks_to_join", name = name),
        other => format!("{name} {other}"),
    }
}

/// The hook's own notes, translated where the words are known.
pub fn hook_text(s: &str) -> String {
    match s {
        "Invite sent" => t!("hook.invite_sent").into(),
        "Can't invite" => t!("hook.cant_invite").into(),
        "Nobody to invite" => t!("hook.nobody").into(),
        "No other player of this game is running, or they're all with you already"
        | "No other player of this game is running on your network, or they're all with you already" => t!("hook.nobody_detail").into(),
        "A player asked to join you" => t!("hook.asked_to_join").into(),
        "Achievement unlocked" => t!("hook.achievement").into(),
        "Host or join a game first" => t!("overlay.host_first").into(),
        // The hook's invite.rs LEARN_FIRST.
        "Invite someone once from the game's friends list first. After that, Invite and Join work here" => t!("hook.learn_invite").into(),
        // The hook's invite.rs PHOTON_NO_INVITES, with the room after it.
        _ if s.starts_with("This game has no invites: players on your network join your session from the game's own menu (room ") => {
            let room = s.rsplit("(room ").next().unwrap_or_default().trim_end_matches(')');
            t!("hook.photon_room", room = room)
        }
        _ => match s.strip_prefix("To ").and_then(|r| r.split(' ').next()).and_then(|n| n.parse::<u64>().ok()) {
            Some(n) if s.ends_with("player") || s.ends_with("players") => t!("hook.to_players", n = n),
            _ => s.to_string(),
        },
    }
}

impl Overlay {
    pub fn new(game_name: String) -> Overlay {
        Overlay {
            page: Page::Home,
            invite_mode: false,
            sel: 0,
            scroll_to_sel: false,
            sel_key: None,
            sel_moved: None,
            state: GameState::default(),
            game_name,
            toasts: Vec::new(),
            hotkey_hint: "Esc".into(),
            style: Style::Xbox,
            tip: None,
            notice: None,
            firewall: None,
            panel: None,
            paints_background: true,
        }
    }

    pub fn open(&mut self, page: Page, invite_mode: bool) {
        self.page = page;
        self.invite_mode = invite_mode;
        self.sel = 0;
        self.scroll_to_sel = true;
        self.sel_key = None;
        self.sel_moved = None;
    }

    /// Puts the selection back on its row after the rows changed under it (the hook's
    /// state adds and removes invites and problem rows above it). A row that is gone
    /// leaves the selection on the nearest one, for which Select waits a moment.
    fn follow(&mut self, rows: &[Row]) {
        let Some(key) = self.sel_key.as_deref() else { return };
        if rows.get(self.sel).is_some_and(|r| r.selectable && r.key() == key) {
            return;
        }
        if let Some(i) = rows.iter().position(|r| r.selectable && r.key() == key) {
            self.sel = i;
        } else {
            let at = self.sel;
            if let Some(i) = (0..rows.len()).filter(|&i| rows[i].selectable).min_by_key(|&i| i.abs_diff(at)) {
                self.sel = i;
            }
            self.sel_moved = Some(Instant::now());
        }
        self.sel_key = rows.get(self.sel).filter(|r| r.selectable).map(Row::key);
        self.scroll_to_sel = true;
    }

    /// A result to show: under the panel while it's open, else as a notification.
    pub fn note(&mut self, title: &str, detail: &str, visible: bool) {
        let (title, detail) = (hook_text(title), hook_text(detail));
        if visible {
            self.notice = Some((format!("{title}: {detail}"), Instant::now() + Duration::from_secs(6)));
        } else {
            self.toast(title, detail);
        }
    }

    pub fn toast(&mut self, title: impl Into<String>, detail: impl Into<String>) {
        self.toasts.retain(|t| t.until > Instant::now());
        self.toasts.push(Toast { title: title.into(), detail: detail.into(), until: Instant::now() + Duration::from_secs(7) });
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
    }

    pub fn has_toasts(&self) -> bool {
        self.toasts.iter().any(|t| t.until > Instant::now())
    }

    /// Problems that keep other players out: the firewall (fixed from here), players on
    /// this network we can't reach.
    fn problems(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if let Some(fw) = &self.firewall {
            rows.push(
                Row::action(fw.title.clone(), fw.detail.clone(), Action::FixFirewall(false), t!("common.fix"))
                    .second(Action::FixFirewall(true), t!("overlay.fix_public"))
                    .icon(Icon::Shield, Tone::Warn),
            );
        }
        if let Some(n) = self.state.lan_unreachable.filter(|&n| n > 0) {
            rows.push(Row::info(t!("overlay.unreachable", n = n), t!("overlay.unreachable_detail")).icon(Icon::Warning, Tone::Warn));
        }
        rows
    }

    /// Invites waiting for an answer: Accept, or Decline from the second button.
    fn invites(&self) -> Vec<Row> {
        self.state
            .pending
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let row = if !p.code.is_empty() {
                    Row::action(t!("overlay.code_from", name = p.name, code = p.code), t!("overlay.code_answer"), Action::Accept(i), t!("overlay.type_code"))
                        .second(Action::Decline(i), t!("overlay.dismiss"))
                } else {
                    Row::action(invite_text(&p.name, &p.kind), t!("overlay.invite_answer"), Action::Accept(i), t!("overlay.accept")).second(Action::Decline(i), t!("overlay.decline"))
                };
                Row { id: format!("invite {}", p.id), ..row.icon(Icon::Bell, Tone::Good) }
            })
            .collect()
    }

    /// Invite everyone on your network: a button when there's somewhere to invite to,
    /// else just why not (host first, or this game has no invites and how to join it).
    fn invite_all(&self, detail_when_able: &str) -> Row {
        let st = &self.state;
        if !st.can_invite {
            return Row::info(t!("overlay.invite_all"), hook_text(&st.why_no_invite())).icon(Icon::People, Tone::Plain);
        }
        Row::action(t!("overlay.invite_all"), detail_when_able.to_string(), Action::InviteAll, t!("overlay.invite")).icon(Icon::People, Tone::Plain)
    }

    /// Types the room code on the clipboard into the game: closes the overlay first.
    fn paste_row(&self) -> Row {
        Row::action(t!("overlay.paste_code"), t!("overlay.paste_code_detail"), Action::PasteCode, t!("overlay.paste")).icon(Icon::Link, Tone::Plain)
    }

    /// How internet play stands, in one line, for Home.
    fn online_line(&self) -> Row {
        let st = &self.state;
        let (title, detail, tone) = if !st.internet_configured {
            (t!("overlay.online_unset").to_string(), t!("overlay.online_unset_detail").to_string(), Tone::Plain)
        } else if !st.internet_on {
            (t!("overlay.online_paused").to_string(), t!("overlay.online_paused_detail").to_string(), Tone::Plain)
        } else {
            let n: usize = st.internet.iter().map(|n| n.peers.len()).sum();
            let online = st.internet.iter().any(|n| n.online);
            let what = match (st.internet.iter().any(|n| n.mode == "room"), st.internet.iter().any(|n| n.mode == "public")) {
                (true, true) => t!("overlay.via_both"),
                (false, true) => t!("overlay.via_public"),
                _ => t!("overlay.via_room"),
            };
            if online {
                (t!("overlay.online_on", what = what, n = n), t!("overlay.online_on_detail").to_string(), Tone::Good)
            } else {
                (t!("overlay.online_looking", what = what), t!("overlay.online_on_detail").to_string(), Tone::Plain)
            }
        };
        Row::action(title, detail, Action::Open(PageRef::Network), t!("overlay.open")).icon(Icon::Globe, tone)
    }

    fn rows(&self, prefs: &Prefs) -> Vec<Row> {
        let st = &self.state;
        match &self.page {
            Page::Home => {
                let mut rows = self.problems();
                rows.extend(self.invites());
                let room = st.lobby.as_ref().is_some_and(|l| l.kind == "photon.room");
                rows.push(self.invite_all(if room { t!("overlay.invite_code_detail") } else { t!("overlay.invite_all_detail") }));
                if st.codes {
                    rows.push(self.paste_row());
                }
                // Players and dedicated servers are counted apart: a server isn't a player.
                let list = |server: bool| -> Vec<&str> { st.peers.iter().filter(|p| p.server == server).map(|p| p.name.as_str()).collect() };
                let shown = |names: &[&str]| {
                    let shown = names.iter().take(4).copied().collect::<Vec<_>>().join(", ");
                    let more = if names.len() > 4 { t!("overlay.and_more", n = names.len() - 4) } else { String::new() };
                    format!("{shown}{more}")
                };
                let names = list(false);
                let servers = list(true);
                let who = if !names.is_empty() {
                    Row::action(t!("overlay.players_here", n = names.len()), shown(&names), Action::Open(PageRef::Players), t!("overlay.open"))
                } else if servers.is_empty() {
                    Row::action(t!("overlay.nobody_yet"), t!("overlay.nobody_yet_detail"), Action::Open(PageRef::Players), t!("overlay.open"))
                } else {
                    Row::info(t!("overlay.nobody_yet"), t!("overlay.nobody_yet_detail"))
                };
                rows.push(who.icon(Icon::People, if names.is_empty() { Tone::Plain } else { Tone::Good }));
                if !servers.is_empty() {
                    let row = Row::action(t!("overlay.servers_here", n = servers.len()), shown(&servers), Action::Open(PageRef::Players), t!("overlay.open"));
                    rows.push(Row { id: "servers here".into(), ..row.icon(Icon::Home, Tone::Good) });
                }
                let lobby = match &st.lobby {
                    Some(l) if l.kind == "photon.room" => {
                        let members: Vec<String> = l.members.iter().map(|(_, n, owner)| if *owner { t!("overlay.host_name", name = n) } else { n.clone() }).collect();
                        let title = if l.max > 0 {
                            t!("overlay.in_room_max", code = l.id, n = l.members.len(), max = l.max)
                        } else {
                            t!("overlay.in_room", code = l.id, n = l.members.len())
                        };
                        Row::action(title, members.join(", "), Action::CopyCode, t!("overlay.copy_code"))
                    }
                    Some(l) => {
                        let members: Vec<String> = l.members.iter().map(|(_, n, owner)| if *owner { t!("overlay.host_name", name = n) } else { n.clone() }).collect();
                        let mut detail = members.join(", ");
                        if let Some(kind) = lobby_kind(&l.kind) {
                            detail.push_str(&format!("  \u{b7}  {kind}"));
                        }
                        Row::info(t!("overlay.in_lobby", n = l.members.len(), max = l.max.max(l.members.len() as u32)), detail)
                    }
                    None => Row::info(t!("overlay.no_lobby"), t!("overlay.no_lobby_detail")),
                };
                rows.push(lobby.icon(Icon::Gamepad, Tone::Plain));
                rows.push(self.online_line());
                if !st.sdks.is_empty() {
                    rows.push(Row::info(t!("overlay.through"), st.sdks.iter().map(|s| sdk_label(s)).collect::<Vec<_>>().join(", ")).icon(Icon::Link, Tone::Plain));
                }
                rows.push(Row::action(t!("overlay.back_to_game"), "", Action::Close, t!("overlay.close")));
                rows
            }
            Page::Players => {
                let mut rows = Vec::new();
                if self.invite_mode {
                    rows.push(self.invite_all(t!("overlay.invite_asked")));
                }
                rows.extend(self.invites());
                if st.codes {
                    rows.push(self.paste_row());
                }
                if st.peers.is_empty() {
                    rows.push(Row::info(t!("overlay.no_players"), t!("overlay.no_players_detail")));
                }
                for (i, p) in st.peers.iter().enumerate() {
                    // A server can't be invited anywhere.
                    if p.server && self.invite_mode {
                        continue;
                    }
                    let mut detail = if p.server {
                        t!("overlay.dedicated_server").to_string()
                    } else if p.with_me {
                        t!("overlay.in_your_lobby").to_string()
                    } else if p.status.is_empty() {
                        t!("overlay.playing").to_string()
                    } else {
                        p.status.clone()
                    };
                    if p.public() {
                        detail.push_str(&format!("  \u{b7}  {}", t!("overlay.public_player")));
                    } else if p.via == "room" {
                        detail.push_str(&format!("  \u{b7}  {}", t!("overlay.your_room")));
                    } else if let Some(ip) = p.shown_ip() {
                        detail.push_str(&format!("  \u{b7}  {ip}"));
                    }
                    if !p.same_build && !p.server {
                        detail.push_str(&format!("  \u{b7}  {}", t!("overlay.other_version")));
                    }
                    let icon = if p.public() { Icon::Globe } else if p.via == "room" { Icon::People } else { Icon::Home };
                    // A joins them (X invites them here); opened from the game's own
                    // invite button, A invites; someone already in your lobby, A shows
                    // their details.
                    let r = if p.server && p.public() {
                        Row::action(p.name.clone(), detail, Action::Join(i), t!("overlay.join")).second(Action::Profile(i), t!("overlay.details"))
                    } else if p.server {
                        // Join asks the game to join the server (the hook's steamgs.rs).
                        Row::action(p.name.clone(), detail, Action::Join(i), t!("overlay.join"))
                    } else if p.with_me {
                        Row::action(p.name.clone(), detail, Action::Profile(i), t!("overlay.details"))
                    } else if self.invite_mode {
                        Row::action(p.name.clone(), detail, Action::Invite(i), t!("overlay.invite"))
                    } else if p.public() {
                        // Strangers: their details (and Block) are one button away.
                        Row::action(p.name.clone(), detail, Action::Join(i), t!("overlay.join")).second(Action::Profile(i), t!("overlay.details"))
                    } else {
                        Row::action(p.name.clone(), detail, Action::Join(i), t!("overlay.join")).second(Action::Invite(i), t!("overlay.invite"))
                    };
                    rows.push(Row { id: format!("peer {}", p.id), ..r.icon(icon, if p.with_me { Tone::Good } else { Tone::Plain }) });
                }
                rows
            }
            Page::Network => {
                let mut rows = Vec::new();
                let problems = self.problems();
                if !problems.is_empty() || st.lan_unreachable.is_some() {
                    rows.push(Row::header(t!("overlay.net_pc")));
                }
                if problems.is_empty() && st.lan_unreachable.is_some() {
                    rows.push(Row::info(t!("overlay.net_fw_ok"), t!("overlay.net_fw_ok_detail")).icon(Icon::Shield, Tone::Good));
                }
                rows.extend(problems);
                rows.push(Row::header(t!("overlay.net_home")));
                let near = st.peers.iter().filter(|p| p.via.is_empty() || p.via == "lan").count();
                rows.push(Row::info(t!("overlay.net_near", n = near), t!("overlay.net_near_detail")).icon(Icon::Home, if near > 0 { Tone::Good } else { Tone::Plain }));
                rows.push(Row::header(t!("overlay.net_internet")));
                // The switch has no left / right value, so those still switch pages from it.
                if !st.internet_configured {
                    rows.push(Row::info(t!("overlay.net_unset"), t!("overlay.net_unset_detail")).icon(Icon::Globe, Tone::Plain));
                } else {
                    rows.push(
                        Row::toggle(t!("overlay.net_switch"), if st.internet_on { t!("overlay.net_switch_on") } else { t!("overlay.net_switch_off") }, st.internet_on, Action::ToggleInternet)
                            .icon(Icon::Globe, if st.internet_on { Tone::Good } else { Tone::Plain }),
                    );
                }
                if st.internet_on {
                    for n in &st.internet {
                        let title = if n.mode == "public" { t!("overlay.net_public", n = n.peers.len()) } else { t!("overlay.net_room", n = n.peers.len()) };
                        let detail = if n.online { t!("overlay.net_online", nat = if n.nat.is_empty() { t!("overlay.unknown").to_string() } else { n.nat.clone() }) } else { t!("overlay.net_looking").to_string() };
                        rows.push(Row::info(title, detail).icon(if n.mode == "public" { Icon::Globe } else { Icon::People }, if n.online { Tone::Good } else { Tone::Plain }));
                    }
                    if !st.internet.is_empty() {
                        rows.push(Row::action(t!("overlay.details_title"), t!("overlay.details_sub"), Action::Open(PageRef::Details), t!("overlay.open")).icon(Icon::Info, Tone::Plain));
                    }
                }
                rows.push(Row::action(t!("overlay.back_to_game"), t!("overlay.no_addresses"), Action::Close, t!("overlay.close")));
                rows
            }
            Page::Details => {
                let mut rows = Vec::new();
                for n in &st.internet {
                    rows.push(Row::header(if n.mode == "public" { t!("overlay.via_public") } else { t!("overlay.via_room") }));
                    rows.push(Row::info(t!("overlay.d_router"), if n.nat.is_empty() { t!("overlay.unknown").to_string() } else { n.nat.clone() }));
                    rows.push(Row::info(t!("overlay.d_ports"), if n.port_map.is_empty() { t!("overlay.d_ports_none").to_string() } else { n.port_map.clone() }));
                    rows.push(Row::info(t!("overlay.d_dht"), t!("overlay.d_dht_detail", nodes = n.dht_nodes, found = n.found)));
                    if n.blocked > 0 {
                        rows.push(Row::info(t!("overlay.d_blocked"), n.blocked.to_string()));
                    }
                    for (name, rtt, resent, address, v6) in &n.peers {
                        let ping = rtt.map_or(t!("overlay.d_no_ping").to_string(), |r| t!("overlay.d_ping", ms = r));
                        let ping = if *v6 { t!("overlay.d_ipv6", ping = ping) } else { ping };
                        let lost = if *resent > 0 { t!("overlay.d_resent", n = resent) } else { String::new() };
                        let ip = if address.is_empty() || address == "hidden" { String::new() } else { t!("overlay.d_ip_to_type", ip = address) };
                        rows.push(Row::info(format!("    {name}"), format!("{ping}{lost}{ip}")));
                    }
                }
                rows.push(Row::action(t!("common.back"), t!("overlay.no_addresses"), Action::Open(PageRef::Network), t!("common.back")));
                rows
            }
            Page::Settings => {
                let hk = &prefs.hotkeys;
                let keys: Vec<&str> = [(hk.shift_tab, "Shift+Tab"), (hk.shift_f3, "Shift+F3"), (hk.shift_f1, "Shift+F1"), (hk.shift_f2, "Shift+F2")].iter().filter(|k| k.0).map(|k| k.1).collect();
                let mut open_with = keys.join(", ");
                if hk.guide {
                    if !open_with.is_empty() {
                        open_with.push_str(", ");
                    }
                    open_with.push_str(t!("overlay.home_button"));
                }
                vec![
                    // The top row has no left / right value, so those still switch pages
                    // from the top of the list.
                    Row::toggle(t!("settings.toasts"), t!("overlay.s_toasts_detail"), prefs.toasts, Action::ToggleToasts),
                    Row::toggle(t!("together.auto_accept"), t!("overlay.s_auto_accept_detail"), prefs.auto_accept, Action::ToggleAutoAccept),
                    Row::setting(
                        t!("together.invite_buttons"),
                        t!("overlay.s_invite_buttons_detail"),
                        match prefs.invite_buttons.as_str() {
                            "guide" => t!("together.ib_guide"),
                            "combo" => t!("together.ib_combo"),
                            "both" => t!("together.ib_both"),
                            _ => t!("together.ib_none"),
                        },
                        Action::CycleInviteButtons(1),
                        (Action::CycleInviteButtons(-1), Action::CycleInviteButtons(1)),
                    ),
                    Row::header(t!("settings.look")),
                    Row::setting(t!("settings.size"), "", format!("{:.0}%", prefs.overlay_scale * 100.0), Action::Scale(1), (Action::Scale(-1), Action::Scale(1))),
                    Row::setting(
                        t!("settings.glyphs"),
                        "",
                        match prefs.glyphs.as_str() {
                            "xbox" => "Xbox",
                            "playstation" => "PlayStation",
                            "nintendo" => "Nintendo",
                            _ => t!("settings.glyphs_auto"),
                        },
                        Action::Glyphs(1),
                        (Action::Glyphs(-1), Action::Glyphs(1)),
                    ),
                    Row::action(t!("settings.open_with"), if open_with.is_empty() { t!("overlay.none").to_string() } else { open_with }, Action::Open(PageRef::Hotkeys), t!("overlay.open")).icon(Icon::Keyboard, Tone::Plain),
                    Row::info(t!("overlay.your_name", name = st.me), t!("overlay.your_name_detail")).icon(Icon::Person, Tone::Plain),
                ]
            }
            Page::Hotkeys => {
                let hk = &prefs.hotkeys;
                vec![
                    Row::toggle("Shift+Tab", t!("settings.key_steam"), hk.shift_tab, Action::Hotkey(0)),
                    Row::toggle("Shift+F3", t!("settings.key_epic"), hk.shift_f3, Action::Hotkey(1)),
                    Row::toggle("Shift+F1", t!("settings.key_ea"), hk.shift_f1, Action::Hotkey(2)),
                    Row::toggle("Shift+F2", t!("settings.key_ubisoft"), hk.shift_f2, Action::Hotkey(3)),
                    Row::toggle(t!("settings.key_guide"), t!("settings.key_guide_sub"), hk.guide, Action::Hotkey(4)),
                    Row {
                        value: if hk.chord.count_ones() >= 2 { super::pad::names(hk.chord) } else { t!("overlay.none").into() },
                        main: Action::RecordChord,
                        main_label: t!("overlay.chord_set"),
                        second: Action::ClearChord,
                        second_label: t!("overlay.none"),
                        selectable: true,
                        ..Row::info(t!("settings.chord"), t!("overlay.chord_detail"))
                    },
                ]
            }
            Page::Profile(id) => {
                let Some((i, p)) = st.peers.iter().enumerate().find(|(_, p)| &p.id == id) else {
                    return vec![Row::info(t!("overlay.left"), "")];
                };
                let mut rows = vec![Row::info(p.name.clone(), if p.with_me { t!("overlay.in_your_lobby") } else { p.reach() }), Row::info(t!("overlay.connection"), p.connection())];
                if !p.status.is_empty() {
                    rows.push(Row::info(t!("overlay.status"), p.status.clone()));
                }
                rows.push(Row::action(t!("overlay.invite_to_yours"), "", Action::Invite(i), t!("overlay.invite")));
                rows.push(Row::action(t!("overlay.join_theirs"), "", Action::Join(i), t!("overlay.join")));
                if p.public() {
                    rows.push(Row::action(t!("overlay.block"), t!("overlay.block_detail"), Action::Block(i), t!("overlay.block_button")).icon(Icon::Shield, Tone::Warn));
                }
                rows
            }
        }
    }

    fn act(&mut self, a: Action, prefs: &mut Prefs, out: &mut Vec<Cmd>) {
        let st = &self.state;
        let msg = |v: Value| Cmd::Hook(v);
        match a {
            Action::InviteAll => out.push(msg(serde_json::json!({"t": "invite", "to": null}))),
            Action::CopyCode => {
                if let Some(l) = st.lobby.as_ref().filter(|l| l.kind == "photon.room") {
                    out.push(msg(serde_json::json!({"t": "copy", "text": l.id})));
                }
            }
            Action::PasteCode => {
                out.push(msg(serde_json::json!({"t": "paste"})));
                out.push(Cmd::Close);
            }
            Action::Invite(i) => {
                if let Some(p) = st.peers.get(i) {
                    out.push(msg(serde_json::json!({"t": "invite", "to": p.id})));
                }
            }
            Action::Join(i) => {
                if let Some(p) = st.peers.get(i) {
                    out.push(msg(serde_json::json!({"t": "join", "peer": p.id})));
                    out.push(Cmd::Close);
                }
            }
            Action::Profile(i) => {
                if let Some(p) = st.peers.get(i) {
                    let id = p.id.clone();
                    self.open(Page::Profile(id), false);
                }
            }
            Action::Block(i) => {
                if let Some(p) = st.peers.get(i).filter(|p| p.public()) {
                    out.push(msg(serde_json::json!({"t": "block", "peer": p.id})));
                    self.open(Page::Players, false);
                }
            }
            Action::Accept(i) => {
                if let Some(p) = st.pending.get(i) {
                    out.push(msg(serde_json::json!({"t": "accept", "id": p.id})));
                    out.push(Cmd::Close);
                }
            }
            Action::Decline(i) => {
                if let Some(p) = st.pending.get(i) {
                    out.push(msg(serde_json::json!({"t": "decline", "id": p.id})));
                }
            }
            Action::Open(to) => {
                let page = match to {
                    PageRef::Players => Page::Players,
                    PageRef::Network => Page::Network,
                    PageRef::Details => Page::Details,
                    PageRef::Hotkeys => Page::Hotkeys,
                };
                self.open(page, false);
            }
            Action::ToggleAutoAccept => {
                prefs.auto_accept = !prefs.auto_accept;
                out.push(Cmd::Prefs);
            }
            Action::CycleInviteButtons(d) => {
                let opts = ["", "guide", "combo", "both"];
                let i = opts.iter().position(|o| *o == prefs.invite_buttons).unwrap_or(0) as i32;
                prefs.invite_buttons = opts[(i + d).rem_euclid(4) as usize].to_string();
                out.push(Cmd::Prefs);
            }
            Action::ToggleToasts => {
                prefs.toasts = !prefs.toasts;
                out.push(Cmd::Prefs);
            }
            Action::ToggleInternet => {
                // Flips what the game reports (an environment value may differ from the
                // saved setting), saves it for next time, and switches the running game.
                let on = !st.internet_on;
                prefs.internet = on;
                out.push(Cmd::Prefs);
                out.push(msg(serde_json::json!({"t": "set", "key": "internet", "value": if on { "on" } else { "off" }})));
                self.state.internet_on = on;
            }
            Action::Scale(d) => {
                prefs.overlay_scale = (prefs.overlay_scale + d as f32 * 0.125).clamp(0.75, 1.5);
                out.push(Cmd::Prefs);
            }
            Action::Glyphs(d) => {
                let opts = ["auto", "xbox", "playstation", "nintendo"];
                let i = opts.iter().position(|o| *o == prefs.glyphs).unwrap_or(0) as i32;
                prefs.glyphs = opts[(i + d).rem_euclid(4) as usize].to_string();
                out.push(Cmd::Prefs);
            }
            Action::Hotkey(i) => {
                let hk = &mut prefs.hotkeys;
                let b = match i {
                    0 => &mut hk.shift_tab,
                    1 => &mut hk.shift_f3,
                    2 => &mut hk.shift_f1,
                    3 => &mut hk.shift_f2,
                    _ => &mut hk.guide,
                };
                *b = !*b;
                out.push(Cmd::Prefs);
            }
            Action::RecordChord => out.push(Cmd::RecordChord),
            Action::FixFirewall(public) => out.push(Cmd::FixFirewall(public)),
            Action::ClearChord => {
                prefs.hotkeys.chord = 0;
                out.push(Cmd::Prefs);
            }
            Action::Close => out.push(Cmd::Close),
            Action::None => {}
        }
    }

    fn glyph_style(&self, prefs: &Prefs) -> Style {
        match prefs.glyphs.as_str() {
            "xbox" => Style::Xbox,
            "playstation" => Style::PlayStation,
            "nintendo" => Style::Nintendo,
            _ => self.style,
        }
    }

    /// Where B goes from a page: a sub-page back to its tab, a tab closes.
    fn parent(&self) -> Option<Page> {
        match self.page {
            Page::Profile(_) => Some(Page::Players),
            Page::Details => Some(Page::Network),
            Page::Hotkeys => Some(Page::Settings),
            _ => None,
        }
    }

    /// One frame of the full overlay. `screen` is the game's client area in points.
    pub fn frame(&mut self, ui: &mut egui::Ui, mut nav: Nav, prefs: &mut Prefs, out: &mut Vec<Cmd>) {
        let screen = ui.max_rect();
        // A pass egui runs again (`request_discard` below) has the same input: it was
        // already acted on.
        if ui.ctx().current_pass_index() > 0 {
            nav = Nav::default();
        }
        let rows = self.rows(prefs);
        self.follow(&rows);
        if self.sel_moved.is_some_and(|t| t.elapsed() < MOVED_HOLD) {
            nav.select = false;
            nav.second = false;
        }
        // Left / right change the selected row's value when it has one, and switch pages
        // otherwise.
        if nav.left || nav.right {
            let rows = self.rows(prefs);
            let changes = rows.get(self.sel).is_some_and(|r| r.selectable && r.change.is_some());
            if !changes {
                nav.prev_tab |= nav.left;
                nav.next_tab |= nav.right;
                nav.left = false;
                nav.right = false;
            }
        }
        // Page switching and closing.
        let tab_i = TABS.iter().position(|p| *p == self.page.tab()).unwrap_or(0);
        if nav.next_tab || nav.prev_tab {
            let n = TABS.len() as i32;
            let i = (tab_i as i32 + if nav.next_tab { 1 } else { -1 }).rem_euclid(n) as usize;
            self.open(TABS[i].clone(), false);
        }
        // The tab to highlight is the one switched to just now (frames are only drawn
        // on input, so a stale one would stay until the next button press).
        let tab_i = TABS.iter().position(|p| *p == self.page.tab()).unwrap_or(0);
        if nav.close {
            out.push(Cmd::Close);
        }
        if nav.back {
            match self.parent() {
                Some(p) => self.open(p, false),
                None => out.push(Cmd::Close),
            }
        }
        let rows = self.rows(prefs);
        let selectable: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r.selectable).map(|(i, _)| i).collect();
        if !selectable.contains(&self.sel) {
            self.sel = selectable.first().copied().unwrap_or(0);
        }
        let pos = selectable.iter().position(|&i| i == self.sel).unwrap_or(0);
        if nav.down && pos + 1 < selectable.len() {
            self.sel = selectable[pos + 1];
            self.scroll_to_sel = true;
        }
        if nav.up && pos > 0 {
            self.sel = selectable[pos - 1];
            self.scroll_to_sel = true;
        }
        // What the buttons do is done before drawing, so this frame already shows it (a
        // new value, the page it opened): frames are only drawn on input.
        let mut nav_action = Action::None;
        if let Some(r) = rows.get(self.sel).filter(|r| r.selectable) {
            if nav.select {
                nav_action = r.main;
            } else if nav.second {
                nav_action = r.second;
            } else if nav.left {
                nav_action = r.change.map(|c| c.0).unwrap_or(Action::None);
            } else if nav.right {
                nav_action = r.change.map(|c| c.1).unwrap_or(Action::None);
            }
        }
        let rows = if nav_action != Action::None {
            self.act(nav_action, prefs, out);
            let rows = self.rows(prefs);
            let selectable: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r.selectable).map(|(i, _)| i).collect();
            if !selectable.contains(&self.sel) {
                self.sel = selectable.first().copied().unwrap_or(0);
            }
            rows
        } else {
            rows
        };
        let sel_before = self.sel;
        let mut pending_action = Action::None;

        // The panel: a lit card over the dimmed game.
        let panel = panel_rect(screen);
        self.panel = Some(panel);
        // A click on the dimmed game around the panel closes the overlay.
        if ui.input(|i| i.pointer.any_pressed() && i.pointer.press_origin().is_some_and(|p| !panel.contains(p))) {
            out.push(Cmd::Close);
        }
        let painter = ui.painter().clone();
        if self.paints_background {
            painter.extend(background(panel));
        }
        let inner = panel.shrink2(Vec2::new(26.0, 20.0));
        // Header: the mark, the game, you.
        let mark = Rect::from_min_size(inner.left_top() + Vec2::new(0.0, 2.0), Vec2::splat(38.0));
        brand(&painter, mark);
        painter.text(mark.right_top() + Vec2::new(12.0, 0.0), Align2::LEFT_TOP, "PeerParty", theme::strong(12.0), ACCENT_HI);
        let title = painter.layout_no_wrap(self.game_name.clone(), theme::strong(20.0), TEXT);
        let tw = (inner.width() - 250.0).max(100.0);
        painter.with_clip_rect(Rect::from_min_size(mark.right_top() + Vec2::new(12.0, 14.0), Vec2::new(tw, 30.0))).galley(mark.right_top() + Vec2::new(12.0, 14.0), title, TEXT);
        if !self.state.me.is_empty() {
            let t = painter.text(inner.right_top() + Vec2::new(0.0, 12.0), Align2::RIGHT_CENTER, &self.state.me, theme::strong(13.5), TEXT2);
            Icon::Person.paint(&painter, Pos2::new(t.left() - 14.0, t.center().y), 14.0, DIM);
        }
        // Tabs, with LB / RB either side.
        let style = self.glyph_style(prefs);
        let tabs_y = inner.top() + 58.0;
        let mut x = inner.left();
        icons::glyph(&painter, Pos2::new(x + 13.0, tabs_y + 16.0), Glyph::Lb, style);
        x += 34.0;
        for (i, page) in TABS.iter().enumerate() {
            let name = tab_name(page);
            let badge = match page {
                Page::Players if !self.state.pending.is_empty() => Some((self.state.pending.len().to_string(), GOOD)),
                Page::Network if self.firewall.is_some() || self.state.lan_unreachable.is_some_and(|n| n > 0) => Some(("!".to_string(), theme::WARN)),
                _ => None,
            };
            let tw = painter.layout_no_wrap(name.to_string(), theme::strong(14.0), TEXT).size().x;
            let w = 44.0 + tw + if badge.is_some() { 24.0 } else { 0.0 };
            let rr = Rect::from_min_size(Pos2::new(x, tabs_y), Vec2::new(w, 32.0));
            let current = i == tab_i;
            let resp = ui.interact(rr, ui.scope_id().with(("tab", i)), egui::Sense::click());
            if current {
                fx::glow(&painter, rr, 16.0, ACCENT.gamma_multiply(0.35), 12.0);
                fx::gradient(&painter, rr, 16.0, false, &[(0.0, ROW_SEL), (1.0, theme::mix(ROW_SEL, theme::VIOLET, 0.35))]);
                fx::gradient(&painter, rr, 16.0, true, &[(0.0, Color32::from_white_alpha(30)), (0.6, Color32::TRANSPARENT)]);
            } else if resp.hovered() {
                painter.rect_filled(rr, 16.0, ROW);
            }
            let col = if current { TEXT } else { DIM };
            tab_icon(page).paint(&painter, Pos2::new(rr.left() + 20.0, rr.center().y), 14.0, col);
            painter.text(Pos2::new(rr.left() + 34.0, rr.center().y), Align2::LEFT_CENTER, name, theme::strong(14.0), col);
            if let Some((b, c)) = badge {
                let bc = Pos2::new(rr.right() - 18.0, rr.center().y);
                painter.circle_filled(bc, 9.0, c);
                painter.text(bc, Align2::CENTER_CENTER, b, theme::strong(11.0), Color32::BLACK);
            }
            if resp.clicked() {
                self.open(page.clone(), false);
            }
            x += w + 6.0;
        }
        icons::glyph(&painter, Pos2::new(x + 13.0, tabs_y + 16.0), Glyph::Rb, style);
        // A sub-page says where it is.
        let mut top = tabs_y + 46.0;
        let crumb = match &self.page {
            Page::Profile(_) => Some(t!("overlay.crumb_player")),
            Page::Details => Some(t!("overlay.details_title")),
            Page::Hotkeys => Some(t!("settings.open_with")),
            _ => None,
        };
        if let Some(c) = crumb {
            Icon::Back.paint(&painter, Pos2::new(inner.left() + 8.0, top + 9.0), 12.0, DIM);
            painter.text(Pos2::new(inner.left() + 22.0, top + 9.0), Align2::LEFT_CENTER, c, theme::strong(13.0), TEXT2);
            top += 24.0;
        }

        // Rows.
        let list = Rect::from_min_max(Pos2::new(inner.left(), top), Pos2::new(inner.right(), inner.bottom() - 36.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(list));
        let scroll_to = std::mem::take(&mut self.scroll_to_sel);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut child, |ui| {
            for (i, row) in rows.iter().enumerate() {
                if row.header {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), egui::Sense::hover());
                    ui.painter().text(Pos2::new(rect.left() + 4.0, rect.bottom() - 8.0), Align2::LEFT_BOTTOM, row.title.to_uppercase(), theme::strong(11.5), theme::TEXT3);
                    continue;
                }
                // The detail wraps (up to three lines) in the room left of the buttons or
                // value on the right; a longer one makes the row taller.
                let width = ui.available_width();
                let text_x = if row.icon.is_some() { 57.0 } else { 16.0 };
                let right_w = if !row.value.is_empty() { 150.0 } else if row.selectable { 220.0 } else { 0.0 };
                let tw = (width - text_x - right_w - 8.0).max(80.0);
                let detail = (!row.detail.is_empty()).then(|| {
                    let mut job = egui::text::LayoutJob::simple(row.detail.clone(), theme::font(12.5), DIM, tw);
                    job.wrap.max_rows = 3;
                    ui.painter().layout_job(job)
                });
                let line = 17.0;
                let h = match &detail {
                    None => 44.0,
                    Some(d) => 58.0 + (d.size().y - line).max(0.0),
                };
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), if row.selectable { egui::Sense::click() } else { egui::Sense::hover() });
                let selected = row.selectable && i == self.sel;
                if row.selectable && resp.hovered() && resp.ctx.input(|inp| inp.pointer.delta() != Vec2::ZERO) {
                    self.sel = i;
                }
                let p = ui.painter();
                let body = rect.shrink2(Vec2::new(0.0, 2.5));
                let tone = match row.tone {
                    Tone::Warn => theme::WARN,
                    Tone::Good => GOOD,
                    Tone::Plain => ACCENT_HI,
                };
                if selected {
                    fx::glow(p, body, 11.0, ACCENT.gamma_multiply(0.30), 12.0);
                    fx::gradient(p, body, 11.0, false, &[(0.0, ROW_SEL), (1.0, theme::mix(ROW, ROW_SEL, 0.45))]);
                    fx::gradient(p, body, 11.0, true, &[(0.0, Color32::from_white_alpha(22)), (0.5, Color32::TRANSPARENT)]);
                    let bar = Rect::from_min_size(Pos2::new(body.left() + 1.0, body.center().y - 12.0), Vec2::new(3.0, 24.0));
                    fx::glow(p, bar, 1.5, ACCENT_HI.gamma_multiply(0.8), 8.0);
                    p.rect_filled(bar, 1.5, Color32::WHITE);
                } else if row.selectable {
                    p.rect_filled(body, 11.0, ROW);
                } else if row.tone == Tone::Warn {
                    p.rect_filled(body, 11.0, theme::WARN.gamma_multiply(0.07));
                }
                if row.tone == Tone::Warn {
                    p.rect_stroke(body, 11.0, Stroke::new(1.0, theme::WARN.gamma_multiply(0.45)), egui::StrokeKind::Inside);
                }
                let mut tx = rect.left() + 16.0;
                if let Some(icon) = row.icon {
                    let tile = Rect::from_center_size(Pos2::new(rect.left() + 30.0, rect.center().y), Vec2::splat(30.0));
                    let c = if row.tone == Tone::Plain && !selected { DIM } else { tone };
                    p.rect_filled(tile, 9.0, c.gamma_multiply(if selected { 0.32 } else { 0.16 }));
                    icon.paint(p, tile.center(), 15.0, theme::mix(c, Color32::WHITE, 0.3));
                    tx = tile.right() + 12.0;
                }
                // The text stops short of the buttons or value on the right.
                let title_y = if row.detail.is_empty() { rect.center().y } else { rect.top() + 21.0 };
                let tg = p.layout_no_wrap(row.title.clone(), theme::strong(15.5), if row.selectable || row.tone != Tone::Plain { TEXT } else { TEXT2 });
                p.with_clip_rect(Rect::from_min_size(Pos2::new(tx, rect.top()), Vec2::new(tw, rect.height()))).galley(Pos2::new(tx, title_y - tg.size().y / 2.0), tg, TEXT);
                if let Some(dg) = detail {
                    p.galley(Pos2::new(tx, rect.top() + 31.0), dg, DIM);
                }
                let mut rx = rect.right() - 14.0;
                if !row.value.is_empty() {
                    if row.change.is_some() {
                        // Room for the right arrow.
                        rx -= 10.0;
                    }
                    if row.is_on() || row.value == t!("common.off") {
                        // A switch, drawn as one.
                        let on = row.is_on();
                        let sw = Rect::from_min_size(Pos2::new(rx - 40.0, rect.center().y - 11.0), Vec2::new(40.0, 22.0));
                        if on {
                            fx::glow(p, sw, 11.0, ACCENT.gamma_multiply(0.35), 8.0);
                            fx::gradient(p, sw, 11.0, false, &[(0.0, ACCENT), (1.0, theme::mix(ACCENT, theme::VIOLET, 0.5))]);
                        } else {
                            p.rect_filled(sw, 11.0, Color32::from_rgb(14, 15, 20));
                            p.rect_stroke(sw, 11.0, Stroke::new(1.0, theme::line_strong()), egui::StrokeKind::Inside);
                        }
                        let k = Pos2::new(if on { sw.right() - 11.0 } else { sw.left() + 11.0 }, sw.center().y);
                        p.circle_filled(k, 8.0, if on { Color32::WHITE } else { Color32::from_gray(190) });
                    } else {
                        let g = p.text(Pos2::new(rx, rect.center().y), Align2::RIGHT_CENTER, &row.value, theme::strong(14.5), TEXT);
                        if selected && row.change.is_some() {
                            arrow(p, Pos2::new(g.left() - 10.0, rect.center().y), -1.0);
                            arrow(p, Pos2::new(g.right() + 10.0, rect.center().y), 1.0);
                        }
                    }
                } else if row.selectable {
                    // Buttons on the right: clickable, with their controller glyph.
                    for (action, label, g) in [(row.second, row.second_label, Glyph::West), (row.main, row.main_label, Glyph::South)] {
                        if action == Action::None || label.is_empty() {
                            continue;
                        }
                        let w = 42.0 + ui.painter().layout_no_wrap(label.to_string(), theme::strong(13.0), TEXT).size().x;
                        let br = Rect::from_min_size(Pos2::new(rx - w, rect.center().y - 15.0), Vec2::new(w, 30.0));
                        let bresp = ui.interact(br, ui.scope_id().with(("btn", i, label)), egui::Sense::click());
                        let p = ui.painter();
                        let fill = if bresp.hovered() { Color32::from_rgb(76, 116, 200) } else if selected { Color32::from_white_alpha(30) } else { Color32::from_white_alpha(14) };
                        p.rect_filled(br, 15.0, fill);
                        if selected {
                            icons::glyph(p, Pos2::new(br.left() + 16.0, br.center().y), g, style);
                        } else {
                            p.circle_stroke(Pos2::new(br.left() + 16.0, br.center().y), 6.0, Stroke::new(1.2, Color32::from_white_alpha(50)));
                        }
                        p.text(Pos2::new(br.left() + 30.0, br.center().y), Align2::LEFT_CENTER, label, theme::strong(13.0), TEXT);
                        if bresp.clicked() {
                            pending_action = action;
                        }
                        rx = br.left() - 8.0;
                    }
                }
                if resp.clicked() && row.selectable && pending_action == Action::None {
                    self.sel = i;
                    pending_action = row.main;
                }
                if selected && scroll_to {
                    ui.scroll_to_rect(rect, None);
                }
            }
        });

        // Footer: what the buttons do on the selected row (only the ones that do
        // something there), and Back.
        let fy = inner.bottom() - 10.0;
        painter.rect_filled(Rect::from_min_size(Pos2::new(inner.left(), fy - 22.0), Vec2::new(inner.width(), 1.0)), 0.0, theme::line());
        let cur = rows.get(self.sel).filter(|r| r.selectable);
        let mut hints: Vec<(Glyph, &str)> = Vec::new();
        if let Some(r) = cur {
            if r.main != Action::None {
                hints.push((Glyph::South, if r.main_label.is_empty() { t!("hint.select") } else { r.main_label }));
            }
            if r.second != Action::None && !r.second_label.is_empty() {
                hints.push((Glyph::West, r.second_label));
            }
        }
        hints.push((Glyph::East, if self.parent().is_some() { t!("hint.back") } else { t!("overlay.close") }));
        icons::hints(&painter, Pos2::new(inner.left(), fy), &hints, style, DIM);
        painter.text(Pos2::new(inner.right(), fy), Align2::RIGHT_CENTER, t!("overlay.to_close", keys = self.hotkey_hint), theme::font(12.0), DIM);
        // Messages under the panel, each on a dark plate of its own so it reads over
        // any game.
        let mut below = panel.bottom() + 14.0;
        let plate = |text: &str, size: f32, color: Color32, y: f32| -> f32 {
            let galley = painter.layout(text.to_string(), theme::font(size), color, panel.width());
            let pad = Vec2::new(16.0, 9.0);
            let r = Rect::from_min_size(Pos2::new(panel.center().x - galley.size().x / 2.0 - pad.x, y), galley.size() + pad * 2.0);
            fx::glow(&painter, r, 10.0, ACCENT.gamma_multiply(0.15), 12.0);
            painter.rect_filled(r, CornerRadius::same(10), Color32::from_rgba_unmultiplied(24, 27, 34, 240));
            painter.rect_stroke(r, CornerRadius::same(10), Stroke::new(1.0, theme::line_strong()), egui::StrokeKind::Inside);
            painter.galley(r.min + pad, galley, color);
            r.bottom() + 8.0
        };
        if let Some((text, until)) = &self.notice {
            if *until > Instant::now() {
                below = plate(text, 15.0, TEXT, below);
            }
        }
        if let Some(tip) = &self.tip {
            plate(tip, 13.0, TEXT, below);
        }
        self.sel_key = rows.get(self.sel).filter(|r| r.selectable).map(Row::key);
        if pending_action != Action::None {
            self.act(pending_action, prefs, out);
        }
        // A click or the pointer changed something: egui runs the frame again at once so
        // it shows (the next input may be a while away).
        if pending_action != Action::None || self.sel != sel_before {
            ui.ctx().request_discard("the overlay changed");
        }
    }

    /// The panel's last rectangle in pixels (x, y, w, h).
    pub fn panel_px(&self, ppp: f32) -> Option<[u32; 4]> {
        let r = self.panel?.expand(30.0);
        Some([(r.min.x * ppp).max(0.0) as u32, (r.min.y * ppp).max(0.0) as u32, (r.width() * ppp) as u32, (r.height() * ppp) as u32])
    }

    /// The corner notifications, drawn alone while the overlay is closed. Returns their
    /// area in points (what the host needs to show).
    pub fn toasts_frame(&mut self, ui: &mut egui::Ui) -> Option<Rect> {
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        if self.toasts.is_empty() {
            return None;
        }
        let screen = ui.max_rect();
        let w = 360.0;
        let painter = ui.painter();
        let mut y = screen.top() + 28.0;
        let mut area = Rect::NOTHING;
        for t in &self.toasts {
            let dg = painter.layout(t.detail.clone(), theme::font(12.5), DIM, w - 64.0);
            let h = 44.0 + dg.size().y;
            let r = Rect::from_min_size(Pos2::new(screen.right() - w - 28.0, y), Vec2::new(w, h));
            fx::glow(painter, r, 12.0, ACCENT.gamma_multiply(0.22), 16.0);
            fx::gradient(painter, r, 12.0, true, &[(0.0, Color32::from_rgba_unmultiplied(32, 36, 47, 246)), (1.0, Color32::from_rgba_unmultiplied(22, 24, 31, 246))]);
            painter.rect_stroke(r, 12.0, Stroke::new(1.0, theme::line_strong()), egui::StrokeKind::Inside);
            let tile = Rect::from_min_size(r.left_top() + Vec2::new(14.0, 14.0), Vec2::splat(28.0));
            fx::gradient(painter, tile, 8.0, true, &[(0.0, ACCENT), (1.0, theme::VIOLET)]);
            brand_lines(painter, tile);
            painter.text(Pos2::new(tile.right() + 12.0, r.top() + 15.0), Align2::LEFT_TOP, &t.title, theme::strong(15.0), TEXT);
            painter.galley(Pos2::new(tile.right() + 12.0, r.top() + 37.0), dg, DIM);
            area = area.union(r.expand(18.0));
            y += h + 10.0;
        }
        Some(area)
    }
}

/// Where the panel goes on a screen (both in points).
pub fn panel_rect(screen: Rect) -> Rect {
    let size = Vec2::new((screen.width() * 0.62).clamp(540.0, 920.0), (screen.height() * 0.8).clamp(400.0, 700.0));
    Rect::from_center_size(screen.center(), size)
}

/// The panel's background: its light, gradient and edge. The same every frame, so the
/// host draws it once and keeps it (its lights alone would cost most of a frame).
pub fn background(panel: Rect) -> Vec<egui::Shape> {
    let r = 16.0;
    vec![
        fx::glow_shape(panel, r, ACCENT.gamma_multiply(0.14), 28.0),
        fx::gradient_shape(panel, r, true, &[(0.0, PANEL_TOP), (1.0, PANEL)]),
        fx::spotlight_shape(panel, r, Pos2::new(panel.left() + panel.width() * 0.3, panel.top() + 2.0), panel.width() * 0.7, ACCENT.gamma_multiply(0.10)),
        egui::Shape::rect_stroke(panel, r, Stroke::new(1.0, theme::line_strong()), egui::StrokeKind::Inside),
    ]
}

/// PeerParty's mark: two nodes and their link on a lit tile.
fn brand(p: &egui::Painter, r: Rect) {
    fx::glow(p, r, 10.0, ACCENT.gamma_multiply(0.4), 12.0);
    fx::gradient(p, r, 10.0, false, &[(0.0, ACCENT), (1.0, theme::VIOLET)]);
    fx::gradient(p, r, 10.0, true, &[(0.0, Color32::from_white_alpha(46)), (0.55, Color32::TRANSPARENT)]);
    brand_lines(p, r);
}

fn brand_lines(p: &egui::Painter, r: Rect) {
    let k = r.width() / 38.0;
    let (a, b) = (r.left_center() + Vec2::new(10.5 * k, 0.0), r.right_center() - Vec2::new(10.5 * k, 0.0));
    p.line_segment([a, b], Stroke::new(2.6 * k, Color32::WHITE));
    p.circle_filled(a, 4.6 * k, Color32::WHITE);
    p.circle_filled(b, 4.6 * k, Color32::WHITE);
}

fn sdk_label(s: &str) -> &str {
    match s {
        "Eos" => "Epic Online Services",
        "Steam" => "Steam",
        "Party" => "PlayFab Party",
        "PlayFab" => "PlayFab",
        "PfLobby" => "PlayFab Lobby",
        "Galaxy" => "GOG Galaxy",
        "Ugs" => "Unity Gaming Services",
        other => other,
    }
}

/// The service a lobby belongs to, from its namespace ("steam.lobby").
fn lobby_kind(ns: &str) -> Option<&'static str> {
    Some(match ns.split('.').next().unwrap_or_default() {
        "steam" => "Steam",
        "eos" => "Epic Online Services",
        "playfab" | "party" => "PlayFab",
        "galaxy" => "GOG Galaxy",
        "ugs" => "Unity",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::raster::{Canvas, Textures};

    fn sample_state() -> GameState {
        GameState::from_json(&serde_json::json!({
            "t": "state",
            "me": {"id": "aa", "name": "Kim"},
            "peers": [
                {"id": "bb", "name": "Sam", "status": "", "with_me": true, "ip": "192.168.1.20", "same_build": true},
                {"id": "cc", "name": "Alex", "status": "In the menus", "with_me": false, "ip": "192.168.1.31", "same_build": false}
            ],
            "lobby": {"id": "x", "kind": "eos.lobby", "max": 4, "members": [
                {"id": "aa", "name": "Kim", "owner": true}, {"id": "bb", "name": "Sam", "owner": false}
            ]},
            "pending": [{"id": 3, "from": "cc", "name": "Alex", "kind": "invited you"}],
            "can_invite": true,
            "sdks": ["Eos", "Steam"],
            "internet": [{"mode": "room", "online": true, "nat": "hole-punch friendly",
                "port_map": "UPnP: external port 49294", "dht_nodes": 38, "found": 2, "blocked": 0,
                "peers": [{"name": "Sam", "rtt_ms": 23, "bridged": true, "resent": 0, "ipv6": true}]}]
        }))
    }

    /// No internet address reaches the overlay's text, whatever the hook sends: public
    /// and room peers show how they connect, a LAN peer only a private address.
    #[test]
    fn no_internet_address_on_any_page() {
        let mut o = Overlay::new("Game".into());
        o.state = GameState::from_json(&serde_json::json!({
            "t": "state",
            "me": {"id": "aa", "name": "Kim"},
            "peers": [
                {"id": "bb", "name": "Sam", "with_me": false, "ip": "192.168.1.20", "via": "lan"},
                {"id": "cc", "name": "Pat", "with_me": false, "ip": "203.0.113.7", "via": "public", "fp": "abcd-ef01-2345-6789"},
                {"id": "dd", "name": "Kim", "with_me": true, "ip": "198.51.100.4", "via": "room"},
                {"id": "ee", "name": "Old", "with_me": false, "ip": "8.8.8.8"}
            ],
            "can_invite": true,
            "internet": [{"mode": "public", "online": true, "nat": "symmetric 192.0.2.9",
                "port_map": "UPnP: external port 5000 at 203.0.113.7:5000", "peers": [{"name": "1.1.1.1", "rtt_ms": 5, "address": "198.18.7.9"}, {"name": "Zed", "rtt_ms": 6, "address": "203.0.113.8"}]}]
        }));
        let prefs = Prefs::default();
        let mut text = String::new();
        let pages = [Page::Network, Page::Details, Page::Players, Page::Profile("bb".into()), Page::Profile("cc".into()), Page::Profile("dd".into()), Page::Profile("ee".into())];
        for page in pages {
            o.open(page, false);
            for r in o.rows(&prefs) {
                text.push_str(&format!("{} | {} | {}\n", r.title, r.detail, r.value));
            }
        }
        assert!(text.contains("external port 5000"), "{text}");
        assert!(text.contains("IP to type: 198.18.7.9"), "{text}");
        for bad in ["203.0.113.8", "203.0.113.7", "198.51.100.4", "8.8.8.8", "192.0.2.9", "1.1.1.1"] {
            assert!(!text.contains(bad), "{bad} shown:\n{text}");
        }
        assert!(text.contains("192.168.1.20"), "{text}");
        assert!(text.contains("abcd-ef01-2345-6789"), "{text}");
        assert!(text.contains("Block this player"), "{text}");
        // Block is only offered (and only sent) for a public player.
        let mut out = Vec::new();
        let mut prefs = Prefs::default();
        o.act(Action::Block(0), &mut prefs, &mut out);
        assert!(out.is_empty());
        o.act(Action::Block(1), &mut prefs, &mut out);
        assert!(matches!(&out[..], [Cmd::Hook(v)] if v["t"] == "block" && v["peer"] == "cc"));
    }

    /// A dedicated server is listed by its own name as a server: no version warning
    /// (its program is never the client's), Join only, and nothing to invite it to.
    #[test]
    fn a_dedicated_server_is_listed_as_a_server() {
        let mut o = Overlay::new("Game".into());
        o.state = GameState::from_json(&serde_json::json!({
            "t": "state",
            "me": {"id": "aa", "name": "Kim"},
            "peers": [
                {"id": "bb", "name": "My World", "server": true, "ip": "192.168.1.20", "via": "lan", "same_build": false},
                {"id": "cc", "name": "Sam", "ip": "192.168.1.31", "via": "lan", "same_build": false}
            ]
        }));
        o.open(Page::Players, false);
        let rows = o.rows(&Prefs::default());
        let server = rows.iter().find(|r| r.title == "My World").expect("the server is listed");
        assert_eq!(server.detail, "Dedicated server  \u{b7}  192.168.1.20");
        assert!(server.main == Action::Join(0) && server.second_label.is_empty());
        let sam = rows.iter().find(|r| r.title == "Sam").unwrap();
        assert!(sam.detail.contains("a different version of the game"), "a player still gets the warning");
        assert!(!sam.second_label.is_empty(), "a player can still be invited");
        o.open(Page::Players, true);
        let rows = o.rows(&Prefs::default());
        assert!(!rows.iter().any(|r| r.title == "My World"), "nothing to invite a server to");
        // Home counts the player and the server apart.
        o.open(Page::Home, false);
        let rows = o.rows(&Prefs::default());
        assert!(rows.iter().any(|r| r.title == "1 other player" && r.detail == "Sam"));
        assert!(rows.iter().any(|r| r.title == "1 dedicated server" && r.detail == "My World"));
    }

    /// A new player is "playing too", a new server "is running", and the player's
    /// own other copy on this PC gets no notice.
    #[test]
    fn arrival_notices_tell_players_servers_and_your_own_copies_apart() {
        let st = GameState::from_json(&serde_json::json!({
            "t": "state",
            "peers": [
                {"id": "bb", "name": "Sam"},
                {"id": "cc", "name": "My World", "server": true, "this_pc": true},
                {"id": "dd", "name": "Kim (2)", "this_pc": true}
            ]
        }));
        assert_eq!(arrival_text(&st.peers[0]).as_deref(), Some("Sam is playing too"));
        assert_eq!(arrival_text(&st.peers[1]).as_deref(), Some("Dedicated server My World is running"));
        assert_eq!(arrival_text(&st.peers[2]), None);
    }

    /// The Network page's internet switch: it shows the game's state,
    /// Select flips it (saved, and sent to the game), left / right still switch pages,
    /// and off hides the room and public status. Not set up: no switch, just how to.
    #[test]
    fn internet_page_switches_internet_play() {
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        assert!(o.state.internet_on && o.state.internet_configured, "an older hook's state reads as on and set up");
        o.open(Page::Network, false);
        let rows = o.rows(&Prefs::default());
        let top = rows.iter().find(|r| r.selectable).unwrap();
        assert_eq!((top.title.as_str(), top.value.as_str()), ("Internet play", "On"));
        assert!(top.change.is_none(), "left / right stay free for switching pages");
        assert!(top.main == Action::ToggleInternet);
        assert!(rows.iter().any(|r| r.title.starts_with("Your room")));

        let mut prefs = Prefs::default();
        let mut out = Vec::new();
        o.act(Action::ToggleInternet, &mut prefs, &mut out);
        assert!(!prefs.internet);
        assert!(matches!(&out[..], [Cmd::Prefs, Cmd::Hook(v)] if v["t"] == "set" && v["key"] == "internet" && v["value"] == "off"));
        let rows = o.rows(&prefs);
        assert_eq!(rows.iter().find(|r| r.selectable).unwrap().value, "Off");
        assert!(!rows.iter().any(|r| r.title.starts_with("Your room")), "no status while off");

        // The game's own report wins over the saved setting (say an environment value
        // started it off): Select turns on what the game shows as off.
        let mut s = sample_state();
        s.internet_on = false;
        o.state = s;
        let mut prefs = Prefs::default();
        let mut out = Vec::new();
        o.act(Action::ToggleInternet, &mut prefs, &mut out);
        assert!(prefs.internet && o.state.internet_on);
        assert!(matches!(&out[..], [Cmd::Prefs, Cmd::Hook(v)] if v["value"] == "on"));

        o.state = GameState::from_json(&serde_json::json!({"t": "state", "internet_on": true, "internet_configured": false, "internet": []}));
        let rows = o.rows(&Prefs::default());
        assert!(rows.iter().all(|r| r.main != Action::ToggleInternet));
        assert!(rows.iter().any(|r| r.title == "Internet play isn't set up"));
    }

    /// A firewall problem is the Network page's first row (and Home's): Fix asks the host to fix it
    /// (public networks too from the second button), and players we can't reach get a
    /// row saying where to fix it. Without either (an older hook), nothing extra shows.
    #[test]
    fn internet_page_warns_about_the_firewall_and_fixes_it() {
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        assert_eq!(o.state.lan_unreachable, None, "an older hook doesn't say");
        o.open(Page::Network, false);
        let rows = o.rows(&Prefs::default());
        assert!(!rows.iter().any(|r| matches!(r.main, Action::FixFirewall(_)) || r.title.contains("can't be reached")));

        let mut v = serde_json::json!({"t": "state", "internet": [], "lan_unreachable": 2});
        o.state = GameState::from_json(&v);
        assert_eq!(o.state.lan_unreachable, Some(2));
        o.firewall = Some(Firewall { title: "Windows Firewall blocks game.exe".into(), detail: "Other players can't connect to you".into() });
        let rows = o.rows(&Prefs::default());
        let top = rows.iter().find(|r| r.selectable).unwrap();
        assert_eq!(top.title, "Windows Firewall blocks game.exe");
        assert!(top.main == Action::FixFirewall(false) && top.second == Action::FixFirewall(true));
        assert!(top.change.is_none(), "left / right stay free for switching pages");
        assert!(rows.iter().any(|r| r.title == "2 players on your network can't be reached"), "{:?}", rows.iter().map(|r| &r.title).collect::<Vec<_>>());

        let mut prefs = Prefs::default();
        let mut out = Vec::new();
        o.act(Action::FixFirewall(false), &mut prefs, &mut out);
        o.act(Action::FixFirewall(true), &mut prefs, &mut out);
        assert!(matches!(&out[..], [Cmd::FixFirewall(false), Cmd::FixFirewall(true)]));

        o.open(Page::Home, false);
        let home = o.rows(&Prefs::default());
        assert_eq!(home.iter().find(|r| r.selectable).unwrap().title, "Windows Firewall blocks game.exe", "Home leads with it too");
        o.open(Page::Network, false);
        v["lan_unreachable"] = 0.into();
        o.state = GameState::from_json(&v);
        o.firewall = None;
        let rows = o.rows(&Prefs::default());
        assert!(!rows.iter().any(|r| matches!(r.main, Action::FixFirewall(_)) || r.title.contains("can't be reached")));
    }

    /// Left on the Network page's switch row goes to Players, right to Settings.
    #[test]
    fn internet_switch_leaves_left_right_to_pages() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let mut tex = Textures::default();
        let mut prefs = Prefs::default();
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        o.open(Page::Network, false);
        draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        draw(&ctx, &mut tex, &mut o, &mut prefs, Nav { right: true, ..Nav::default() });
        assert_eq!(o.page, Page::Settings);
        assert!(prefs.internet, "switching pages doesn't flip it");
        o.open(Page::Network, false);
        draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        draw(&ctx, &mut tex, &mut o, &mut prefs, Nav { select: true, ..Nav::default() });
        assert_eq!(o.page, Page::Network);
        assert!(!prefs.internet && !o.state.internet_on, "Select flips it");
    }

    /// Renders each page to a PNG (in PEERPARTY_PREVIEW_DIR, when set) and checks the
    /// pages draw something.
    #[test]
    fn pages_render() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let mut tex = Textures::default();
        let mut prefs = Prefs::default();
        let mut o = Overlay::new("Sample Game".into());
        o.state = sample_state();
        let (w, h) = (1600usize, 900usize);
        let dir = std::env::var_os("PEERPARTY_PREVIEW_DIR");
        for (i, page) in [Page::Home, Page::Players, Page::Network, Page::Settings, Page::Details].into_iter().enumerate() {
            o.open(page, false);
            o.style = [Style::Xbox, Style::PlayStation, Style::Nintendo, Style::Xbox, Style::PlayStation][i];
            // The settings page with a message and a tip under the panel.
            if i == 3 {
                o.notice = Some(("Hold two or more controller buttons together for a second".into(), std::time::Instant::now() + std::time::Duration::from_secs(5)));
                o.tip = Some("Tip: if Game Bar opens too, turn off Settings > Gaming > Game Bar".into());
            }
            let mut canvas = Canvas::new(w, h);
            // Two passes: the first lays out, the second draws with fonts ready.
            for _ in 0..2 {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w as f32, h as f32))),
                    ..Default::default()
                };
                let mut out_cmds = Vec::new();
                let out = ctx.run_ui(input, |ui| o.frame(ui, Nav::default(), &mut prefs, &mut out_cmds));
                tex.apply(out.textures_delta);
                let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
                canvas.clear(Color32::from_black_alpha(150));
                tex.draw(&mut canvas, &prims, out.pixels_per_point, (0.0, 0.0));
            }
            let opaque = canvas.px.iter().filter(|p| *p >> 24 == 255).count();
            assert!(opaque > 10_000, "page {i} drew {opaque} opaque pixels");
            if let Some(d) = &dir {
                let rgba: Vec<u8> = canvas
                    .px
                    .iter()
                    .flat_map(|p| {
                        // Over a mid-grey "game" so the dim shows.
                        let a = (p >> 24) as f32 / 255.0;
                        let bg = 90.0 * (1.0 - a);
                        [((p >> 16) & 0xff) as f32 + bg, ((p >> 8) & 0xff) as f32 + bg, (p & 0xff) as f32 + bg, 255.0]
                    })
                    .map(|v| v.min(255.0) as u8)
                    .collect();
                let img = image::RgbaImage::from_raw(w as u32, h as u32, rgba).unwrap();
                img.save(std::path::Path::new(d).join(format!("overlay-{i}.png"))).unwrap();
            }
        }
    }

    /// A game hosting a session that takes no invites (a Photon room): Home says how the
    /// others join it, in the overlay's language, and offers no Invite button.
    #[test]
    fn a_game_without_invites_says_how_to_join() {
        crate::i18n::set("en");
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        o.state.can_invite = false;
        o.state.invite_note = "This game has no invites: players on your network join your session from the game's own menu (room EUCSH)".into();
        o.open(Page::Home, false);
        let rows = o.rows(&Prefs::default());
        let row = rows.iter().find(|r| r.title == t!("overlay.invite_all")).unwrap();
        assert!(!row.selectable, "nothing to press");
        assert!(row.detail.contains("game's own menu") && row.detail.ends_with("(room EUCSH)"), "{}", row.detail);
        assert_eq!(hook_text(&o.state.invite_note), t!("hook.photon_room", room = "EUCSH"));
        if let Some(d) = std::env::var_os("PEERPARTY_PREVIEW_DIR") {
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            let mut tex = Textures::default();
            let mut prefs = Prefs::default();
            draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
            let c = draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
            let rgba: Vec<u8> = c.px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255]).collect();
            image::RgbaImage::from_raw(c.w as u32, c.h as u32, rgba).unwrap().save(std::path::Path::new(&d).join("overlay-no-invites.png")).unwrap();
        }
    }

    /// A Photon room: Home says which room we're in and who is there (not "no lobby"),
    /// Invite sends the code, a received code is typed in from a row, and pasting closes
    /// the overlay.
    #[test]
    fn a_photon_room_shows_its_code_and_types_received_ones() {
        crate::i18n::set("en");
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        o.state.codes = true;
        o.state.can_invite = true;
        o.state.lobby = Some(LobbyInfo { id: "EOXBY".into(), kind: "photon.room".into(), max: 0, members: vec![("a".into(), "Timber".into(), true), ("b".into(), "Lola".into(), false)] });
        o.state.pending = vec![PendingInvite { id: 7, name: "Lola".into(), kind: "sent you a room code".into(), code: "QWERT".into() }];
        o.open(Page::Home, false);
        let rows = o.rows(&Prefs::default());
        assert!(rows.iter().all(|r| r.title != t!("overlay.no_lobby")));
        let room = rows.iter().find(|r| r.title.contains("EOXBY")).expect("room row");
        assert!(room.title.contains('2') && room.detail.contains("Lola"), "{} / {}", room.title, room.detail);
        assert_eq!(room.main, Action::CopyCode);
        let code = rows.iter().find(|r| r.title.contains("QWERT")).expect("received code row");
        assert_eq!(code.main, Action::Accept(0));
        assert_eq!(code.second, Action::Decline(0));
        let invite = rows.iter().find(|r| r.title == t!("overlay.invite_all")).unwrap();
        assert_eq!(invite.detail, t!("overlay.invite_code_detail"));
        let paste = rows.iter().find(|r| r.main == Action::PasteCode).expect("paste row");
        let mut out = Vec::new();
        let mut prefs = Prefs::default();
        o.act(paste.main, &mut prefs, &mut out);
        assert!(out.iter().any(|c| matches!(c, Cmd::Hook(v) if v["t"] == "paste")) && out.iter().any(|c| matches!(c, Cmd::Close)));
        out.clear();
        o.act(Action::Accept(0), &mut prefs, &mut out);
        assert!(out.iter().any(|c| matches!(c, Cmd::Hook(v) if v["t"] == "accept" && v["id"] == 7)) && out.iter().any(|c| matches!(c, Cmd::Close)));
    }

    /// Draws one frame and returns what it asked for.
    fn press(ctx: &egui::Context, tex: &mut Textures, o: &mut Overlay, prefs: &mut Prefs, nav: Nav) -> Vec<Cmd> {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 900.0))), ..Default::default() };
        let mut cmds = Vec::new();
        let out = ctx.run_ui(input, |ui| o.frame(ui, nav, prefs, &mut cmds));
        tex.apply(out.textures_delta);
        cmds
    }

    fn invites_state(pending: &[(u64, &str)], unreachable: u32) -> GameState {
        let pending: Vec<serde_json::Value> = pending.iter().map(|(id, name)| serde_json::json!({"id": id, "from": name, "name": name, "kind": "invited you"})).collect();
        GameState::from_json(&serde_json::json!({"t": "state", "can_invite": true, "lan_unreachable": unreachable, "pending": pending}))
    }

    fn accepted(cmds: &[Cmd]) -> Option<u64> {
        cmds.iter().find_map(|c| match c {
            Cmd::Hook(v) if v["t"] == "accept" => v["id"].as_u64(),
            _ => None,
        })
    }

    /// The selection stays on its row while the hook's state adds a row above it or
    /// takes one away: Select accepts the invite that was selected, not whatever row now
    /// has its place.
    #[test]
    fn the_selection_follows_its_row_through_state_updates() {
        crate::i18n::set("en");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let (mut tex, mut prefs) = (Textures::default(), Prefs::default());
        let mut o = Overlay::new("Game".into());
        o.state = invites_state(&[(3, "Alex"), (4, "Pat")], 0);
        o.open(Page::Home, false);
        press(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        let on_pat = |o: &Overlay| o.rows(&Prefs::default())[o.sel].title.contains("Pat");
        for _ in 0..6 {
            if on_pat(&o) {
                break;
            }
            press(&ctx, &mut tex, &mut o, &mut prefs, Nav { down: true, ..Nav::default() });
        }
        assert!(on_pat(&o), "Pat's invite is never selected");
        // A problem row appears on top.
        o.state = invites_state(&[(3, "Alex"), (4, "Pat")], 1);
        press(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        assert!(on_pat(&o), "a row above moved the selection to {:?}", o.rows(&prefs)[o.sel].title);
        // Alex's invite is answered elsewhere, just before A is pressed on Pat's.
        o.state = invites_state(&[(4, "Pat")], 1);
        let cmds = press(&ctx, &mut tex, &mut o, &mut prefs, Nav { select: true, ..Nav::default() });
        assert_eq!(accepted(&cmds), Some(4));
    }

    /// The selected row itself goes away: the selection moves to its neighbour, and a
    /// Select pressed right then (meant for the row that went) does nothing; a moment
    /// later it acts again.
    #[test]
    fn select_waits_a_moment_when_the_selected_row_goes() {
        crate::i18n::set("en");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let (mut tex, mut prefs) = (Textures::default(), Prefs::default());
        let mut o = Overlay::new("Game".into());
        o.state = invites_state(&[(3, "Alex"), (4, "Pat")], 0);
        o.open(Page::Home, false);
        press(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        while !o.rows(&prefs)[o.sel].title.contains("Pat") {
            press(&ctx, &mut tex, &mut o, &mut prefs, Nav { down: true, ..Nav::default() });
        }
        o.state = invites_state(&[(3, "Alex")], 0);
        let cmds = press(&ctx, &mut tex, &mut o, &mut prefs, Nav { select: true, ..Nav::default() });
        assert!(cmds.is_empty(), "Select acted at once on {:?}", o.rows(&prefs)[o.sel].title);
        std::thread::sleep(MOVED_HOLD + Duration::from_millis(50));
        while !o.rows(&prefs)[o.sel].title.contains("Alex") {
            press(&ctx, &mut tex, &mut o, &mut prefs, Nav { up: true, ..Nav::default() });
        }
        let cmds = press(&ctx, &mut tex, &mut o, &mut prefs, Nav { select: true, ..Nav::default() });
        assert_eq!(accepted(&cmds), Some(3));
    }

    /// Draws one frame and returns its pixels.
    fn draw(ctx: &egui::Context, tex: &mut Textures, o: &mut Overlay, prefs: &mut Prefs, nav: Nav) -> Canvas {
        let (w, h) = (1600usize, 900usize);
        let mut canvas = Canvas::new(w, h);
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w as f32, h as f32))), ..Default::default() };
        let mut cmds = Vec::new();
        let out = ctx.run_ui(input, |ui| o.frame(ui, nav, prefs, &mut cmds));
        tex.apply(out.textures_delta);
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        canvas.clear(Color32::from_black_alpha(150));
        tex.draw(&mut canvas, &prims, out.pixels_per_point, (0.0, 0.0));
        canvas
    }

    /// Left / right on Settings' top row switch pages, and the switched-to tab is the one
    /// highlighted in that same frame (the next frame may be a while away).
    #[test]
    fn settings_top_row_leaves_left_right_to_pages() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let mut tex = Textures::default();
        let mut prefs = Prefs::default();
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        o.open(Page::Settings, false);
        let rows = o.rows(&prefs);
        assert!(rows.iter().find(|r| r.selectable).is_some_and(|r| r.change.is_none()), "top row takes left / right");
        draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        let before = draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        let switched = draw(&ctx, &mut tex, &mut o, &mut prefs, Nav { left: true, ..Nav::default() });
        assert_eq!(o.page, Page::Network);
        let again = draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        assert_ne!(before.px, switched.px);
        assert_eq!(switched.px, again.px, "the switch frame already shows the new tab");
        // A multi-value row (the size, past a heading) still takes left / right.
        o.open(Page::Settings, false);
        draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        for _ in 0..3 {
            draw(&ctx, &mut tex, &mut o, &mut prefs, Nav { down: true, ..Nav::default() });
        }
        let scale = prefs.overlay_scale;
        let c = draw(&ctx, &mut tex, &mut o, &mut prefs, Nav { right: true, ..Nav::default() });
        assert_eq!(o.page, Page::Settings);
        assert!(prefs.overlay_scale > scale);
        if let Some(d) = std::env::var_os("PEERPARTY_PREVIEW_DIR") {
            let rgba: Vec<u8> = c.px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255]).collect();
            image::RgbaImage::from_raw(c.w as u32, c.h as u32, rgba).unwrap().save(std::path::Path::new(&d).join("overlay-settings.png")).unwrap();
        }
    }
}
