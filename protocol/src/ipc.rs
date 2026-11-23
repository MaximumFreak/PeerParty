//! The messages between the launcher and the hook in the game: one line of JSON each,
//! with its type in `t`.
//!
//! Up, hook to launcher (`HookMsg`): `state` (who we are, the players, the lobby,
//! invites waiting, settings), `open` (the game asked for an overlay page), `note` (a
//! result for the player, as a `Notice`), `local_saves`, `close`, `log`, `viewport` (the
//! game's frame size, window and graphics API), `surface` (the shared textures and fences
//! of its frames for a compositor, `frame_ring`).
//!
//! Down, launcher to hook (`HostMsg`): `invite`, `accept` / `decline`, `paste` / `copy`,
//! `join`, `block` / `unblock`, `mic` / `mute`, `set`, `overlay`, `pads_held`, `note` (a
//! line for the game's log), `frame` (the overlay frame's block), `instance` (this copy's
//! `InstanceSpec`, changed), `input_shm` (its virtual input block), `capture` (its frames
//! for a compositor, on or off, with the `frame_ring` block and the event to set).
//!
//! Notices carry a code and their arguments, so the launcher says them in the player's
//! language without matching English sentences. Their English (`Notice::title` and
//! `detail`) goes along too, for logs and for a reader that doesn't know the code.

use std::io::BufRead;

use crate::json::Json;

/// The longest line the launcher takes from the hook (a state with many players and
/// lobbies is a few tens of KB).
pub const MAX_HOOK_LINE: usize = 4 << 20;
/// The longest line the hook takes from the launcher (a frame name, settings, a paste).
pub const MAX_HOST_LINE: usize = 1 << 20;

/// The most items a notice's list keeps (services, layers), and the longest text kept.
const MAX_ITEMS: usize = 32;
const MAX_TEXT: usize = 4096;

/// Reads one line of at most `max` bytes into `buf`, without its `\n` or `\r\n`: false at
/// the end, on an error, or for a line that runs past `max` or the end of the stream
/// (the sender is broken: the connection ends rather than growing without end).
pub fn read_line_capped<R: BufRead>(r: &mut R, buf: &mut Vec<u8>, max: usize) -> bool {
    use std::io::Read;
    buf.clear();
    match r.by_ref().take(max as u64 + 1).read_until(b'\n', buf) {
        Ok(0) | Err(_) => false,
        Ok(_) if buf.last() == Some(&b'\n') => {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            true
        }
        Ok(_) => false,
    }
}

/// A message's type (`t`), whichever way it goes.
pub fn msg_type(v: &Json) -> Option<&str> {
    v.get("t").as_str()
}

fn text(v: &Json, key: &str) -> Option<String> {
    v.get(key).as_str().map(|s| s.chars().take(MAX_TEXT).collect())
}

fn texts(v: &Json, key: &str) -> Vec<String> {
    v.get(key).as_arr().iter().filter_map(Json::as_str).take(MAX_ITEMS).map(|s| s.chars().take(MAX_TEXT).collect()).collect()
}

fn strs(items: &[String]) -> Json {
    Json::Arr(items.iter().map(|s| Json::str(s.as_str())).collect())
}

fn opt_str(s: &Option<String>) -> Json {
    s.as_deref().map_or(Json::Null, Json::str)
}

fn flag(v: &Json, key: &str) -> bool {
    v.get(key).as_bool() == Some(true)
}

fn u32_of(v: &Json, key: &str) -> Option<u32> {
    v.get(key).as_u64().and_then(|n| u32::try_from(n).ok())
}

/// A field that may be missing (`default`) but, when there, must be a `u32`.
fn u32_or(v: &Json, key: &str, default: u32) -> Option<u32> {
    match v.get(key) {
        Json::Null => Some(default),
        n => n.as_u64().and_then(|n| u32::try_from(n).ok()),
    }
}

/// Why there's nothing to invite to (hook/src/invite.rs `why_not_for`): the overlay's
/// note after Invite or a Join request, and the state's `invite_note`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InviteBlock {
    /// The game's online play is on these services' own servers (their labels).
    Official(Vec<String>),
    /// The game hosts a room on PeerParty's Photon, which has no invites: its name.
    PhotonRoom(String),
    /// The layers the game's lobbies travel through are turned off (their ids).
    LayersOff(Vec<String>),
    /// The game's services carry no invite PeerParty can hand it (their labels).
    Unsupported(Vec<String>),
    /// In a lobby, but PeerParty hasn't learned how to hand it over yet.
    LearnFirst,
    /// Not hosting or in a game yet.
    HostFirst,
}

