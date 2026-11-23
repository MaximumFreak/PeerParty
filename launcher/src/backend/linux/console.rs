//! The first things peerparty does on Windows (DLL search order, the parent's console)
//! have nothing to do on Unix: a program finds its libraries the system's way and
//! writes to the terminal it was started from.

pub fn only_system_dlls() {}

pub fn attach_console() {}
