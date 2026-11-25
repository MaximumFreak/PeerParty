//! The overlay's pages, drawn with egui into a software canvas (raster.rs).
//!
//! Built for controllers first: every page is a list of rows, one selected at a time.
//! Up / down (d-pad, stick, arrow keys) moves, south (A / Cross, Enter, Space) does the
//! row's main action, west (X / Square, the Delete or X key) its second one, north (Y /
//! Triangle, the Y key) its details, left / right changes a setting, the shoulders (Q /
//! E, Page Up / Down) switch pages, east (B / Circle, Escape) goes back and closes. The
//! mouse works too: hovering selects a row, clicking acts.
//!
//! It's for what can be done in the game now; how it looks and opens is set in the
//! launcher window. Two pages: Players (what keeps them out, invites, everyone once, in
//! groups) and Network (this PC's firewall, the home network, the internet); a
//! split-screen session has a third, Split screen, first. A player's page and the
//! connection details open from them, and B goes back.

use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, Stroke, Vec2};
use peerparty_protocol::ipc::{HostMsg, InviteBlock, Notice, NoticeCode};
use serde_json::Value;

use super::pad::Style;
use crate::t;
use crate::ui::fx;
use crate::ui::icons::{self, Glyph, Icon};
use crate::ui::settings::{layer_name, layer_off_effect};
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
    /// Which of this PC's player slots this copy holds: 0 is the first player, 1 and up
    /// another copy of the game on this PC (another player, saves of its own).
    pub me_slot: u32,
    /// A voice chat is on in the game, and whether the player's microphone is.
    pub voice_in_use: bool,
    pub mic_on: bool,
    pub peers: Vec<Peer>,
    pub lobby: Option<LobbyInfo>,
    pub pending: Vec<PendingInvite>,
    pub can_invite: bool,
    /// The game joins rooms by code (Photon): copy and paste codes are offered.
    pub codes: bool,
    /// Why there's nothing to invite to (None when inviting works, or the hook didn't say
    /// in a form this build knows), and the hook's English for it.
    pub invite_block: Option<InviteBlock>,
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
    /// Copies of this game heard running another PeerParty, which can't connect to this
    /// one, and whether any of them is newer.
    pub other_version: u64,
    pub newer_version: bool,
    /// PeerParty layers turned off for this game that it needs (`PEERPARTY_SKIP` names):
    /// each gets a row saying what that costs.
    pub layers_off: Vec<String>,
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
    /// The player muted their voice here (the overlay's "Mute their voice").
    pub muted: bool,
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
    /// The sender's peer id: their row on Players is this invite's.
    pub from: String,
    pub name: String,
    pub kind: String,
    /// A room code to type into the game (an invite to a game that joins by code).
    pub code: String,
}

impl GameState {
    /// The invite rows' subtitle when there's nothing to invite to.
    pub fn why_no_invite(&self) -> String {
        match &self.invite_block {
            Some(b) => block_text(b),
            None if self.invite_note.is_empty() => t!("overlay.host_first").into(),
            None => self.invite_note.clone(),
        }
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
            other_version: v.get("lan_other_version").and_then(Value::as_f64).map_or(0, |n| n.max(0.0) as u64),
            newer_version: v.get("lan_newer_version").and_then(Value::as_bool).unwrap_or(false),
            me: v.get("me").map(|m| s(m, "name")).unwrap_or_default(),
            me_slot: v.get("me").and_then(|m| m.get("slot")).and_then(Value::as_u64).map_or(0, |n| n as u32),
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
                    muted: p.get("muted").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect(),
            voice_in_use: v.get("voice").and_then(|x| x.get("in_use")).and_then(Value::as_bool).unwrap_or(false),
            mic_on: v.get("voice").and_then(|x| x.get("mic")).and_then(Value::as_bool).unwrap_or(true),
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
                    from: s(p, "from"),
                    name: s(p, "name"),
                    kind: s(p, "kind"),
                    code: s(p, "code"),
                })
                .collect(),
            can_invite: v.get("can_invite").and_then(Value::as_bool).unwrap_or(false),
            codes: v.get("codes").and_then(Value::as_bool).unwrap_or(false),
            invite_block: v.get("invite_block").and_then(|b| peerparty_protocol::json::Json::parse(&b.to_string())).and_then(|b| InviteBlock::from_json(&b)),
            invite_note: s(v, "invite_note"),
            sdks: arr(v, "sdks").iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            foreign: arr(v, "foreign").iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            layers_off: arr(v, "layers_off").iter().filter_map(|x| x.as_str().map(String::from)).collect(),
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
    /// A split-screen session's players on this PC (only in a session of two or more).
    SplitScreen,
    Players,
    Network,
    Profile(String),
    /// Network's connection details: the router, port forwarding, finding players.
    Details,
}

impl Page {
    /// The tab a page sits under.
    fn tab(&self) -> Page {
        match self {
            Page::Profile(_) => Page::Players,
            Page::Details => Page::Network,
            p => p.clone(),
        }
    }
}

fn tab_name(p: &Page) -> &'static str {
    match p {
        Page::SplitScreen => t!("game.split_screen"),
        Page::Players => t!("overlay.tab_players"),
        _ => t!("overlay.tab_network"),
    }
}