impl InviteBlock {
    pub fn code(&self) -> &'static str {
        match self {
            InviteBlock::Official(_) => "official",
            InviteBlock::PhotonRoom(_) => "photon_room",
            InviteBlock::LayersOff(_) => "layers_off",
            InviteBlock::Unsupported(_) => "unsupported",
            InviteBlock::LearnFirst => "learn_first",
            InviteBlock::HostFirst => "host_first",
        }
    }

    /// The hook's English for it.
    pub fn english(&self) -> String {
        match self {
            InviteBlock::Official(names) => format!(
                "This game's online play goes through {}'s own servers, which PeerParty can't invite through: use the game's own invites or room codes, or set it to PeerParty's own in this game's page in PeerParty (Advanced, Online services)",
                names.join(" and ")
            ),
            InviteBlock::PhotonRoom(room) => {
                format!("This game has no invites: players on your network join your session from the game's own menu (room {room})")
            }
            InviteBlock::LayersOff(layers) => format!("Turned off for this game in PeerParty: {}", layers.join(", ")),
            InviteBlock::Unsupported(names) => format!("Not supported for this game yet: PeerParty can't hand it an invite through {}", names.join(" or ")),
            InviteBlock::LearnFirst => "Invite someone once from the game's friends list first. After that, Invite and Join work here".into(),
            InviteBlock::HostFirst => "Host or join a game first".into(),
        }
    }

    pub fn to_json(&self) -> Json {
        let mut pairs = vec![("code", Json::str(self.code()))];
        match self {
            InviteBlock::Official(items) | InviteBlock::LayersOff(items) | InviteBlock::Unsupported(items) => pairs.push(("items", strs(items))),
            InviteBlock::PhotonRoom(room) => pairs.push(("room", Json::str(room.as_str()))),
            InviteBlock::LearnFirst | InviteBlock::HostFirst => {}
        }
        Json::obj(pairs)
    }

    pub fn from_json(v: &Json) -> Option<InviteBlock> {
        Some(match v.get("code").as_str()? {
            "official" => InviteBlock::Official(texts(v, "items")),
            "photon_room" => InviteBlock::PhotonRoom(text(v, "room")?),
            "layers_off" => InviteBlock::LayersOff(texts(v, "items")),
            "unsupported" => InviteBlock::Unsupported(texts(v, "items")),
            "learn_first" => InviteBlock::LearnFirst,
            "host_first" => InviteBlock::HostFirst,
            _ => return None,
        })
    }
}

/// What a notice is about. Its wire name is `as_str`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoticeCode {
    /// An invite went: to one player (`name`) or to `count`.
    InviteSent,
    /// Invite found nothing to invite to (`reason`).
    CantInvite,
    /// No other player of this game to invite.
    NobodyToInvite,
    /// A player asked to join, and there's nothing to invite them to (`reason`).
    JoinAsked,
    /// Join's answer: already playing with that player (`name`).
    AlreadyTogether,
    /// Join's answer: that player (`name`) isn't anywhere to join yet.
    CantJoin,
    /// Paste found nothing code-like on the clipboard.
    NoCodeToPaste,
    /// The game unlocked an achievement (`name`).
    Achievement,
    /// Free text (`fallback_title`, `fallback_detail`): an older hook's note.
    Text,
}

impl NoticeCode {
    pub const ALL: [NoticeCode; 9] = [
        NoticeCode::InviteSent,
        NoticeCode::CantInvite,
        NoticeCode::NobodyToInvite,
        NoticeCode::JoinAsked,
        NoticeCode::AlreadyTogether,
        NoticeCode::CantJoin,
        NoticeCode::NoCodeToPaste,
        NoticeCode::Achievement,
        NoticeCode::Text,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NoticeCode::InviteSent => "invite_sent",
            NoticeCode::CantInvite => "cant_invite",
            NoticeCode::NobodyToInvite => "nobody_to_invite",
            NoticeCode::JoinAsked => "join_asked",
            NoticeCode::AlreadyTogether => "already_together",
            NoticeCode::CantJoin => "cant_join",
            NoticeCode::NoCodeToPaste => "no_code",
            NoticeCode::Achievement => "achievement",
            NoticeCode::Text => "text",
        }
    }

    pub fn parse(s: &str) -> Option<NoticeCode> {
        NoticeCode::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// A short message for the player about something they just did: the overlay shows it,
/// or a notification while the overlay is closed. Build one with its constructor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub code: NoticeCode,
    /// A player's name (an invite to one, Join's answers) or an achievement's.
    pub name: Option<String>,
    /// How many players an invite went to.
    pub count: Option<u64>,
    /// Why nothing could be invited to.
    pub reason: Option<InviteBlock>,
    /// The English, for `Text` (and what a reader shows for a code it doesn't know).
    pub fallback_title: Option<String>,
    pub fallback_detail: Option<String>,
}

impl Notice {
    fn new(code: NoticeCode) -> Notice {
        Notice { code, name: None, count: None, reason: None, fallback_title: None, fallback_detail: None }
    }

    /// An invite reached these players (by name).
    pub fn invite_sent(reached: &[String]) -> Notice {
        let mut n = Notice::new(NoticeCode::InviteSent);
        n.count = Some(reached.len() as u64);
        if let [one] = reached {
            n.name = Some(one.clone());
        }
        n
    }

    pub fn cant_invite(reason: InviteBlock) -> Notice {
        Notice { reason: Some(reason), ..Notice::new(NoticeCode::CantInvite) }
    }

    pub fn nobody_to_invite() -> Notice {
        Notice::new(NoticeCode::NobodyToInvite)
    }

    pub fn join_asked(reason: InviteBlock) -> Notice {
        Notice { reason: Some(reason), ..Notice::new(NoticeCode::JoinAsked) }
    }

    pub fn already_together(name: &str) -> Notice {
        Notice { name: Some(name.into()), ..Notice::new(NoticeCode::AlreadyTogether) }
    }

    pub fn cant_join(name: &str) -> Notice {
        Notice { name: Some(name.into()), ..Notice::new(NoticeCode::CantJoin) }
    }

    pub fn no_code_to_paste() -> Notice {
        Notice::new(NoticeCode::NoCodeToPaste)
    }

    pub fn achievement(name: &str) -> Notice {
        Notice { name: Some(name.into()), ..Notice::new(NoticeCode::Achievement) }
    }

    pub fn text(title: &str, detail: &str) -> Notice {
        Notice { fallback_title: Some(title.into()), fallback_detail: Some(detail.into()), ..Notice::new(NoticeCode::Text) }
    }

    /// Its title in English, as the hook's log and older launchers know it.
    pub fn title(&self) -> String {
        match self.code {
            NoticeCode::InviteSent => "Invite sent".into(),
            NoticeCode::CantInvite => "Can't invite".into(),
            NoticeCode::NobodyToInvite => "Nobody to invite".into(),
            NoticeCode::JoinAsked => "A player asked to join you".into(),
            NoticeCode::AlreadyTogether => "Already playing together".into(),
            NoticeCode::CantJoin => "Can't join".into(),
            NoticeCode::NoCodeToPaste => "No code to paste".into(),
            NoticeCode::Achievement => "Achievement unlocked".into(),
            NoticeCode::Text => self.fallback_title.clone().unwrap_or_default(),
        }
    }

