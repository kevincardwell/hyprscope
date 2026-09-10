//! Talking to a running Hyprland: the request socket (`.socket.sock`) and the
//! event socket (`.socket2.sock`).

pub mod ipc;
pub mod model;

pub use ipc::{Event, Hypr};
pub use model::{Client, Monitor, Workspace};
