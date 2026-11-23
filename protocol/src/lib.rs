//! What the launcher (peerparty.exe), the hook in the game and the LAN core agree on,
//! in one crate both sides build from:
//!
//! - `env`: the `PEERPARTY_*` variables and how their values read.
//! - `catalog`: the online services, PeerParty's layers, the hook's front ends and the
//!   anti-cheat file names.
//! - `json`: the JSON the pipe and the services' REST bodies use, 64-bit ids exact.
//! - `ipc`: the messages on the pipe, notices with codes, and line framing.
//! - `shm`: the overlay frame block's header and its sequence lock.
//! - `frame_ring`: which of a split-screen copy's shared frame textures the compositor
//!   may draw and which the game's hook may write.
//! - `input_shm`: one copy's virtual input block (controller, keyboard, mouse, wheel),
//!   under the same kind of sequence lock.
//! - `transport`: where the control connection and the frame block are (named pipes and
//!   mappings, or loopback TCP with a token and a file), as environment strings.
//! - `session`: copies of a game sharing one screen.
//!
//! No dependencies and no OS calls: it builds for the x64 and x86 hooks and for Linux
//! alike.

pub mod catalog;
pub mod env;
pub mod frame_ring;
pub mod input_shm;
pub mod ipc;
pub mod json;
pub mod session;
pub mod shm;
pub mod transport;

pub use catalog::{ANTICHEAT_PATTERNS, FRONTS, LAYERS, LayerEntry, SDKS, SdkEntry, is_anticheat_name};
pub use env::{SdkMode, env_bool, parse_bool, parse_list, parse_mode};
pub use input_shm::{InputHeader, InputMode, InputSnapshot, VirtualKeyboard, VirtualMouse, VirtualPad, VirtualWheel};
pub use ipc::{HookMsg, HostMsg, InviteBlock, Notice, NoticeCode, SurfaceMsg, read_line_capped};
pub use json::Json;
pub use session::{InputAssignment, InstanceId, InstanceSpec, LayoutPreset, PaneRect};
pub use shm::FrameHeader;
pub use transport::{IpcEndpoint, ShmBacking};