    /// Its detail in English.
    pub fn detail(&self) -> String {
        let name = self.name.as_deref().unwrap_or("That player");
        let reason = || self.reason.as_ref().map(InviteBlock::english).unwrap_or_default();
        match self.code {
            NoticeCode::InviteSent => match &self.name {
                Some(one) => format!("To {one}"),
                None => format!("To {} players", self.count.unwrap_or(0)),
            },
            NoticeCode::CantInvite | NoticeCode::JoinAsked => reason(),
            NoticeCode::NobodyToInvite => "No other player of this game is running on your network, or they're all with you already".into(),
            NoticeCode::AlreadyTogether => format!("You're already in {name}'s game"),
            NoticeCode::CantJoin => format!("{name} isn't in a game you can join yet"),
            NoticeCode::NoCodeToPaste => "Copy the room code first, then try again".into(),
            NoticeCode::Achievement => self.name.clone().unwrap_or_default(),
            NoticeCode::Text => self.fallback_detail.clone().unwrap_or_default(),
        }
    }

    /// The `note` message: its code and arguments, and its English as `title` / `detail`.
    pub fn to_json(&self) -> Json {
        let mut pairs = vec![("t", Json::str("note")), ("code", Json::str(self.code.as_str()))];
        if let Some(n) = &self.name {
            pairs.push(("name", Json::str(n.as_str())));
        }
        if let Some(c) = self.count {
            pairs.push(("count", Json::u64(c)));
        }
        if let Some(r) = &self.reason {
            pairs.push(("reason", r.to_json()));
        }
        pairs.push(("title", Json::str(self.title())));
        pairs.push(("detail", Json::str(self.detail())));
        Json::obj(pairs)
    }

    /// A `note` message. One without a code, or with one this build doesn't know, is
    /// `Text` with its `title` and `detail`.
    pub fn from_json(v: &Json) -> Notice {
        let code = v.get("code").as_str().and_then(NoticeCode::parse);
        let reason = InviteBlock::from_json(v.get("reason"));
        let known = match code {
            Some(NoticeCode::CantInvite | NoticeCode::JoinAsked) => reason.is_some(),
            Some(NoticeCode::AlreadyTogether | NoticeCode::CantJoin | NoticeCode::Achievement) => v.get("name").as_str().is_some(),
            Some(_) => true,
            None => false,
        };
        let Some(code) = code.filter(|c| known && *c != NoticeCode::Text) else {
            return Notice::text(&text(v, "title").unwrap_or_default(), &text(v, "detail").unwrap_or_default());
        };
        // A known code's English is made again from its arguments.
        Notice { code, name: text(v, "name"), count: v.get("count").as_u64(), reason, fallback_title: None, fallback_detail: None }
    }
}

/// The launcher's commands to the hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostMsg {
    /// Invite one player (their peer id, hex) or, with None, everyone on this network.
    Invite { to: Option<String> },
    /// Answer a waiting invite.
    Accept { id: u64 },
    Decline { id: u64 },
    /// Type the clipboard's room code into the game (the overlay is closing).
    Paste,
    /// Put a room code on the clipboard.
    Copy { text: String },
    /// Ask a player to invite us.
    Join { peer: String },
    /// Block a public player, or undo that (in this run).
    Block { peer: String },
    Unblock { peer: String },
    /// The overlay's voice switches: the microphone, one player.
    Mic { on: bool },
    Mute { peer: String, on: bool },
    /// A setting changed (`auto_accept`, `invite_buttons`, `internet`, ...).
    Set { key: String, value: String },
    /// The overlay showed (with what opened it) or hid (with whether controller buttons
    /// are still held down from it).
    Overlay { visible: bool, why: Option<String>, pads_held: bool },
    /// Whether the buttons down when the overlay closed are still down.
    PadsHeld { on: bool },
    /// A line for the game's log.
    Note { text: String },
    /// The overlay frame's block (a mapping name, or a `ShmBacking` string).
    Frame { name: String },
    /// This copy's place changed: its pane, virtual screen, focus spoofing or input.
    Instance(crate::session::InstanceSpec),
    /// This copy's virtual input block (`input_shm`), a `ShmBacking` string.
    InputShm { name: String },
    /// Share this copy's frames with the compositor (textures reported in a `surface`,
    /// the newest finished one said in the `frame_ring` block `ring`, a mapping name or a
    /// `ShmBacking` string, and the event named `event` set after each), or stop. With
    /// `pixels`, the frames are read back and written into a file of the hook's own next
    /// to the ring's (`SurfaceMsg::pixels`) instead: what a game under Wine can share with
    /// a native Linux window, which can't open Windows' shared textures.
    Capture { enabled: bool, ring: Option<String>, event: Option<String>, pixels: bool },
    /// This copy's assigned controller arrived, changed, or went away: tell its windows
    /// so engines that only re-check controllers on `WM_DEVICECHANGE` scan again.
    DevicesChanged,
}