fn tab_icon(p: &Page) -> Icon {
    match p {
        Page::SplitScreen => Icon::Gamepad,
        Page::Players => Icon::People,
        _ => Icon::Wifi,
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
    /// North (Y / Triangle): the row's details.
    pub third: bool,
    pub back: bool,
    pub prev_tab: bool,
    pub next_tab: bool,
    pub close: bool,
}

impl Nav {
    pub fn any(&self) -> bool {
        self.up || self.down || self.left || self.right || self.select || self.second || self.third || self.back || self.prev_tab || self.next_tab || self.close
    }
}

/// What the overlay asks of the host.
pub enum Cmd {
    /// A command for the hook (ipc.rs).
    Hook(HostMsg),
    Close,
    /// The global settings changed: save them and tell the hook what it needs.
    Prefs,
    /// A split-screen session: player `k`'s controller trades places with the next
    /// player's who plays with one.
    SwapSeat(usize),
    /// Player `k`'s controller is let go: the next controller used takes their seat.
    FreeSeat(usize),
    /// The keyboard and mouse go to the next (1) or previous (-1) player.
    KeyboardStep(i32),
    /// Open the live Layout Studio canvas over the split-screen panes.
    OpenLayoutStudio,
    /// Let the game through Windows Firewall (on public networks too, when true).
    FixFirewall(bool),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Action {
    InviteAll,
    /// A split-screen session's seats (`Cmd::SwapSeat` and the others).
    SwapSeat(usize),
    FreeSeat(usize),
    KeyboardStep(i32),
    ArrangePanes,
    /// Copy the room code we're in; type the copied one into the game.
    CopyCode,
    PasteCode,
    Invite(usize),
    Join(usize),
    Profile(usize),
    Block(usize),
    /// The player's microphone, in every voice chat of the game.
    ToggleMic,
    /// Mute or hear again this player's voice.
    ToggleMute(usize),
    /// Takes back the block just made (`Overlay::blocked`).
    Unblock,
    Accept(usize),
    Decline(usize),
    ToggleInternet,
    FixFirewall(bool),
    Open(PageRef),
    None,
}

/// A page a row opens (`Page` holds a String, so isn't Copy).
#[derive(Clone, Copy, PartialEq, Debug)]
enum PageRef {
    Network,
    Details,
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
    /// North's: the row's details.
    third: Action,
    /// left / right
    change: Option<(Action, Action)>,
    main_label: &'static str,
    second_label: &'static str,
    third_label: &'static str,
    selectable: bool,
    /// A small heading over the rows after it.
    header: bool,
    /// A quiet line of text, at the foot of a page.
    note: bool,
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
            third: Action::None,
            change: None,
            main_label: "",
            second_label: "",
            third_label: "",
            selectable: false,
            header: false,
            note: false,
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

    fn note(text: impl Into<String>) -> Row {
        Row { note: true, ..Row::info(text, "") }
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

    fn third(mut self, a: Action, label: &'static str) -> Row {
        self.third = a;
        self.third_label = label;
        self
    }

    fn is_on(&self) -> bool {
        self.value == t!("common.on")
    }
}

/// How long Select waits after the selected row went away (`Overlay::sel_moved`).
const MOVED_HOLD: Duration = Duration::from_millis(300);
/// Room each side of the rows, inside their scroll area, for the selected row's glow.
const LIST_ROOM: f32 = 12.0;

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
    /// Block pressed once for this player (by id): a second press within BLOCK_ARMED
    /// blocks them.
    block_armed: Option<(String, Instant)>,
    /// The player just blocked (id, name): the Players page offers Undo for BLOCK_UNDO.
    blocked: Option<(String, String, Instant)>,
    /// A split-screen session's players on this PC, for the Players page (the host
    /// fills it in while the overlay is open).
    pub seats: Option<Seats>,
}

/// The players of a split-screen session on this PC (`Overlay::seats`).
#[derive(Clone, Debug, PartialEq)]
pub struct Seats {
    pub players: Vec<PlayerSeat>,
    /// This overlay's player.
    pub me: usize,
    /// The player the keyboard and mouse go to (the pane with the focus), or None when
    /// keyboard and mouse are off for the games (controllers only).
    pub keyboard: Option<usize>,
}

/// One player of a split-screen session (`Seats`).
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerSeat {
    /// What they play with now, in words.
    pub detail: String,
    /// Their controller can trade places with another player's.
    pub can_swap: bool,
    /// They hold a controller that can be let go.
    pub can_free: bool,
}

/// How long Block waits for its second press, and how long Undo is offered after.
const BLOCK_ARMED: Duration = Duration::from_secs(5);
const BLOCK_UNDO: Duration = Duration::from_secs(30);

/// How long a toast stays on screen: enough to notice it in the corner of a game and read
/// longer notices without rushing (8–15 s by UTF-8 length, which tracks CJK reading time too).
fn toast_duration(title: &str, detail: &str) -> Duration {
    let bytes = title.len() + detail.len();
    Duration::from_secs((7 + bytes / 15).clamp(8, 15) as u64)
}

/// The notice for someone new in the game: a player "is playing too", a dedicated
/// server "is running", and nothing for another copy on this PC (the player's own).
pub fn arrival_text(p: &Peer) -> Option<String> {
    if p.this_pc && !p.server {
        return None;
    }
    Some(if p.server { t!("toast.server_running", name = p.name) } else { t!("toast.playing_too", name = p.name) })
}

/// Players of this game who run another PeerParty: they never show up as players, and
/// this says why.
pub fn other_version_title(st: &GameState) -> String {
    t!("overlay.other_version", n = st.other_version).into()
}

pub fn other_version_detail(st: &GameState) -> String {
    if st.newer_version { t!("overlay.other_version_newer") } else { t!("overlay.other_version_older") }.into()
}

/// An invite's wording from the hook ("invited you", "asks to join you"), translated.
pub fn invite_text(name: &str, kind: &str) -> String {
    match kind {
        "invited you" => t!("overlay.invited_you", name = name),
        "asks to join you" => t!("overlay.asks_to_join", name = name),
        other => format!("{name} {other}"),
    }
}

/// A turned-off layer's overlay row: its name, then what that costs and where it's
/// turned back on (the game's page, or `PEERPARTY_SKIP` for a layer the page doesn't list).
pub fn layer_off_text(layer: &str) -> (String, String) {
    let title = t!("overlay.layer_off", name = layer_name(layer));
    let detail = match layer_off_effect(layer) {
        Some(effect) => format!("{effect} {}", t!("overlay.layer_off_where")),
        None => t!("overlay.layer_off_env").to_string(),
    };
    (title, detail)
}

/// Why there's nothing to invite to, in the overlay's language. Service names are the
/// hook's labels; layers are named as the game page names them.
pub fn block_text(b: &InviteBlock) -> String {
    match b {
        InviteBlock::Official(names) => t!("hook.official", names = names.join(" and "), mode = t!("mode.lan")),
        InviteBlock::PhotonRoom(room) => t!("hook.photon_room", room = room),
        InviteBlock::LayersOff(layers) => t!("hook.layers_off", names = layers.iter().map(|l| layer_name(l)).collect::<Vec<_>>().join(", ")),
        InviteBlock::Unsupported(names) => t!("hook.unsupported", names = names.join(" or ")),
        InviteBlock::LearnFirst => t!("hook.learn_invite").into(),
        InviteBlock::HostFirst => t!("overlay.host_first").into(),
    }
}

/// A notice from the hook as its title and detail, in the overlay's language; free text
/// (an older or newer hook's note) as the hook wrote it.
pub fn notice_text(note: &Notice) -> (String, String) {
    let name = note.name.clone().unwrap_or_default();
    let reason = || note.reason.as_ref().map(block_text).unwrap_or_default();
    match note.code {
        NoticeCode::InviteSent => {
            let to = match &note.name {
                Some(one) => t!("hook.to_name", name = one),
                None => t!("hook.to_players", n = note.count.unwrap_or(0)),
            };
            (t!("hook.invite_sent").into(), to)
        }
        NoticeCode::CantInvite => (t!("hook.cant_invite").into(), reason()),
        NoticeCode::NobodyToInvite => (t!("hook.nobody").into(), t!("hook.nobody_detail").into()),
        NoticeCode::JoinAsked => (t!("hook.asked_to_join").into(), reason()),
        NoticeCode::AlreadyTogether => (t!("hook.already_together").into(), t!("hook.together_detail", name = name)),
        NoticeCode::CantJoin => (t!("hook.cant_join").into(), t!("hook.nowhere_detail", name = name)),
        NoticeCode::NoCodeToPaste => (t!("hook.no_code").into(), t!("hook.no_code_detail").into()),
        NoticeCode::Achievement => (t!("hook.achievement").into(), name),
        NoticeCode::Text => (note.title(), note.detail()),
    }
}

impl Overlay {
    pub fn new(game_name: String) -> Overlay {
        Overlay {
            page: Page::Players,
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
            block_armed: None,
            blocked: None,
            seats: None,
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

    /// The hook's result to show (`note`).
    pub fn notice(&mut self, note: &Notice, visible: bool) {
        let (title, detail) = notice_text(note);
        self.note(title, detail, visible);
    }

    /// A result to show: under the panel while it's open, else as a notification.
    pub fn note(&mut self, title: String, detail: String, visible: bool) {
        if visible {
            let d = toast_duration(&title, &detail);
            self.notice = Some((format!("{title}: {detail}"), Instant::now() + d));
        } else {
            self.toast(title, detail);
        }
    }

    pub fn toast(&mut self, title: impl Into<String>, detail: impl Into<String>) {
        let (title, detail) = (title.into(), detail.into());
        let d = toast_duration(&title, &detail);
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        self.toasts.push(Toast { title, detail, until: now + d });
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
    }

    /// Toasts that couldn't be seen for `d` stay that much longer.
    pub fn delay_toasts(&mut self, d: Duration) {
        for t in &mut self.toasts {
            t.until += d;
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
        if self.state.other_version > 0 {
            rows.push(Row::info(other_version_title(&self.state), other_version_detail(&self.state)).icon(Icon::Warning, Tone::Warn));
        }
        rows.extend(self.state.layers_off.iter().map(|l| {
            let (title, detail) = layer_off_text(l);
            Row { id: format!("layer off {l}"), ..Row::info(title, detail).icon(Icon::Warning, Tone::Warn) }
        }));
        rows
    }

    /// The tabs: Players and Network, with Split screen first in a split-screen session
    /// of two or more players.
    fn tabs(&self) -> Vec<Page> {
        let mut tabs = vec![Page::Players, Page::Network];
        if self.split() {
            tabs.insert(0, Page::SplitScreen);
        }
        tabs
    }

    /// A split-screen session of two or more players on this PC.
    fn split(&self) -> bool {
        self.seats.as_ref().is_some_and(|s| s.players.len() > 1)
    }

    /// Where the overlay opens when nothing asked for a page: Split screen in a
    /// split-screen session with no invite waiting, else Players.
    pub fn landing(&self) -> Page {
        if self.split() && self.state.pending.is_empty() { Page::SplitScreen } else { Page::Players }
    }

    /// What keeps players out, at the top of Players so it's seen on opening: the one
    /// problem itself (the firewall's with Fix), or the first with a way to the rest on
    /// Network.
    fn alert(&self) -> Option<Row> {
        let mut problems = self.problems();
        if problems.len() <= 1 {
            return problems.pop();
        }
        let first = problems.remove(0);
        Some(if first.selectable {
            first.third(Action::Open(PageRef::Network), t!("overlay.tab_network"))
        } else {
            Row { main: Action::Open(PageRef::Network), main_label: t!("overlay.open"), selectable: true, detail: t!("overlay.more_problems").into(), ..first }
        })
    }

    /// Invite everyone on your network: a button when the game has somewhere to invite to
    /// and there's someone on the network to invite (or the game asked to invite);
    /// otherwise why not, when that's worth knowing (a turned-off layer, an official
    /// service, ...). "Host or join a game first" goes without saying.
    fn invite_all(&self) -> Option<Row> {
        let st = &self.state;
        if st.can_invite {
            let anyone = st.peers.iter().any(|p| !p.server && !p.with_me && (p.via.is_empty() || p.via == "lan"));
            if !anyone && !self.invite_mode {
                return None;
            }
            let room = st.lobby.as_ref().is_some_and(|l| l.kind == "photon.room");
            let detail = if self.invite_mode {
                t!("overlay.invite_asked")
            } else if room {
                t!("overlay.invite_code_detail")
            } else {
                t!("overlay.invite_all_detail")
            };
            return Some(Row::action(t!("overlay.invite_all"), detail, Action::InviteAll, t!("overlay.invite")).icon(Icon::People, Tone::Plain));
        }
        let worth = match &st.invite_block {
            Some(InviteBlock::HostFirst) => false,
            Some(_) => true,
            None => !st.invite_note.is_empty(),
        };
        (worth || self.invite_mode).then(|| Row::info(t!("overlay.invite_all"), st.why_no_invite()).icon(Icon::People, Tone::Plain))
    }

    /// Types the room code on the clipboard into the game: closes the overlay first.
    fn paste_row(&self) -> Row {
        Row::action(t!("overlay.paste_code"), t!("overlay.paste_code_detail"), Action::PasteCode, t!("overlay.paste")).icon(Icon::Link, Tone::Plain)
    }

    /// The Photon room the game is in: its code to copy, and who's in it.
    fn room_row(&self, l: &LobbyInfo) -> Row {
        let members: Vec<String> = l.members.iter().map(|(_, n, owner)| if *owner { t!("overlay.host_name", name = n) } else { n.clone() }).collect();
        let title = if l.max > 0 { t!("overlay.in_room_max", code = l.id, n = l.members.len(), max = l.max) } else { t!("overlay.in_room", code = l.id, n = l.members.len()) };
        Row::action(title, members.join(", "), Action::CopyCode, t!("overlay.copy_code")).icon(Icon::Gamepad, Tone::Plain)
    }

    /// The line under the Players tab: the lobby the game is in, its size and service.
    /// A Photon room has a row of its own instead (its code to copy).
    fn caption(&self) -> Option<String> {
        if self.page != Page::Players {
            return None;
        }
        let l = self.state.lobby.as_ref().filter(|l| l.kind != "photon.room")?;
        let mut text = t!("overlay.in_lobby", n = l.members.len(), max = l.max.max(l.members.len() as u32));
        if let Some(kind) = lobby_kind(&l.kind) {
            text.push_str(&format!("  \u{b7}  {kind}"));
        }
        Some(text)
    }

    /// A player's round trip over the internet, as the hook measured it for the tunnel
    /// of that name.
    fn ping_of(&self, p: &Peer) -> Option<String> {
        if !(p.public() || p.via == "room") {
            return None;
        }
        let (_, rtt, _, _, v6) = self.state.internet.iter().flat_map(|n| &n.peers).find(|x| x.0 == p.name)?;
        let ping = t!("overlay.d_ping", ms = (*rtt)?);
        Some(if *v6 { t!("overlay.d_ipv6", ping = ping) } else { ping })
    }

    /// The stand-in address a game that asks for an IP reaches an internet player at.
    fn ip_to_type(&self, p: &Peer) -> Option<String> {
        let (.., address, _) = self.state.internet.iter().flat_map(|n| &n.peers).find(|x| x.0 == p.name)?;
        (!address.is_empty() && address != "hidden").then(|| address.clone())
    }

    /// A player's line under their name: what they're doing, where they play from, their
    /// ping over the internet, and a different version of the game.
    fn peer_detail(&self, p: &Peer) -> String {
        let mut parts = vec![if p.server {
            t!("overlay.dedicated_server").to_string()
        } else if p.with_me {
            t!("overlay.in_your_lobby").to_string()
        } else if p.status.is_empty() {
            t!("overlay.playing").to_string()
        } else {
            p.status.clone()
        }];
        if p.public() {
            parts.push(t!("overlay.public_player").to_string());
        } else if p.via == "room" {
            parts.push(t!("overlay.your_room").to_string());
        } else if let Some(ip) = p.shown_ip() {
            parts.push(ip);
        }
        parts.extend(self.ping_of(p));
        if !p.same_build && !p.server {
            parts.push(t!("overlay.other_version").to_string());
        }
        parts.join("  \u{b7}  ")
    }

    /// A player's one row, with the same buttons wherever it is: A does the likely thing
    /// (join them; invite them when the game asked; their voice once they're with you),
    /// X invites them here, Y their details.
    fn peer_row(&self, i: usize) -> Row {
        let st = &self.state;
        let p = &st.peers[i];
        let (name, detail) = (p.name.clone(), self.peer_detail(p));
        let details = (Action::Profile(i), t!("overlay.details"));
        let r = if p.server {
            // Join asks the game to join the server (the hook's steamgs.rs).
            let r = Row::action(name, detail, Action::Join(i), t!("overlay.join"));
            if p.public() { r.third(details.0, details.1) } else { r }
        } else if self.invite_mode {
            Row::action(name, detail, Action::Invite(i), t!("overlay.invite")).third(details.0, details.1)
        } else if p.with_me && st.voice_in_use {
            Row::action(name, detail, Action::ToggleMute(i), if p.muted { t!("overlay.unmute") } else { t!("overlay.mute") }).third(details.0, details.1)
        } else if p.with_me {
            Row::action(name, detail, details.0, details.1)
        } else {
            let r = Row::action(name, detail, Action::Join(i), t!("overlay.join")).third(details.0, details.1);
            if st.can_invite { r.second(Action::Invite(i), t!("overlay.invite")) } else { r }
        };
        let icon = if p.this_pc && !p.server {
            Icon::Gamepad
        } else if p.public() {
            Icon::Globe
        } else if p.via == "room" {
            Icon::People
        } else {
            Icon::Home
        };
        Row { id: format!("peer {}", p.id), ..r.icon(icon, if p.with_me { Tone::Good } else { Tone::Plain }) }
    }

    /// Players: what keeps them out, what there is to do now (a room code, inviting
    /// everyone, undoing a block, your microphone), then everyone once: invites first,
    /// then this PC's other copies, your network, the internet, dedicated servers.
    fn player_rows(&self) -> Vec<Row> {
        let st = &self.state;
        let mut rows: Vec<Row> = self.alert().into_iter().collect();
        if let Some(l) = st.lobby.as_ref().filter(|l| l.kind == "photon.room") {
            rows.push(self.room_row(l));
        }
        if st.codes {
            rows.push(self.paste_row());
        }
        rows.extend(self.invite_all());
        if let Some((_, name, _)) = self.blocked.as_ref().filter(|b| b.2.elapsed() < BLOCK_UNDO) {
            rows.push(Row::action(t!("overlay.blocked_now", name = name), t!("overlay.blocked_now_detail"), Action::Unblock, t!("overlay.undo")).icon(Icon::Shield, Tone::Warn));
        }
        if st.voice_in_use {
            rows.push(Row::toggle(t!("overlay.mic"), t!("overlay.mic_detail"), st.mic_on, Action::ToggleMic).icon(Icon::Mic, Tone::Plain));
        }
        // Who sent an invite is listed with it, not again below.
        let mut listed = Vec::new();
        if !st.pending.is_empty() {
            rows.push(Row::header(t!("overlay.sec_invites")));
        }
        for (k, inv) in st.pending.iter().enumerate() {
            let from = st.peers.iter().position(|p| !inv.from.is_empty() && p.id == inv.from);
            listed.extend(from);
            let row = if !inv.code.is_empty() {
                Row::action(t!("overlay.code_from", name = inv.name, code = inv.code), t!("overlay.code_answer"), Action::Accept(k), t!("overlay.type_code"))
                    .second(Action::Decline(k), t!("overlay.dismiss"))
            } else {
                let detail = from.map_or_else(|| t!("overlay.invite_answer").to_string(), |i| self.peer_detail(&st.peers[i]));
                let r = Row::action(invite_text(&inv.name, &inv.kind), detail, Action::Accept(k), t!("overlay.accept")).second(Action::Decline(k), t!("overlay.decline"));
                // Their details too: a stranger sending invites can be blocked from there.
                match from {
                    Some(i) => r.third(Action::Profile(i), t!("overlay.details")),
                    None => r,
                }
            };
            rows.push(Row { id: format!("invite {}", inv.id), ..row.icon(Icon::Bell, Tone::Good) });
        }
        let groups: [(&str, fn(&Peer) -> bool); 4] = [
            (t!("overlay.net_pc"), |p| p.this_pc && !p.server),
            (t!("overlay.net_home"), |p| !p.this_pc && !p.server && (p.via.is_empty() || p.via == "lan")),
            (t!("overlay.net_internet"), |p| !p.this_pc && !p.server && (p.via == "room" || p.public())),
            (t!("overlay.sec_servers"), |p| p.server),
        ];
        for (title, belongs) in groups {
            // A server can't be invited anywhere.
            let members: Vec<usize> = (0..st.peers.len()).filter(|&i| belongs(&st.peers[i]) && !listed.contains(&i) && !(st.peers[i].server && self.invite_mode)).collect();
            if members.is_empty() {
                continue;
            }
            rows.push(Row::header(title));
            rows.extend(members.into_iter().map(|i| self.peer_row(i)));
        }
        if st.peers.is_empty() && st.pending.is_empty() {
            rows.push(Row::info(t!("overlay.no_players"), t!("overlay.no_players_detail")));
        }
        rows
    }

    /// A split-screen session's players on this PC: Layout Studio, each player's
    /// controller (Swap, Free), and who has the keyboard and mouse.
    fn split_rows(&self) -> Vec<Row> {
        let Some(s) = self.seats.as_ref().filter(|s| s.players.len() > 1) else { return Vec::new() };
        let mut rows = vec![
            Row::action(t!("overlay.arrange_panes"), t!("overlay.arrange_panes_detail"), Action::ArrangePanes, t!("overlay.open")).icon(Icon::Sliders, Tone::Plain),
            Row::header(t!("overlay.sec_controllers")),
        ];
        for (k, p) in s.players.iter().enumerate() {
            let title = if k == s.me { t!("overlay.player_you", n = k + 1) } else { t!("overlay.player_n", n = k + 1) };
            let mut row = if p.can_swap {
                Row::action(title, p.detail.clone(), Action::SwapSeat(k), t!("overlay.seat_swap"))
            } else if p.can_free {
                Row::action(title, p.detail.clone(), Action::FreeSeat(k), t!("overlay.seat_free"))
            } else {
                Row::info(title, p.detail.clone())
            };
            if p.can_swap && p.can_free {
                row = row.second(Action::FreeSeat(k), t!("overlay.seat_free"));
            }
            rows.push(Row { id: format!("seat {k}"), ..row.icon(Icon::Gamepad, Tone::Plain) });
        }
        rows.push(Row::header(t!("overlay.seat_kbm")));
        let kbm_val = match s.keyboard {
            Some(k) => t!("overlay.player_n", n = k + 1),
            None => t!("common.off").to_string(),
        };
        rows.push(
            Row::setting(t!("overlay.keyboard_to"), t!("overlay.keyboard_to_detail"), kbm_val, Action::KeyboardStep(1), (Action::KeyboardStep(-1), Action::KeyboardStep(1)))
                .icon(Icon::Keyboard, Tone::Plain),
        );
        rows
    }

    /// Network: this PC's problems (the firewall fixed here), the home network, internet
    /// play and its switch.
    fn network_rows(&self) -> Vec<Row> {
        let st = &self.state;
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
        }
        if !st.internet.is_empty() || !st.sdks.is_empty() {
            rows.push(Row::action(t!("overlay.details_title"), t!("overlay.details_sub"), Action::Open(PageRef::Details), t!("overlay.open")).icon(Icon::Info, Tone::Plain));
        }
        rows.push(Row::note(t!("overlay.no_addresses")));
        rows
    }

    /// Connection details: per kind of internet play, the router, port forwarding and
    /// finding players; and the game's services PeerParty answers for. Each player's
    /// ping is on their own row and page.
    fn details_rows(&self) -> Vec<Row> {
        let st = &self.state;
        let mut rows = Vec::new();
        for n in &st.internet {
            rows.push(Row::header(if n.mode == "public" { t!("overlay.via_public") } else { t!("overlay.via_room") }));
            rows.push(Row::info(t!("overlay.d_router"), if n.nat.is_empty() { t!("overlay.unknown").to_string() } else { n.nat.clone() }));
            rows.push(Row::info(t!("overlay.d_ports"), if n.port_map.is_empty() { t!("overlay.d_ports_none").to_string() } else { n.port_map.clone() }));
            rows.push(Row::info(t!("overlay.d_dht"), t!("overlay.d_dht_detail", nodes = n.dht_nodes, found = n.found)));
            if n.blocked > 0 {
                rows.push(Row::info(t!("overlay.d_blocked"), n.blocked.to_string()));
            }
        }
        if !st.sdks.is_empty() {
            rows.push(Row::header(t!("overlay.through")));
            rows.push(Row::info(st.sdks.iter().map(|s| sdk_label(s)).collect::<Vec<_>>().join(", "), "").icon(Icon::Link, Tone::Plain));
        }
        rows.push(Row::note(t!("overlay.no_addresses")));
        rows
    }

    /// One player's page: who and where they are, how they're connected, and only what
    /// can be done with them (no Invite or Join once they're with you, no Invite for a
    /// server or where the game can't).
    fn profile_rows(&self, id: &str) -> Vec<Row> {
        let st = &self.state;
        let Some((i, p)) = st.peers.iter().enumerate().find(|(_, p)| p.id == id) else {
            return vec![Row::info(t!("overlay.left"), "")];
        };
        let mut rows = vec![Row::info(p.name.clone(), self.peer_detail(p))];
        let mut conn = p.connection();
        if let Some(ping) = self.ping_of(p) {
            conn.push_str(&format!("  \u{b7}  {ping}"));
        }
        if let Some(ip) = self.ip_to_type(p) {
            conn.push_str(&t!("overlay.d_ip_to_type", ip = ip));
        }
        rows.push(Row::info(t!("overlay.connection"), conn));
        if st.voice_in_use && !p.server {
            rows.push(Row::toggle(t!("overlay.mute_them"), t!("overlay.mute_them_detail"), p.muted, Action::ToggleMute(i)).icon(Icon::Mic, Tone::Plain));
        }
        if !p.with_me && !p.server && st.can_invite {
            rows.push(Row::action(t!("overlay.invite_to_yours"), "", Action::Invite(i), t!("overlay.invite")).icon(Icon::People, Tone::Plain));
        }
        if !p.with_me {
            rows.push(Row::action(t!("overlay.join_theirs"), "", Action::Join(i), t!("overlay.join")).icon(Icon::Play, Tone::Plain));
        }
        if p.public() {
            let armed = self.block_armed.as_ref().is_some_and(|(a, at)| a == &p.id && at.elapsed() < BLOCK_ARMED);
            let row = if armed {
                Row::action(t!("overlay.block_sure", name = p.name), t!("overlay.block_detail"), Action::Block(i), t!("overlay.block_again"))
            } else {
                Row::action(t!("overlay.block"), t!("overlay.block_detail"), Action::Block(i), t!("overlay.block_button"))
            };
            rows.push(Row { id: "block".into(), ..row.icon(Icon::Shield, Tone::Warn) });
        }
        rows
    }

    fn rows(&self) -> Vec<Row> {
        match &self.page {
            Page::SplitScreen => self.split_rows(),
            Page::Players => self.player_rows(),
            Page::Network => self.network_rows(),
            Page::Details => self.details_rows(),
            Page::Profile(id) => self.profile_rows(id),
        }
    }

    fn act(&mut self, a: Action, prefs: &mut Prefs, out: &mut Vec<Cmd>) {
        let st = &self.state;
        let msg = Cmd::Hook;
        match a {
            Action::InviteAll => out.push(msg(HostMsg::Invite { to: None })),
            Action::CopyCode => {
                if let Some(l) = st.lobby.as_ref().filter(|l| l.kind == "photon.room") {
                    out.push(msg(HostMsg::Copy { text: l.id.clone() }));
                }
            }
            Action::PasteCode => {
                out.push(msg(HostMsg::Paste));
                out.push(Cmd::Close);
            }
            Action::Invite(i) => {
                if let Some(p) = st.peers.get(i) {
                    out.push(msg(HostMsg::Invite { to: Some(p.id.clone()) }));
                }
            }
            Action::Join(i) => {
                if let Some(p) = st.peers.get(i) {
                    out.push(msg(HostMsg::Join { peer: p.id.clone() }));
                    out.push(Cmd::Close);
                }
            }
            Action::Profile(i) => {
                if let Some(p) = st.peers.get(i) {
                    let id = p.id.clone();
                    self.open(Page::Profile(id), false);
                }
            }
            // Two presses: the first asks, the second (within BLOCK_ARMED) blocks.
            Action::Block(i) => {
                if let Some(p) = st.peers.get(i).filter(|p| p.public()) {
                    let armed = self.block_armed.take().is_some_and(|(a, at)| a == p.id && at.elapsed() < BLOCK_ARMED);
                    if armed {
                        out.push(msg(HostMsg::Block { peer: p.id.clone() }));
                        self.blocked = Some((p.id.clone(), p.name.clone(), Instant::now()));
                        self.open(Page::Players, false);
                    } else {
                        self.block_armed = Some((p.id.clone(), Instant::now()));
                    }
                }
            }
            Action::ToggleMic => {
                let on = !self.state.mic_on;
                self.state.mic_on = on;
                out.push(msg(HostMsg::Mic { on }));
            }
            Action::ToggleMute(i) => {
                if let Some(p) = self.state.peers.get_mut(i) {
                    p.muted = !p.muted;
                    out.push(msg(HostMsg::Mute { peer: p.id.clone(), on: p.muted }));
                }
            }
            Action::Unblock => {
                if let Some((id, ..)) = self.blocked.take().filter(|b| b.2.elapsed() < BLOCK_UNDO) {
                    out.push(msg(HostMsg::Unblock { peer: id }));
                }
            }
            Action::Accept(i) => {
                if let Some(p) = st.pending.get(i) {
                    out.push(msg(HostMsg::Accept { id: p.id }));
                    out.push(Cmd::Close);
                }
            }
            Action::Decline(i) => {
                if let Some(p) = st.pending.get(i) {
                    out.push(msg(HostMsg::Decline { id: p.id }));
                }
            }
            Action::Open(to) => {
                let page = match to {
                    PageRef::Network => Page::Network,
                    PageRef::Details => Page::Details,
                };
                self.open(page, false);
            }
            Action::ToggleInternet => {
                // Flips what the game reports (an environment value may differ from the
                // saved setting), saves it for next time, and switches the running game.
                let on = !st.internet_on;
                prefs.internet = on;
                out.push(Cmd::Prefs);
                out.push(msg(HostMsg::Set { key: "internet".into(), value: if on { "on" } else { "off" }.into() }));
                self.state.internet_on = on;
            }
            Action::SwapSeat(k) => out.push(Cmd::SwapSeat(k)),
            Action::FreeSeat(k) => out.push(Cmd::FreeSeat(k)),
            Action::KeyboardStep(d) => out.push(Cmd::KeyboardStep(d)),
            Action::ArrangePanes => out.push(Cmd::OpenLayoutStudio),
            Action::FixFirewall(public) => out.push(Cmd::FixFirewall(public)),
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
            _ => None,
        }
    }

    /// One frame of the full overlay. `screen` is the game's client area in points.
    pub fn frame(&mut self, ui: &mut egui::Ui, mut nav: Nav, prefs: &mut Prefs, out: &mut Vec<Cmd>) {
        let screen = ui.max_rect();
        // A pass egui runs again (`request_discard` below) is the same frame: its `nav`
        // was already acted on. egui doesn't hand that pass the mouse's events again, and
        // nothing here acts on a click in it either, should that change (a switch would
        // flip back).
        let replay = ui.ctx().current_pass_index() > 0;
        if replay {
            nav = Nav::default();
        }
        // Split screen's tab goes with the session's second player.
        if self.page == Page::SplitScreen && !self.split() {
            self.open(Page::Players, self.invite_mode);
        }
        // The rows are built once a frame, and again only when something this frame
        // changed them (another page opened, a row's action done).
        let mut rows = self.rows();
        self.follow(&rows);
        if self.sel_moved.is_some_and(|t| t.elapsed() < MOVED_HOLD) {
            nav.select = false;
            nav.second = false;
            nav.third = false;
        }
        // Left / right change the selected row's value when it has one, and switch pages
        // otherwise.
        if nav.left || nav.right {
            let changes = rows.get(self.sel).is_some_and(|r| r.selectable && r.change.is_some());
            if !changes {
                nav.prev_tab |= nav.left;
                nav.next_tab |= nav.right;
                nav.left = false;
                nav.right = false;
            }
        }
        // Page switching and closing.
        let (page_before, invite_before) = (self.page.clone(), self.invite_mode);
        let tabs = self.tabs();
        let tab_i = tabs.iter().position(|p| *p == self.page.tab()).unwrap_or(0);
        if nav.next_tab || nav.prev_tab {
            let n = tabs.len() as i32;
            let i = (tab_i as i32 + if nav.next_tab { 1 } else { -1 }).rem_euclid(n) as usize;
            self.open(tabs[i].clone(), false);
        }
        // The tab to highlight is the one switched to just now (frames are only drawn
        // on input, so a stale one would stay until the next button press).
        let tab_i = tabs.iter().position(|p| *p == self.page.tab()).unwrap_or(0);
        if nav.close {
            out.push(Cmd::Close);
        }
        if nav.back {
            match self.parent() {
                Some(p) => self.open(p, false),
                None => out.push(Cmd::Close),
            }
        }
        if self.page != page_before || self.invite_mode != invite_before {
            rows = self.rows();
        }
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
            } else if nav.third {
                nav_action = r.third;
            } else if nav.left {
                nav_action = r.change.map(|c| c.0).unwrap_or(Action::None);
            } else if nav.right {
                nav_action = r.change.map(|c| c.1).unwrap_or(Action::None);
            }
        }
        let rows = if nav_action != Action::None {
            self.act(nav_action, prefs, out);
            let rows = self.rows();
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
        if !replay && ui.input(|i| i.pointer.any_pressed() && i.pointer.press_origin().is_some_and(|p| !panel.contains(p))) {
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
        for (i, page) in tabs.iter().enumerate() {
            let name = tab_name(page);
            let badge = match page {
                Page::Players if !self.state.pending.is_empty() => Some((self.state.pending.len().to_string(), GOOD)),
                Page::Network if !self.problems().is_empty() => Some(("!".to_string(), theme::WARN)),
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
            if resp.clicked() && !replay {
                self.open(page.clone(), false);
            }
            x += w + 6.0;
        }
        icons::glyph(&painter, Pos2::new(x + 13.0, tabs_y + 16.0), Glyph::Rb, style);
        // A sub-page says where it is, and a click there goes back; Players says which
        // lobby the game is in.
        let mut top = tabs_y + 46.0;
        let crumb = match &self.page {
            Page::Profile(_) => Some(t!("overlay.crumb_player")),
            Page::Details => Some(t!("overlay.details_title")),
            _ => None,
        };
        if let Some(c) = crumb {
            Icon::Back.paint(&painter, Pos2::new(inner.left() + 8.0, top + 9.0), 12.0, DIM);
            let t = painter.text(Pos2::new(inner.left() + 22.0, top + 9.0), Align2::LEFT_CENTER, c, theme::strong(13.0), TEXT2);
            let hit = Rect::from_min_max(Pos2::new(inner.left(), top - 2.0), Pos2::new(t.right() + 8.0, top + 20.0));
            if ui.interact(hit, ui.scope_id().with("crumb"), egui::Sense::click()).clicked() && !replay {
                if let Some(parent) = self.parent() {
                    self.open(parent, false);
                    ui.ctx().request_discard("the overlay went back");
                }
            }
            top += 24.0;
        } else if let Some(c) = self.caption() {
            Icon::Gamepad.paint(&painter, Pos2::new(inner.left() + 8.0, top + 9.0), 12.0, DIM);
            painter.text(Pos2::new(inner.left() + 22.0, top + 9.0), Align2::LEFT_CENTER, c, theme::font(13.0), DIM);
            top += 24.0;
        }

        // Rows. The scroll area reaches into the panel's margin, with that room inside
        // it, so the selected row's glow isn't cut off at its edges.
        let list = Rect::from_min_max(Pos2::new(inner.left() - LIST_ROOM, top), Pos2::new(inner.right() + LIST_ROOM, inner.bottom() - 36.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(list));
        let scroll_to = std::mem::take(&mut self.scroll_to_sel);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut child, |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: LIST_ROOM as i8, right: LIST_ROOM as i8, top: 6, bottom: 8 }).show(ui, |ui| {
                for (i, row) in rows.iter().enumerate() {
                    if row.header {
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), egui::Sense::hover());
                        ui.painter().text(Pos2::new(rect.left() + 4.0, rect.bottom() - 8.0), Align2::LEFT_BOTTOM, row.title.to_uppercase(), theme::strong(11.5), theme::TEXT3);
                        continue;
                    }
                    if row.note {
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), egui::Sense::hover());
                        ui.painter().text(Pos2::new(rect.left() + 4.0, rect.center().y + 2.0), Align2::LEFT_CENTER, &row.title, theme::font(12.0), theme::TEXT3);
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
                            // A switch, drawn as one (shared with launcher/src/ui/widgets.rs).
                            let on = row.is_on();
                            let sw = Rect::from_min_size(Pos2::new(rx - 40.0, rect.center().y - 11.0), Vec2::new(40.0, 22.0));
                            crate::ui::shared::paint_switch(p, sw, if on { 1.0 } else { 0.0 }, selected);
                        } else {
                            let g = p.text(Pos2::new(rx, rect.center().y), Align2::RIGHT_CENTER, &row.value, theme::strong(14.5), TEXT);
                            if selected && row.change.is_some() {
                                arrow(p, Pos2::new(g.left() - 10.0, rect.center().y), -1.0);
                                arrow(p, Pos2::new(g.right() + 10.0, rect.center().y), 1.0);
                            }
                        }
                    } else if row.selectable {
                        // Buttons on the right, A's leftmost: clickable, with their controller
                        // glyph on the selected row only (elsewhere a glyph-sized gap reads as
                        // an empty radio button).
                        for (action, label, g) in [(row.third, row.third_label, Glyph::North), (row.second, row.second_label, Glyph::West), (row.main, row.main_label, Glyph::South)] {
                            if action == Action::None || label.is_empty() {
                                continue;
                            }
                            let text_w = ui.painter().layout_no_wrap(label.to_string(), theme::strong(13.0), TEXT).size().x;
                            let w = text_w + if selected { 42.0 } else { 26.0 };
                            let br = Rect::from_min_size(Pos2::new(rx - w, rect.center().y - 15.0), Vec2::new(w, 30.0));
                            let bresp = ui.interact(br, ui.scope_id().with(("btn", i, label)), egui::Sense::click());
                            let p = ui.painter();
                            let fill = if bresp.hovered() { Color32::from_rgb(76, 116, 200) } else if selected { Color32::from_white_alpha(30) } else { Color32::from_white_alpha(14) };
                            p.rect_filled(br, 15.0, fill);
                            if selected {
                                icons::glyph(p, Pos2::new(br.left() + 16.0, br.center().y), g, style);
                                p.text(Pos2::new(br.left() + 30.0, br.center().y), Align2::LEFT_CENTER, label, theme::strong(13.0), TEXT);
                            } else {
                                p.text(br.center(), Align2::CENTER_CENTER, label, theme::strong(13.0), TEXT2);
                            }
                            if bresp.clicked() && !replay {
                                pending_action = action;
                            }
                            rx = br.left() - 8.0;
                        }
                    }
                    if resp.clicked() && !replay && row.selectable && pending_action == Action::None {
                        self.sel = i;
                        pending_action = row.main;
                    }
                    if selected && scroll_to {
                        ui.scroll_to_rect(rect, None);
                    }
                }
            });
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
            if r.third != Action::None && !r.third_label.is_empty() {
                hints.push((Glyph::North, r.third_label));
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

    /// The notifications showing now (the ones over go): what they say.
    pub fn toast_list(&mut self) -> Vec<(String, String)> {
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        self.toasts.iter().map(|t| (t.title.clone(), t.detail.clone())).collect()
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
        // As wide as fits a narrow pane (a quarter of a small screen).
        let w = 360.0f32.min(screen.width() - 40.0).max(160.0);
        let painter = ui.painter();
        let mut y = screen.top() + 28.0;
        let mut area = Rect::NOTHING;
        let right = if screen.width() < w + 56.0 { 20.0 } else { 28.0 };
        for t in &self.toasts {
            // A pane too short for the next one: the newest showing are enough.
            if y > screen.bottom() - 60.0 {
                break;
            }
            let dg = painter.layout(t.detail.clone(), theme::font(12.5), DIM, w - 64.0);
            let h = 44.0 + dg.size().y;
            let r = Rect::from_min_size(Pos2::new(screen.right() - w - right, y), Vec2::new(w, h));
            fx::glow(painter, r, 12.0, ACCENT.gamma_multiply(0.22), 16.0);
            fx::gradient(painter, r, 12.0, true, &[(0.0, Color32::from_rgba_unmultiplied(32, 36, 47, 246)), (1.0, Color32::from_rgba_unmultiplied(22, 24, 31, 246))]);
            painter.rect_stroke(r, 12.0, Stroke::new(1.0, theme::line_strong()), egui::StrokeKind::Inside);
            let tile = Rect::from_min_size(r.left_top() + Vec2::new(14.0, 14.0), Vec2::splat(28.0));
            crate::ui::shared::brand_mark(painter, tile);
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
    // Never bigger than the screen (a split-screen player's small pane): cut off, its
    // footer and Close would be out of reach.
    Rect::from_center_size(screen.center(), size.min(screen.size()))
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

/// PeerParty's 3-node brand mark (shared with the launcher window).
fn brand(p: &egui::Painter, r: Rect) {
    crate::ui::shared::brand_mark(p, r);
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
                "port_map": "UPnP: external port 5000 at 203.0.113.7:5000", "peers": [{"name": "Pat", "rtt_ms": 5, "address": "198.18.7.9"}, {"name": "1.1.1.1", "rtt_ms": 6, "address": "203.0.113.8"}]}]
        }));
        let mut text = String::new();
        let pages = [Page::Network, Page::Details, Page::Players, Page::Profile("bb".into()), Page::Profile("cc".into()), Page::Profile("dd".into()), Page::Profile("ee".into())];
        for page in pages {
            o.open(page, false);
            for r in o.rows() {
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
        // The first press asks; the second blocks; Undo takes it back.
        o.act(Action::Block(1), &mut prefs, &mut out);
        assert!(out.is_empty(), "one press blocked");
        o.act(Action::Block(1), &mut prefs, &mut out);
        assert!(matches!(&out[..], [Cmd::Hook(HostMsg::Block { peer })] if peer == "cc"));
        let undo = o.rows().into_iter().find(|r| r.main == Action::Unblock).expect("no Undo row");
        assert!(undo.title.contains(&o.state.peers[1].name), "{}", undo.title);
        out.clear();
        o.act(Action::Unblock, &mut prefs, &mut out);
        assert!(matches!(&out[..], [Cmd::Hook(HostMsg::Unblock { peer })] if peer == "cc"));
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
            ],
            "can_invite": true
        }));
        o.open(Page::Players, false);
        let rows = o.rows();
        let server = rows.iter().find(|r| r.title == "My World").expect("the server is listed");
        assert_eq!(server.detail, "Dedicated server  \u{b7}  192.168.1.20");
        assert!(server.main == Action::Join(0) && server.second_label.is_empty() && server.third_label.is_empty());
        let sam = rows.iter().find(|r| r.title == "Sam").unwrap();
        assert!(sam.detail.contains("a different version of the game"), "a player still gets the warning");
        assert!(!sam.second_label.is_empty(), "a player can still be invited");
        o.open(Page::Players, true);
        let rows = o.rows();
        assert!(!rows.iter().any(|r| r.title == "My World"), "nothing to invite a server to");
        // Players and servers are listed apart, each under its heading.
        o.open(Page::Players, false);
        let headers: Vec<String> = o.rows().iter().filter(|r| r.header).map(|r| r.title.clone()).collect();
        assert_eq!(headers, [t!("overlay.net_home"), t!("overlay.sec_servers")]);
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
    /// With a voice chat on, Players has the microphone switch and a player's page
    /// "Mute their voice"; each goes to the hook.
    #[test]
    fn voice_switches_show_while_voice_is_on() {
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        o.open(Page::Players, false);
        assert!(!o.rows().iter().any(|r| r.main == Action::ToggleMic), "no voice, no switch");
        o.state.voice_in_use = true;
        o.state.mic_on = true;
        assert!(o.rows().iter().any(|r| r.main == Action::ToggleMic));
        // Someone in your lobby: A mutes them right from their row.
        let sam = o.rows().into_iter().find(|r| r.title == "Sam").unwrap();
        assert_eq!((sam.main, sam.main_label, sam.third), (Action::ToggleMute(0), t!("overlay.mute"), Action::Profile(0)));
        let mut out = Vec::new();
        let mut p = Prefs::default();
        o.act(Action::ToggleMic, &mut p, &mut out);
        assert!(matches!(&out[..], [Cmd::Hook(HostMsg::Mic { on: false })]));
        let id = o.state.peers[0].id.clone();
        o.open(Page::Profile(id.clone()), false);
        assert!(o.rows().iter().any(|r| r.main == Action::ToggleMute(0)));
        out.clear();
        o.act(Action::ToggleMute(0), &mut p, &mut out);
        assert!(matches!(&out[..], [Cmd::Hook(HostMsg::Mute { peer, on: true })] if *peer == id));
    }

    #[test]
    fn internet_page_switches_internet_play() {
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        assert!(o.state.internet_on && o.state.internet_configured, "an older hook's state reads as on and set up");
        o.open(Page::Network, false);
        let rows = o.rows();
        let top = rows.iter().find(|r| r.selectable).unwrap();
        assert_eq!((top.title.as_str(), top.value.as_str()), ("Internet play", "On"));
        assert!(top.change.is_none(), "left / right stay free for switching pages");
        assert!(top.main == Action::ToggleInternet);
        assert!(rows.iter().any(|r| r.title.starts_with("Your room")));

        let mut prefs = Prefs::default();
        let mut out = Vec::new();
        o.act(Action::ToggleInternet, &mut prefs, &mut out);
        assert!(!prefs.internet);
        assert!(matches!(&out[..], [Cmd::Prefs, Cmd::Hook(HostMsg::Set { key, value })] if key == "internet" && value == "off"));
        let rows = o.rows();
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
        assert!(matches!(&out[..], [Cmd::Prefs, Cmd::Hook(HostMsg::Set { value, .. })] if value == "on"));

        o.state = GameState::from_json(&serde_json::json!({"t": "state", "internet_on": true, "internet_configured": false, "internet": []}));
        let rows = o.rows();
        assert!(rows.iter().all(|r| r.main != Action::ToggleInternet));
        assert!(rows.iter().any(|r| r.title == "Internet play isn't set up"));
    }

    /// A firewall problem is the Network page's first row (and Players'): Fix asks the host to fix it
    /// (public networks too from the second button), and players we can't reach get a
    /// row saying where to fix it. Without either (an older hook), nothing extra shows.
    #[test]
    fn internet_page_warns_about_the_firewall_and_fixes_it() {
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        assert_eq!(o.state.lan_unreachable, None, "an older hook doesn't say");
        o.open(Page::Network, false);
        let rows = o.rows();
        assert!(!rows.iter().any(|r| matches!(r.main, Action::FixFirewall(_)) || r.title.contains("can't be reached")));

        let mut v = serde_json::json!({"t": "state", "internet": [], "lan_unreachable": 2});
        o.state = GameState::from_json(&v);
        assert_eq!(o.state.lan_unreachable, Some(2));
        o.firewall = Some(Firewall { title: "Windows Firewall blocks game.exe".into(), detail: "Other players can't connect to you".into() });
        let rows = o.rows();
        let top = rows.iter().find(|r| r.selectable).unwrap();
        assert_eq!(top.title, "Windows Firewall blocks game.exe");
        assert!(top.main == Action::FixFirewall(false) && top.second == Action::FixFirewall(true));
        assert!(top.change.is_none(), "left / right stay free for switching pages");
        assert!(rows.iter().any(|r| r.title == "2 players on your network can't be reached"), "{:?}", rows.iter().map(|r| &r.title).collect::<Vec<_>>());
        assert!(!rows.iter().any(|r| r.title.contains("PeerParty version")), "nobody on another version");

        let mut prefs = Prefs::default();
        let mut out = Vec::new();
        o.act(Action::FixFirewall(false), &mut prefs, &mut out);
        o.act(Action::FixFirewall(true), &mut prefs, &mut out);
        assert!(matches!(&out[..], [Cmd::FixFirewall(false), Cmd::FixFirewall(true)]));

        o.open(Page::Players, false);
        let players = o.rows();
        let top = players.iter().find(|r| r.selectable).unwrap();
        assert_eq!(top.title, "Windows Firewall blocks game.exe", "Players leads with it too");
        // Two problems: the first, Fix and all, with a way to the other on Network.
        assert!(top.main == Action::FixFirewall(false) && top.third == Action::Open(PageRef::Network));
        assert!(!players.iter().any(|r| r.title.contains("can't be reached")), "the rest are on Network");
        o.open(Page::Network, false);
        v["lan_unreachable"] = 0.into();
        o.state = GameState::from_json(&v);
        o.firewall = None;
        let rows = o.rows();
        assert!(!rows.iter().any(|r| matches!(r.main, Action::FixFirewall(_)) || r.title.contains("can't be reached")));
    }

    /// Players of the game on another PeerParty never show up as players: the Network
    /// page says how many, and who should update.
    #[test]
    fn players_on_another_peerparty_are_explained() {
        let mut o = Overlay::new("Game".into());
        o.state = GameState::from_json(&serde_json::json!({"t": "state", "internet": [], "lan_unreachable": 0, "lan_other_version": 2, "lan_newer_version": true}));
        o.open(Page::Network, false);
        let rows = o.rows();
        let row = rows.iter().find(|r| r.title == "2 players of this game run a different version of PeerParty").expect("the row");
        assert!(row.detail.starts_with("Theirs is newer: update PeerParty on this PC"), "{}", row.detail);
        o.state = GameState::from_json(&serde_json::json!({"t": "state", "internet": [], "lan_other_version": 1, "lan_newer_version": false}));
        let rows = o.rows();
        let row = rows.iter().find(|r| r.title == "A player of this game runs a different version of PeerParty").expect("the row");
        assert!(row.detail.starts_with("Theirs is older: they need to update"), "{}", row.detail);
        o.state = GameState::from_json(&serde_json::json!({"t": "state", "internet": []}));
        assert!(!o.rows().iter().any(|r| r.title.contains("PeerParty")));
    }

    /// Left or right on the Network page's switch row goes to Players (the other tab).
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
        assert_eq!(o.page, Page::Players);
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
        let pages = [Page::Players, Page::SplitScreen, Page::Network, Page::Details, Page::Profile("cc".into())];
        for (i, page) in pages.into_iter().enumerate() {
            // The second, a split-screen session's own tab (and three tabs).
            o.seats = (i == 1).then(two_seats);
            o.open(page, false);
            o.style = [Style::Xbox, Style::PlayStation, Style::Nintendo, Style::Xbox, Style::PlayStation][i];
            // A page with a message and a tip under the panel.
            if i == 3 {
                o.notice = Some(("Invite sent: to Sam".into(), std::time::Instant::now() + std::time::Duration::from_secs(5)));
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

    /// A game hosting a session that takes no invites (a Photon room): Players says how the
    /// others join it, in the overlay's language, and offers no Invite button.
    #[test]
    fn a_game_without_invites_says_how_to_join() {
        crate::i18n::set("en");
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        o.state.can_invite = false;
        o.state.invite_block = Some(InviteBlock::PhotonRoom("EUCSH".into()));
        o.open(Page::Players, false);
        let rows = o.rows();
        let row = rows.iter().find(|r| r.title == t!("overlay.invite_all")).unwrap();
        assert!(!row.selectable, "nothing to press");
        assert!(row.detail.contains("game's own menu") && row.detail.ends_with("(room EUCSH)"), "{}", row.detail);
        assert_eq!(row.detail, t!("hook.photon_room", room = "EUCSH"));
        // The state carries it as the hook sends it.
        let st = GameState::from_json(&serde_json::json!({"t": "state", "invite_block": InviteBlock::PhotonRoom("EUCSH".into()).to_json().to_string().parse::<Value>().unwrap()}));
        assert_eq!(st.invite_block, Some(InviteBlock::PhotonRoom("EUCSH".into())));
        assert_eq!(st.why_no_invite(), row.detail);
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

    /// A game whose Steam layer the player turned off: Network says so, with what that
    /// costs and where it's turned back on, and Players' invite row names the layer
    /// instead of asking to host first. The hook's other invite notes are translated too.
    #[test]
    fn a_turned_off_layer_is_named_with_what_it_costs() {
        crate::i18n::set("en");
        let st = GameState::from_json(&serde_json::json!({"t": "state", "layers_off": ["padblock", "steam", "calls"]}));
        assert_eq!(st.layers_off, ["padblock", "steam", "calls"]);
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        o.state.can_invite = false;
        o.state.layers_off = st.layers_off;
        // The hook's invite.rs LAYERS_OFF note.
        o.state.invite_block = Some(InviteBlock::LayersOff(vec!["steam".into()]));
        o.open(Page::Network, false);
        let rows = o.rows();
        let steam = rows.iter().find(|r| r.title == t!("overlay.layer_off", name = t!("layer.steam"))).expect("a row for the Steam layer");
        assert!(steam.detail.starts_with(t!("off.steam")) && steam.detail.ends_with(t!("overlay.layer_off_where")), "{}", steam.detail);
        assert!(rows.iter().any(|r| r.title == t!("overlay.layer_off", name = t!("layer.padblock"))));
        // A layer the game page doesn't list: named as written, turned back on where it was turned off.
        let calls = rows.iter().find(|r| r.title == t!("overlay.layer_off", name = "calls")).unwrap();
        assert_eq!(calls.detail, t!("overlay.layer_off_env"));
        o.open(Page::Players, false);
        let rows = o.rows();
        let invite = rows.iter().find(|r| r.title == t!("overlay.invite_all")).unwrap();
        assert_eq!(invite.detail, t!("hook.layers_off", names = t!("layer.steam")));
        assert!(invite.detail.contains("Steam lobbies") && !invite.detail.contains("Host or join"), "{}", invite.detail);
        assert_eq!(
            block_text(&InviteBlock::LayersOff(vec!["eos".into(), "galaxy".into()])),
            t!("hook.layers_off", names = format!("{}, {}", t!("layer.eos"), t!("layer.galaxy")))
        );
        // The game's services left on their own servers, and ones without invites.
        let official = InviteBlock::Official(vec!["Steam".into(), "Photon".into()]);
        assert_eq!(block_text(&official), t!("hook.official", names = "Steam and Photon", mode = t!("mode.lan")));
        assert_eq!(block_text(&InviteBlock::Unsupported(vec!["PlayFab Party".into()])), t!("hook.unsupported", names = "PlayFab Party"));
        assert_eq!(block_text(&InviteBlock::LearnFirst), t!("hook.learn_invite"));
        assert_eq!(block_text(&InviteBlock::HostFirst), t!("overlay.host_first"));
        // A note this build can't read as a block shows as the hook wrote it; none at all
        // asks to host first.
        o.state.invite_block = None;
        o.state.invite_note = "Something the hook said".into();
        assert_eq!(o.state.why_no_invite(), "Something the hook said");
        o.state.invite_note.clear();
        assert_eq!(o.state.why_no_invite(), t!("overlay.host_first"));
    }

    /// The hook's answers to Join and its invite notes name the player (invite.rs
    /// together_detail, nowhere_detail, sent_detail).
    #[test]
    fn notes_with_a_players_name_are_translated() {
        // The language stays English: it's global, and other tests run alongside.
        let text = |n: Notice| notice_text(&n);
        assert_eq!(text(Notice::already_together("Lola")), (t!("hook.already_together").into(), t!("hook.together_detail", name = "Lola")));
        assert_eq!(text(Notice::cant_join("Lola")), (t!("hook.cant_join").into(), t!("hook.nowhere_detail", name = "Lola")));
        assert_eq!(text(Notice::invite_sent(&["Lola".into()])), (t!("hook.invite_sent").into(), t!("hook.to_name", name = "Lola")));
        let three = ["Lola".to_string(), "Timber".into(), "Sam".into()];
        assert_eq!(text(Notice::invite_sent(&three)).1, t!("hook.to_players", n = 3));
        assert_eq!(text(Notice { name: None, ..Notice::invite_sent(&["Lola".into()]) }).1, t!("hook.to_players", n = 1));
        assert_eq!(text(Notice::achievement("First steps")), (t!("hook.achievement").into(), "First steps".into()));
        assert_eq!(text(Notice::nobody_to_invite()), (t!("hook.nobody").into(), t!("hook.nobody_detail").into()));
        assert_eq!(text(Notice::cant_invite(InviteBlock::HostFirst)), (t!("hook.cant_invite").into(), t!("overlay.host_first").into()));
        assert_eq!(text(Notice::join_asked(InviteBlock::LearnFirst)), (t!("hook.asked_to_join").into(), t!("hook.learn_invite").into()));
    }

    /// "Type a copied room code" with nothing code-like copied (invite.rs type_clipboard).
    #[test]
    fn nothing_to_paste_is_translated() {
        assert_eq!(notice_text(&Notice::no_code_to_paste()), (t!("hook.no_code").into(), t!("hook.no_code_detail").into()));
    }

    /// A note without a code this build knows (an older hook's, a newer one's) shows its
    /// own words, as they came.
    #[test]
    fn notes_without_a_known_code_show_their_text() {
        assert_eq!(notice_text(&Notice::text("Something new", "from a newer hook")), ("Something new".into(), "from a newer hook".into()));
        let future = peerparty_protocol::ipc::HookMsg::from_line(r#"{"t":"note","code":"from_the_future","title":"Saved","detail":"Your game was saved"}"#);
        let Some(peerparty_protocol::ipc::HookMsg::Note(n)) = future else { panic!("{future:?}") };
        assert_eq!(n.code, NoticeCode::Text);
        assert_eq!(notice_text(&n), ("Saved".into(), "Your game was saved".into()));
        let mut o = Overlay::new("Game".into());
        o.notice(&n, true);
        assert_eq!(o.notice.as_ref().map(|(s, _)| s.as_str()), Some("Saved: Your game was saved"));
    }

    /// A Photon room: Players says which room we're in and who is there,
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
        o.state.pending = vec![PendingInvite { id: 7, from: String::new(), name: "Lola".into(), kind: "sent you a room code".into(), code: "QWERT".into() }];
        o.open(Page::Players, false);
        let rows = o.rows();
        assert_eq!(o.caption(), None, "the room has its row, not the lobby line");
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
        assert!(out.iter().any(|c| matches!(c, Cmd::Hook(HostMsg::Paste))) && out.iter().any(|c| matches!(c, Cmd::Close)));
        out.clear();
        o.act(Action::Accept(0), &mut prefs, &mut out);
        assert!(out.iter().any(|c| matches!(c, Cmd::Hook(HostMsg::Accept { id: 7 }))) && out.iter().any(|c| matches!(c, Cmd::Close)));
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
            Cmd::Hook(HostMsg::Accept { id }) => Some(*id),
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
        o.open(Page::Players, false);
        press(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        let on_pat = |o: &Overlay| o.rows()[o.sel].title.contains("Pat");
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
        assert!(on_pat(&o), "a row above moved the selection to {:?}", o.rows()[o.sel].title);
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
        o.open(Page::Players, false);
        press(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        while !o.rows()[o.sel].title.contains("Pat") {
            press(&ctx, &mut tex, &mut o, &mut prefs, Nav { down: true, ..Nav::default() });
        }
        o.state = invites_state(&[(3, "Alex")], 0);
        let cmds = press(&ctx, &mut tex, &mut o, &mut prefs, Nav { select: true, ..Nav::default() });
        assert!(cmds.is_empty(), "Select acted at once on {:?}", o.rows()[o.sel].title);
        std::thread::sleep(MOVED_HOLD + Duration::from_millis(50));
        while !o.rows()[o.sel].title.contains("Alex") {
            press(&ctx, &mut tex, &mut o, &mut prefs, Nav { up: true, ..Nav::default() });
        }
        let cmds = press(&ctx, &mut tex, &mut o, &mut prefs, Nav { select: true, ..Nav::default() });
        assert_eq!(accepted(&cmds), Some(3));
    }

    /// The panel fits a split-screen player's small pane (a quarter of a small screen),
    /// and is its usual size on a big one.
    #[test]
    fn the_panel_fits_a_small_pane() {
        let small = Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 340.0));
        let p = panel_rect(small);
        assert!(small.contains_rect(p), "{p:?} in {small:?}");
        let big = Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 900.0));
        assert_eq!(panel_rect(big).size(), Vec2::new(920.0, 700.0));
    }

    /// A mouse click at `at` (pressed in one frame, let go in the next): what the frames
    /// asked for.
    fn click(ctx: &egui::Context, tex: &mut Textures, o: &mut Overlay, prefs: &mut Prefs, at: Pos2) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        for pressed in [true, false] {
            let events = vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE },
            ];
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 900.0))), events, ..Default::default() };
            let out = ctx.run_ui(input, |ui| o.frame(ui, Nav::default(), prefs, &mut cmds));
            tex.apply(out.textures_delta);
        }
        cmds
    }

    /// One click on a row does its action once: the pass egui runs again at once (to
    /// show what the click changed) sees the same click, and mustn't act on it again
    /// (a switch would flip back, an invite be answered twice).
    #[test]
    fn a_click_acts_once() {
        crate::i18n::set("en");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let (mut tex, mut prefs) = (Textures::default(), Prefs::default());
        let mut clicked = false;
        for y in (150..760).step_by(12) {
            let mut o = Overlay::new("Game".into());
            o.state = invites_state(&[(3, "Alex")], 0);
            o.open(Page::Players, false);
            press(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
            let at = Pos2::new(o.panel.unwrap().center().x, y as f32);
            let cmds = click(&ctx, &mut tex, &mut o, &mut prefs, at);
            let answers = cmds.iter().filter(|c| matches!(c, Cmd::Hook(HostMsg::Accept { .. }) | Cmd::Hook(HostMsg::Decline { .. }))).count();
            assert!(answers <= 1, "one click at y {y} answered the invite {answers} times");
            clicked |= answers == 1;
        }
        assert!(clicked, "no click reached the invite's row");
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

    /// Left / right on Players' top row switch pages, and the switched-to tab is the one
    /// highlighted in that same frame (the next frame may be a while away). A row with a
    /// value (who has the keyboard, on Split screen) takes them instead.
    #[test]
    fn top_row_leaves_left_right_to_pages() {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let mut tex = Textures::default();
        let mut prefs = Prefs::default();
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        o.open(Page::Players, false);
        let rows = o.rows();
        assert!(rows.iter().find(|r| r.selectable).is_some_and(|r| r.change.is_none()), "top row takes left / right");
        draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        let before = draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        let switched = draw(&ctx, &mut tex, &mut o, &mut prefs, Nav { left: true, ..Nav::default() });
        assert_eq!(o.page, Page::Network);
        let again = draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        assert_ne!(before.px, switched.px);
        assert_eq!(switched.px, again.px, "the switch frame already shows the new tab");
        o.seats = Some(two_seats());
        o.open(Page::SplitScreen, false);
        draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        while o.rows()[o.sel].change.is_none() {
            draw(&ctx, &mut tex, &mut o, &mut prefs, Nav { down: true, ..Nav::default() });
        }
        let cmds = press(&ctx, &mut tex, &mut o, &mut prefs, Nav { right: true, ..Nav::default() });
        assert_eq!(o.page, Page::SplitScreen);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::KeyboardStep(1))));
    }

    fn two_seats() -> Seats {
        let seat = |d: &str| PlayerSeat { detail: d.into(), can_swap: true, can_free: true };
        Seats { players: vec![seat("Xbox controller"), seat("Xbox controller")], me: 0, keyboard: Some(0) }
    }

    /// A split-screen session opens on its own tab, first of three; an invite waiting
    /// opens Players instead. Alone (or once the second player is gone) there's no such
    /// tab.
    #[test]
    fn split_screen_has_its_own_tab() {
        let mut o = Overlay::new("Game".into());
        o.state = invites_state(&[], 0);
        assert_eq!((o.landing(), o.tabs()), (Page::Players, vec![Page::Players, Page::Network]));
        o.seats = Some(two_seats());
        assert_eq!((o.landing(), o.tabs()), (Page::SplitScreen, vec![Page::SplitScreen, Page::Players, Page::Network]));
        o.state = invites_state(&[(3, "Alex")], 0);
        assert_eq!(o.landing(), Page::Players);
        o.open(Page::SplitScreen, false);
        let rows = o.rows();
        assert!(rows.iter().any(|r| r.main == Action::ArrangePanes));
        assert!(rows.iter().any(|r| r.main == Action::SwapSeat(1) && r.second == Action::FreeSeat(1)));
        assert!(rows.iter().any(|r| r.change == Some((Action::KeyboardStep(-1), Action::KeyboardStep(1)))));
        // Its players aren't listed on Players too.
        o.open(Page::Players, false);
        assert!(!o.rows().iter().any(|r| matches!(r.main, Action::SwapSeat(_) | Action::FreeSeat(_) | Action::ArrangePanes)));
        // The session down to one player: back to Players.
        o.seats = None;
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let (mut tex, mut prefs) = (Textures::default(), Prefs::default());
        o.page = Page::SplitScreen;
        press(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        assert_eq!(o.page, Page::Players);
    }

    /// Everyone is listed once: who sent an invite is on the invite's row (Accept,
    /// Decline, Details), not again under their network; the rest in groups, with the
    /// same buttons: Join, Invite (where the game can), Details.
    #[test]
    fn each_player_is_listed_once_with_the_same_buttons() {
        crate::i18n::set("en");
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        o.open(Page::Players, false);
        let rows = o.rows();
        let alex: Vec<&Row> = rows.iter().filter(|r| r.title.contains("Alex")).collect();
        assert_eq!(alex.len(), 1, "{:?}", rows.iter().map(|r| &r.title).collect::<Vec<_>>());
        assert_eq!((alex[0].main, alex[0].second, alex[0].third), (Action::Accept(0), Action::Decline(0), Action::Profile(1)));
        assert!(alex[0].detail.contains("In the menus"), "{}", alex[0].detail);
        assert!(rows.iter().any(|r| r.header && r.title == t!("overlay.sec_invites")));
        // The lobby's line, under the tabs.
        assert!(o.caption().is_some_and(|c| c.contains("2 of 4") && c.contains("Epic Online Services")), "{:?}", o.caption());
        // Without the invite: Join, Invite, Details.
        o.state.pending.clear();
        let alex = o.rows().into_iter().find(|r| r.title == "Alex").unwrap();
        assert_eq!((alex.main, alex.second, alex.third), (Action::Join(1), Action::Invite(1), Action::Profile(1)));
        // Somewhere the game can't invite to: no Invite.
        o.state.can_invite = false;
        let alex = o.rows().into_iter().find(|r| r.title == "Alex").unwrap();
        assert_eq!((alex.main, alex.second, alex.third), (Action::Join(1), Action::None, Action::Profile(1)));
        // Nothing to say about inviting when the game just isn't hosting yet.
        o.state.invite_block = Some(InviteBlock::HostFirst);
        assert!(!o.rows().iter().any(|r| r.title == t!("overlay.invite_all")));
        // Someone in a room: their ping on their row.
        o.state.peers[1].via = "room".into();
        o.state.internet[0].peers[0].0 = "Alex".into();
        let alex = o.rows().into_iter().find(|r| r.title == "Alex").unwrap();
        assert!(alex.detail.contains("23 ms over IPv6"), "{}", alex.detail);
    }

    /// A player's page offers only what can be done: no Invite or Join for someone in
    /// your lobby, no Invite for a server or where the game can't invite.
    #[test]
    fn a_players_page_offers_only_what_can_be_done() {
        let mut o = Overlay::new("Game".into());
        o.state = sample_state();
        let actions = |o: &mut Overlay, id: &str| -> Vec<Action> {
            o.open(Page::Profile(id.into()), false);
            o.rows().iter().filter(|r| r.selectable).map(|r| r.main).collect()
        };
        assert!(actions(&mut o, "bb").is_empty(), "Sam is in your lobby");
        assert_eq!(actions(&mut o, "cc"), [Action::Invite(1), Action::Join(1)]);
        o.state.can_invite = false;
        assert_eq!(actions(&mut o, "cc"), [Action::Join(1)]);
        o.state.can_invite = true;
        o.state.peers[1].server = true;
        assert_eq!(actions(&mut o, "cc"), [Action::Join(1)]);
    }

    #[test]
    fn toast_duration_scales_with_length() {
        assert_eq!(toast_duration("Sam is playing too", "LAN"), Duration::from_secs(8));
        let saves = toast_duration("EOS saves stay on this PC", "Saved locally by PeerParty. They won't sync to the cloud");
        assert!(saves >= Duration::from_secs(12), "saves toast should stay readable: {saves:?}");
        assert_eq!(toast_duration("Title", &"x".repeat(300)), Duration::from_secs(15));
    }

    /// The README's overlay screenshot (docs/images/overlay.png): made-up players over a
    /// dusk drawn in code. Run by hand:
    /// `PEERPARTY_PREVIEW_DIR=<dir> cargo test -p peerparty readme_ -- --ignored`.
    #[test]
    #[ignore = "makes the README's screenshots; run by hand with PEERPARTY_PREVIEW_DIR"]
    fn readme_overlay_screenshot() {
        let Some(dir) = std::env::var_os("PEERPARTY_PREVIEW_DIR") else { return };
        crate::i18n::set("en");
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        let (mut tex, mut prefs) = (Textures::default(), Prefs::default());
        let mut o = Overlay::new("Sample Game".into());
        o.state = GameState::from_json(&serde_json::json!({
            "t": "state",
            "me": {"id": "aa", "name": "Robin"},
            "voice": {"in_use": true, "mic": true},
            "peers": [
                {"id": "bb", "name": "Jules", "with_me": true, "ip": "192.168.1.24", "via": "lan"},
                {"id": "cc", "name": "Mika", "with_me": true, "via": "room"},
                {"id": "dd", "name": "Nova", "status": "In the menus", "via": "public", "fp": "4f2a-91c0-77de-0b13"},
                {"id": "ee", "name": "Theo", "status": "Exploring", "ip": "192.168.1.31", "via": "lan"}
            ],
            "lobby": {"id": "x", "kind": "eos.lobby", "max": 4, "members": [
                {"id": "aa", "name": "Robin", "owner": true}, {"id": "bb", "name": "Jules"}, {"id": "cc", "name": "Mika"}
            ]},
            "pending": [{"id": 3, "from": "dd", "name": "Nova", "kind": "invited you"}],
            "can_invite": true,
            "internet": [
                {"mode": "room", "online": true, "nat": "hole-punch friendly", "peers": [{"name": "Mika", "rtt_ms": 28}]},
                {"mode": "public", "online": true, "nat": "hole-punch friendly", "peers": [{"name": "Nova", "rtt_ms": 41}]}
            ]
        }));
        o.open(Page::Players, false);
        draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        let c = draw(&ctx, &mut tex, &mut o, &mut prefs, Nav::default());
        // A dusk: a sky going warm toward the horizon, two ranges of hills.
        let mix = |a: [f32; 3], b: [f32; 3], t: f32| -> [f32; 3] { [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t.clamp(0.0, 1.0)) };
        let rgb: Vec<u8> = c
            .px
            .iter()
            .enumerate()
            .flat_map(|(i, p)| {
                let (x, y) = ((i % c.w) as f32, (i / c.w) as f32 / c.h as f32);
                let mut bg = mix([18.0, 20.0, 48.0], [196.0, 110.0, 70.0], (y - 0.15) / 0.55);
                for (base, amp, freq, color) in [(0.62, 0.05, 0.004, [52.0, 36.0, 70.0]), (0.74, 0.04, 0.007, [24.0, 20.0, 40.0])] {
                    if y > base + amp * (x * freq).sin() + amp * 0.5 * (x * freq * 2.7 + 1.3).sin() {
                        bg = color;
                    }
                }
                // The overlay is drawn premultiplied over the game.
                let a = (p >> 24) as f32 / 255.0;
                [(p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff].into_iter().zip(bg).map(move |(v, b)| (v as f32 + b * (1.0 - a)).min(255.0) as u8)
            })
            .collect();
        image::RgbImage::from_raw(c.w as u32, c.h as u32, rgb).unwrap().save(std::path::Path::new(&dir).join("overlay.png")).unwrap();
    }
}