impl HostMsg {
    pub fn to_json(&self) -> Json {
        let t = |t: &str, mut rest: Vec<(&str, Json)>| {
            rest.insert(0, ("t", Json::str(t)));
            Json::obj(rest)
        };
        match self {
            HostMsg::Invite { to } => t("invite", vec![("to", opt_str(to))]),
            HostMsg::Accept { id } => t("accept", vec![("id", Json::u64(*id))]),
            HostMsg::Decline { id } => t("decline", vec![("id", Json::u64(*id))]),
            HostMsg::Paste => t("paste", vec![]),
            HostMsg::Copy { text } => t("copy", vec![("text", Json::str(text.as_str()))]),
            HostMsg::Join { peer } => t("join", vec![("peer", Json::str(peer.as_str()))]),
            HostMsg::Block { peer } => t("block", vec![("peer", Json::str(peer.as_str()))]),
            HostMsg::Unblock { peer } => t("unblock", vec![("peer", Json::str(peer.as_str()))]),
            HostMsg::Mic { on } => t("mic", vec![("on", Json::Bool(*on))]),
            HostMsg::Mute { peer, on } => t("mute", vec![("peer", Json::str(peer.as_str())), ("on", Json::Bool(*on))]),
            HostMsg::Set { key, value } => t("set", vec![("key", Json::str(key.as_str())), ("value", Json::str(value.as_str()))]),
            HostMsg::Overlay { visible, why, pads_held } => {
                t("overlay", vec![("visible", Json::Bool(*visible)), ("why", opt_str(why)), ("pads_held", Json::Bool(*pads_held))])
            }
            HostMsg::PadsHeld { on } => t("pads_held", vec![("on", Json::Bool(*on))]),
            HostMsg::Note { text } => t("note", vec![("text", Json::str(text.as_str()))]),
            HostMsg::Frame { name } => t("frame", vec![("name", Json::str(name.as_str()))]),
            HostMsg::Instance(spec) => t("instance", vec![("spec", spec.to_json())]),
            HostMsg::InputShm { name } => t("input_shm", vec![("name", Json::str(name.as_str()))]),
            HostMsg::Capture { enabled, ring, event, pixels } => t(
                "capture",
                vec![("enabled", Json::Bool(*enabled)), ("ring", opt_str(ring)), ("event", opt_str(event)), ("pixels", Json::Bool(*pixels))],
            ),
            HostMsg::DevicesChanged => t("devices_changed", vec![]),
        }
    }

    /// A command, or None for one this build doesn't know or that lacks what it needs.
    pub fn from_json(v: &Json) -> Option<HostMsg> {
        let s = |k: &str| text(v, k);
        Some(match msg_type(v)? {
            "invite" => HostMsg::Invite { to: s("to") },
            "accept" => HostMsg::Accept { id: v.get("id").as_u64()? },
            "decline" => HostMsg::Decline { id: v.get("id").as_u64()? },
            "paste" => HostMsg::Paste,
            "copy" => HostMsg::Copy { text: s("text")? },
            "join" => HostMsg::Join { peer: s("peer")? },
            "block" => HostMsg::Block { peer: s("peer")? },
            "unblock" => HostMsg::Unblock { peer: s("peer")? },
            "mic" => HostMsg::Mic { on: flag(v, "on") },
            "mute" => HostMsg::Mute { peer: s("peer")?, on: flag(v, "on") },
            "set" => HostMsg::Set { key: s("key")?, value: s("value").unwrap_or_default() },
            "overlay" => HostMsg::Overlay { visible: flag(v, "visible"), why: s("why"), pads_held: flag(v, "pads_held") },
            "pads_held" => HostMsg::PadsHeld { on: flag(v, "on") },
            "note" => HostMsg::Note { text: s("text")? },
            "frame" => HostMsg::Frame { name: s("name")? },
            "instance" => HostMsg::Instance(crate::session::InstanceSpec::from_json(v.get("spec"))?),
            "input_shm" => HostMsg::InputShm { name: s("name")? },
            "capture" => HostMsg::Capture { enabled: flag(v, "enabled"), ring: s("ring"), event: s("event"), pixels: flag(v, "pixels") },
            "devices_changed" => HostMsg::DevicesChanged,
            _ => return None,
        })
    }

    /// The message as one line, without its `\n`.
    pub fn to_line(&self) -> String {
        self.to_json().to_string()
    }

    pub fn from_line(line: &str) -> Option<HostMsg> {
        HostMsg::from_json(&Json::parse(line)?)
    }
}

/// What a `surface` carries (`HookMsg::Surface`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SurfaceMsg {
    pub pid: u32,
    /// Counts up from 1 with each new set of textures.
    pub generation: u32,
    /// `frame_ring::SLOTS` of them (none when the frames come as `pixels`).
    pub textures: Vec<u64>,
    pub content: u64,
    pub release: u64,
    pub width: u32,
    pub height: u32,
    /// A `DXGI_FORMAT` (typeless for textures, typed for pixels).
    pub format: u32,
    pub api: String,
    /// How the frames' values are encoded: a `DXGI_COLOR_SPACE_TYPE`
    /// (`frame_ring::COLOR_SPACE_*`), sRGB unless the game said otherwise.
    pub color_space: u32,
    /// The frames as pixels instead (`HostMsg::Capture`'s `pixels`): the file the hook
    /// writes them into, `frame_ring::SLOTS` slots of `frame_ring::slot_bytes` each.
    pub pixels: Option<String>,
    /// Bytes from one row of `pixels` to the next.
    pub stride: u32,
}

impl SurfaceMsg {
    fn from_json(v: &Json) -> Option<SurfaceMsg> {
        let pixels = match v.get("pixels") {
            Json::Null => None,
            p => Some(p.as_str().filter(|p| !p.is_empty() && p.len() <= MAX_TEXT)?.to_string()),
        };
        let width = u32_of(v, "width").filter(|w| *w != 0)?;
        let height = u32_of(v, "height").filter(|h| *h != 0)?;
        let format = u32_or(v, "format", 0)?;
        let stride = u32_or(v, "stride", 0)?;
        let (textures, content, release) = if pixels.is_some() {
            // Rows that fit their pixels, in a format the compositor reads.
            let bpp = crate::frame_ring::bytes_per_pixel(format)?;
            if u64::from(stride) < u64::from(width) * u64::from(bpp) {
                return None;
            }
            (Vec::new(), 0, 0)
        } else {
            let textures: Option<Vec<u64>> = v.get("textures").as_arr().iter().map(Json::as_u64).collect();
            let textures = textures.filter(|t| t.len() == crate::frame_ring::SLOTS && t.iter().all(|h| *h != 0))?;
            (textures, v.get("content").as_u64().filter(|h| *h != 0)?, v.get("release").as_u64().filter(|h| *h != 0)?)
        };
        let m = SurfaceMsg {
            pid: u32_of(v, "pid")?,
            generation: u32_of(v, "generation").filter(|g| *g != 0)?,
            textures,
            content,
            release,
            width,
            height,
            format,
            api: text(v, "api").unwrap_or_default(),
            color_space: u32_or(v, "color_space", 0)?,
            pixels,
            stride,
        };
        Some(m)
    }
}

/// The hook's messages to the launcher.
#[derive(Clone, Debug, PartialEq)]
pub enum HookMsg {
    /// What the overlay shows (hook/src/ipc.rs `state`): the whole object, its `t` aside.
    State(Json),
    /// The game asked for an overlay page (`friends`, `invite`, `profile` with a peer id).
    Open { page: String, peer: Option<String> },
    /// A result to show the player.
    Note(Notice),
    /// A service's cloud saves stay on this PC, in `path`.
    LocalSaves { service: String, path: String },
    /// The game closed its overlay page.
    Close,
    /// A split-screen copy's game locked its cursor to its window (`ClipCursor`), or let
    /// it go: the session keeps the real mouse in its pane meanwhile.
    CursorLock { on: bool },
    /// A line for the launcher's log.
    Log(String),
    /// The game's frames: their size in pixels, its window (0: none known) and the
    /// graphics API drawing them (`d3d11`, `d3d12`, `vulkan`, `opengl`, ...).
    Viewport { width: u32, height: u32, hwnd: u64, api: String },
    /// One generation of the textures the game's frames are copied into (`frame_ring`):
    /// NT handles in process `pid` for the textures (one per slot), the content fence and
    /// the release fence, their size and format (a `DXGI_FORMAT`), and the graphics API
    /// they come from.
    Surface(SurfaceMsg),
}

impl HookMsg {
    pub fn to_json(&self) -> Json {
        match self {
            HookMsg::State(v) => {
                let mut pairs: Vec<(String, Json)> = vec![("t".into(), Json::str("state"))];
                pairs.extend(v.as_obj().iter().filter(|(k, _)| k != "t").cloned());
                Json::Obj(pairs)
            }
            HookMsg::Open { page, peer } => Json::obj(vec![("t", Json::str("open")), ("page", Json::str(page.as_str())), ("peer", opt_str(peer))]),
            HookMsg::Note(n) => n.to_json(),
            HookMsg::LocalSaves { service, path } => {
                Json::obj(vec![("t", Json::str("local_saves")), ("service", Json::str(service.as_str())), ("path", Json::str(path.as_str()))])
            }
            HookMsg::Close => Json::obj(vec![("t", Json::str("close"))]),
            HookMsg::CursorLock { on } => Json::obj(vec![("t", Json::str("cursor_lock")), ("on", Json::Bool(*on))]),
            HookMsg::Log(text) => Json::obj(vec![("t", Json::str("log")), ("text", Json::str(text.as_str()))]),
            HookMsg::Viewport { width, height, hwnd, api } => Json::obj(vec![
                ("t", Json::str("viewport")),
                ("width", Json::u64((*width).into())),
                ("height", Json::u64((*height).into())),
                ("hwnd", Json::u64(*hwnd)),
                ("api", Json::str(api.as_str())),
            ]),
            HookMsg::Surface(m) => Json::obj(vec![
                ("t", Json::str("surface")),
                ("pid", Json::u64(m.pid.into())),
                ("generation", Json::u64(m.generation.into())),
                ("textures", Json::Arr(m.textures.iter().map(|h| Json::u64(*h)).collect())),
                ("content", Json::u64(m.content)),
                ("release", Json::u64(m.release)),
                ("width", Json::u64(m.width.into())),
                ("height", Json::u64(m.height.into())),
                ("format", Json::u64(m.format.into())),
                ("api", Json::str(m.api.as_str())),
                ("color_space", Json::u64(m.color_space.into())),
                ("pixels", opt_str(&m.pixels)),
                ("stride", Json::u64(m.stride.into())),
            ]),
        }
    }

    /// A message, or None for one this build doesn't know or that lacks what it needs.
    pub fn from_json(v: &Json) -> Option<HookMsg> {
        Some(match msg_type(v)? {
            "state" => HookMsg::State(v.clone()),
            "open" => HookMsg::Open { page: text(v, "page").unwrap_or_else(|| "home".into()), peer: text(v, "peer") },
            "note" => HookMsg::Note(Notice::from_json(v)),
            "local_saves" => HookMsg::LocalSaves { service: text(v, "service")?, path: text(v, "path").unwrap_or_default() },
            "close" => HookMsg::Close,
            "cursor_lock" => HookMsg::CursorLock { on: v.get("on").as_bool()? },
            "log" => HookMsg::Log(text(v, "text")?),
            "viewport" => HookMsg::Viewport {
                width: u32_of(v, "width")?,
                height: u32_of(v, "height")?,
                hwnd: match v.get("hwnd") {
                    Json::Null => 0,
                    h => h.as_u64()?,
                },
                api: text(v, "api").unwrap_or_default(),
            },
            "surface" => HookMsg::Surface(SurfaceMsg::from_json(v)?),
            _ => return None,
        })
    }

    /// The message as one line, without its `\n`.
    pub fn to_line(&self) -> String {
        self.to_json().to_string()
    }

    pub fn from_line(line: &str) -> Option<HookMsg> {
        HookMsg::from_json(&Json::parse(line)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines come whole and without their ending; one longer than the limit, or cut
    /// off by the end, ends the reading.
    #[test]
    fn lines_are_capped() {
        let mut r = std::io::Cursor::new(b"{\"t\":1}\r\nabc\n0123456789\nlast".to_vec());
        let mut line = Vec::new();
        assert!(read_line_capped(&mut r, &mut line, 8));
        assert_eq!(line, b"{\"t\":1}");
        assert!(read_line_capped(&mut r, &mut line, 8));
        assert_eq!(line, b"abc");
        assert!(!read_line_capped(&mut r, &mut line, 8), "a 10-byte line under an 8-byte cap");
        let mut r = std::io::Cursor::new(b"last".to_vec());
        assert!(!read_line_capped(&mut r, &mut line, 8), "no newline at the end");
        let mut r = std::io::Cursor::new(b"12345678\n".to_vec());
        assert!(read_line_capped(&mut r, &mut line, 8), "exactly the cap");
        assert!(!read_line_capped(&mut r, &mut line, 8), "the end");
    }

    /// Each notice's English is what the hook says today (hook/src/invite.rs), so logs
    /// and the launcher's translations keep matching.
    #[test]
    fn notices_say_what_the_hook_said() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let cases = [
            (Notice::invite_sent(&names(&["Alex"])), "Invite sent", "To Alex".to_string()),
            (Notice::invite_sent(&names(&["Alex", "Sam", "Kim"])), "Invite sent", "To 3 players".into()),
            (Notice::nobody_to_invite(), "Nobody to invite", "No other player of this game is running on your network, or they're all with you already".into()),
            (Notice::already_together("Alex"), "Already playing together", "You're already in Alex's game".into()),
            (Notice::cant_join("Alex"), "Can't join", "Alex isn't in a game you can join yet".into()),
            (Notice::no_code_to_paste(), "No code to paste", "Copy the room code first, then try again".into()),
            (Notice::achievement("First steps"), "Achievement unlocked", "First steps".into()),
            (Notice::cant_invite(InviteBlock::HostFirst), "Can't invite", "Host or join a game first".into()),
            (
                Notice::join_asked(InviteBlock::LearnFirst),
                "A player asked to join you",
                "Invite someone once from the game's friends list first. After that, Invite and Join work here".into(),
            ),
            (Notice::text("Something", "happened"), "Something", "happened".into()),
        ];
        for (n, title, detail) in cases {
            assert_eq!((n.title().as_str(), n.detail()), (title, detail), "{n:?}");
            // And each comes back as it went.
            assert_eq!(HookMsg::from_line(&HookMsg::Note(n.clone()).to_line()), Some(HookMsg::Note(n.clone())));
        }
        let english = |b: InviteBlock| b.english();
        assert_eq!(
            english(InviteBlock::Official(names(&["Photon", "PlayFab"]))),
            "This game's online play goes through Photon and PlayFab's own servers, which PeerParty can't invite through: use the game's own invites or room codes, or set it to PeerParty's own in this game's page in PeerParty (Advanced, Online services)"
        );
        assert_eq!(
            english(InviteBlock::PhotonRoom("ROOM1".into())),
            "This game has no invites: players on your network join your session from the game's own menu (room ROOM1)"
        );
        assert_eq!(english(InviteBlock::LayersOff(names(&["eos", "galaxy"]))), "Turned off for this game in PeerParty: eos, galaxy");
        assert_eq!(
            english(InviteBlock::Unsupported(names(&["PlayFab Party", "Vivox"]))),
            "Not supported for this game yet: PeerParty can't hand it an invite through PlayFab Party or Vivox"
        );
        for b in [
            InviteBlock::Official(names(&["Photon"])),
            InviteBlock::PhotonRoom("R".into()),
            InviteBlock::LayersOff(names(&["steam"])),
            InviteBlock::Unsupported(names(&["Vivox"])),
            InviteBlock::LearnFirst,
            InviteBlock::HostFirst,
        ] {
            assert_eq!(InviteBlock::from_json(&b.to_json()), Some(b));
        }
        for c in NoticeCode::ALL {
            assert_eq!(NoticeCode::parse(c.as_str()), Some(c));
        }
    }

    /// A note from an older hook (no code), or with a code this build doesn't know or
    /// without its arguments, shows its English.
    #[test]
    fn notes_without_a_known_code_are_text() {
        let old = Json::parse(r#"{"t":"note","title":"Invite sent","detail":"To Alex"}"#).unwrap();
        assert_eq!(HookMsg::from_json(&old), Some(HookMsg::Note(Notice::text("Invite sent", "To Alex"))));
        let future = Json::parse(r#"{"t":"note","code":"from_the_future","title":"T","detail":"D"}"#).unwrap();
        assert_eq!(Notice::from_json(&future), Notice::text("T", "D"));
        let no_reason = Json::parse(r#"{"t":"note","code":"cant_invite","title":"Can't invite","detail":"why"}"#).unwrap();
        assert_eq!(Notice::from_json(&no_reason), Notice::text("Can't invite", "why"));
        let n = Notice::from_json(&Json::parse(r#"{"t":"note","code":"cant_invite","reason":{"code":"layers_off","items":["steam"]}}"#).unwrap());
        assert_eq!(n.code, NoticeCode::CantInvite);
        assert_eq!(n.detail(), "Turned off for this game in PeerParty: steam");
        // Lists from the wire are bounded.
        let many = format!(r#"{{"code":"layers_off","items":[{}]}}"#, vec!["\"x\""; 1000].join(","));
        assert!(matches!(InviteBlock::from_json(&Json::parse(&many).unwrap()), Some(InviteBlock::LayersOff(v)) if v.len() == MAX_ITEMS));
    }

    #[test]
    fn commands_round_trip() {
        let all = [
            HostMsg::Invite { to: None },
            HostMsg::Invite { to: Some("00aa".into()) },
            HostMsg::Accept { id: u64::MAX },
            HostMsg::Decline { id: 76561198012345679 },
            HostMsg::Paste,
            HostMsg::Copy { text: "ROOM1".into() },
            HostMsg::Join { peer: "00aa".into() },
            HostMsg::Block { peer: "00aa".into() },
            HostMsg::Unblock { peer: "00aa".into() },
            HostMsg::Mic { on: true },
            HostMsg::Mute { peer: "00aa".into(), on: false },
            HostMsg::Set { key: "invite_buttons".into(), value: "guide".into() },
            HostMsg::Overlay { visible: true, why: Some("Shift+Tab".into()), pads_held: false },
            HostMsg::Overlay { visible: false, why: None, pads_held: true },
            HostMsg::PadsHeld { on: false },
            HostMsg::Note { text: "overlay: hello".into() },
            HostMsg::Frame { name: r"Local\peerparty-frame-1-00".into() },
        ];
        for m in all {
            let line = m.to_line();
            assert!(!line.contains('\n'));
            assert_eq!(HostMsg::from_line(&line), Some(m), "{line}");
        }
        // What the launcher sends today reads the same.
        assert_eq!(HostMsg::from_line(r#"{"t":"invite","to":null}"#), Some(HostMsg::Invite { to: None }));
        assert_eq!(HostMsg::from_line(r#"{"t":"accept","id":12}"#), Some(HostMsg::Accept { id: 12 }));
        assert_eq!(HostMsg::from_line(r#"{"t":"accept","id":12.0}"#), Some(HostMsg::Accept { id: 12 }), "an older writer's float");
        assert_eq!(HostMsg::from_line(r#"{"t":"overlay","visible":true,"why":"the game"}"#), Some(HostMsg::Overlay { visible: true, why: Some("the game".into()), pads_held: false }));
        assert_eq!(HostMsg::from_line(r#"{"t":"accept"}"#), None, "no id");
        assert_eq!(HostMsg::from_line(r#"{"t":"nothing"}"#), None);
        assert_eq!(HostMsg::from_line("not json"), None);
    }

    #[test]
    fn hook_messages_round_trip() {
        let state = Json::parse(r#"{"t":"state","pid":4242,"me":{"id":"00aa","name":"Alex","slot":1},"pending":[{"id":18446744073709551615,"name":"Sam"}]}"#).unwrap();
        let m = HookMsg::from_json(&state).unwrap();
        let HookMsg::State(v) = &m else { panic!("{m:?}") };
        assert_eq!(v.get("pending").as_arr()[0].get("id").as_u64(), Some(u64::MAX));
        assert_eq!(HookMsg::from_line(&m.to_line()), Some(m.clone()));
        // A state built without its `t` gets one, first.
        let bare = HookMsg::State(Json::obj(vec![("pid", Json::u64(1))]));
        assert_eq!(bare.to_line(), r#"{"t":"state","pid":1}"#);
        for m in [
            HookMsg::Open { page: "profile".into(), peer: Some("00aa".into()) },
            HookMsg::Open { page: "friends".into(), peer: None },
            HookMsg::LocalSaves { service: "PlayFab".into(), path: r"C:\Users\Alex\AppData\Local\peerparty".into() },
            HookMsg::Close,
            HookMsg::CursorLock { on: true },
            HookMsg::Log("hello".into()),
        ] {
            assert_eq!(HookMsg::from_line(&m.to_line()), Some(m));
        }
        assert_eq!(HookMsg::from_line(r#"{"t":"open"}"#), Some(HookMsg::Open { page: "home".into(), peer: None }));
        assert_eq!(HookMsg::from_line(r#"{"t":"unknown"}"#), None);
    }

    fn surface() -> SurfaceMsg {
        SurfaceMsg { pid: 4242, generation: 3, textures: vec![0x2c4, 0x2c8, 0x2cc], content: 0x2d0, release: 0x2d4, width: 1920, height: 1080, format: 87, api: "d3d11".into(), color_space: 12, pixels: None, stride: 0 }
    }

    /// The split-screen messages: a copy's place, its input block and capture going down,
    /// its frames' viewport and surface coming up.
    #[test]
    fn session_messages_round_trip() {
        use crate::session::{InputAssignment, InstanceId, InstanceSpec, PaneRect};
        let mut spec = InstanceSpec::solo(InstanceId(1), 1);
        spec.pane = Some(PaneRect { x: 960, y: 0, width: 960, height: 1080 });
        spec.virtual_width = Some(960);
        spec.virtual_height = Some(1080);
        spec.focus_spoof = true;
        spec.compositor_capture = true;
        spec.input_assignment = InputAssignment::Controller { index: 1 };
        for m in [
            HostMsg::Instance(spec.clone()),
            HostMsg::Instance(InstanceSpec::solo(InstanceId(0), 0)),
            HostMsg::InputShm { name: r"Local\peerparty-input-1-00".into() },
            HostMsg::Capture { enabled: true, ring: Some(r"Local\peerparty-ring-1-00".into()), event: Some(r"Local\peerparty-frames-00".into()), pixels: false },
            HostMsg::Capture { enabled: true, ring: Some("file:/run/user/1000/peerparty/ring-1-00".into()), event: None, pixels: true },
            HostMsg::Capture { enabled: true, ring: None, event: None, pixels: false },
            HostMsg::Capture { enabled: false, ring: None, event: None, pixels: false },
            HostMsg::DevicesChanged,
        ] {
            let line = m.to_line();
            assert!(!line.contains('\n'));
            assert_eq!(HostMsg::from_line(&line), Some(m), "{line}");
        }
        assert_eq!(
            HostMsg::Capture { enabled: false, ring: None, event: None, pixels: false }.to_line(),
            r#"{"t":"capture","enabled":false,"ring":null,"event":null,"pixels":false}"#
        );
        assert_eq!(HostMsg::InputShm { name: "x".into() }.to_line(), r#"{"t":"input_shm","name":"x"}"#);
        // The least a launcher writes, and what lacks what it needs.
        assert_eq!(HostMsg::from_line(r#"{"t":"instance","spec":{"id":2,"slot":1}}"#), Some(HostMsg::Instance(InstanceSpec::solo(InstanceId(2), 1))));
        assert_eq!(HostMsg::from_line(r#"{"t":"capture"}"#), Some(HostMsg::Capture { enabled: false, ring: None, event: None, pixels: false }));
        for bad in [
            r#"{"t":"instance"}"#,
            r#"{"t":"instance","spec":null}"#,
            r#"{"t":"instance","spec":{"slot":0}}"#,
            r#"{"t":"instance","spec":{"id":1,"slot":0,"pane":{"x":0}}}"#,
            r#"{"t":"instance","id":1,"slot":0}"#,
            r#"{"t":"input_shm"}"#,
            r#"{"t":"input_shm","name":7}"#,
        ] {
            assert_eq!(HostMsg::from_line(bad), None, "{bad}");
        }

        for m in [
            HookMsg::Viewport { width: 3840, height: 2160, hwnd: u64::MAX, api: "d3d12".into() },
            HookMsg::Viewport { width: 1, height: 1, hwnd: 0, api: String::new() },
            HookMsg::Surface(surface()),
            HookMsg::Surface(SurfaceMsg { textures: vec![u64::MAX; 3], width: u32::MAX, format: 0, api: String::new(), color_space: 0, ..surface() }),
            HookMsg::Surface(SurfaceMsg {
                textures: Vec::new(),
                content: 0,
                release: 0,
                format: 87,
                pixels: Some("/run/user/1000/peerparty/pixels-4242-3".into()),
                stride: 1920 * 4,
                ..surface()
            }),
        ] {
            let line = m.to_line();
            assert_eq!(HookMsg::from_line(&line), Some(m), "{line}");
        }
        assert_eq!(
            HookMsg::Viewport { width: 1920, height: 1080, hwnd: 0x10a2e, api: "vulkan".into() }.to_line(),
            r#"{"t":"viewport","width":1920,"height":1080,"hwnd":68142,"api":"vulkan"}"#
        );
        // What an older or plainer hook leaves out has its default.
        assert_eq!(HookMsg::from_line(r#"{"t":"viewport","width":800,"height":600}"#), Some(HookMsg::Viewport { width: 800, height: 600, hwnd: 0, api: String::new() }));
        assert_eq!(
            HookMsg::from_line(r#"{"t":"surface","pid":9,"generation":1,"textures":[4,8,12],"content":16,"release":20,"width":2,"height":3}"#),
            Some(HookMsg::Surface(SurfaceMsg { pid: 9, generation: 1, textures: vec![4, 8, 12], content: 16, release: 20, width: 2, height: 3, format: 0, api: String::new(), color_space: 0, pixels: None, stride: 0 }))
        );
        for bad in [
            r#"{"t":"viewport"}"#,
            r#"{"t":"viewport","width":800}"#,
            r#"{"t":"viewport","width":-1,"height":600}"#,
            r#"{"t":"viewport","width":4294967296,"height":600}"#,
            r#"{"t":"viewport","width":800,"height":600,"hwnd":-5}"#,
            r#"{"t":"viewport","width":800,"height":600,"hwnd":"0x10"}"#,
            r#"{"t":"viewport","width":800.5,"height":600}"#,
            r#"{"t":"surface","width":2,"height":3}"#,
            r#"{"t":"surface","pid":9,"generation":1,"textures":[4,8],"content":16,"release":20,"width":2,"height":3}"#,
            r#"{"t":"surface","pid":9,"generation":1,"textures":[4,8,0],"content":16,"release":20,"width":2,"height":3}"#,
            r#"{"t":"surface","pid":9,"generation":1,"textures":[4,8,-12],"content":16,"release":20,"width":2,"height":3}"#,
            r#"{"t":"surface","pid":9,"generation":0,"textures":[4,8,12],"content":16,"release":20,"width":2,"height":3}"#,
            r#"{"t":"surface","pid":9,"generation":1,"textures":[4,8,12],"release":20,"width":2,"height":3}"#,
            r#"{"t":"surface","pid":9,"generation":1,"textures":[4,8,12],"content":16,"release":20,"width":0,"height":3}"#,
            r#"{"t":"surface","pid":9,"generation":1,"textures":[4,8,12],"content":16,"release":20,"width":2,"height":3,"format":"bgra"}"#,
            r#"{"t":"surface","generation":1,"textures":[4,8,12],"content":16,"release":20,"width":2,"height":3}"#,
            r#"{"t":"surface","pid":9,"generation":1,"textures":[4,8,12],"content":16,"release":20,"width":2,"height":3,"color_space":-1}"#,
            // Pixels: rows too short for their width, a format the compositor can't read, no path.
            r#"{"t":"surface","pid":9,"generation":1,"pixels":"/p","width":2,"height":3,"format":87,"stride":7}"#,
            r#"{"t":"surface","pid":9,"generation":1,"pixels":"/p","width":2,"height":3,"format":2,"stride":64}"#,
            r#"{"t":"surface","pid":9,"generation":1,"pixels":"","width":2,"height":3,"format":87,"stride":8}"#,
            r#"{"t":"surface","pid":9,"generation":1,"pixels":7,"width":2,"height":3,"format":87,"stride":8}"#,
        ] {

            assert_eq!(HookMsg::from_line(bad), None, "{bad}");
        }
    }
}
