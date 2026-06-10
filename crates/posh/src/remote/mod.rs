//! Roaming remote terminal over encrypted UDP (mosh port, simplified state
//! sync: frames are complete dump_vt screen states, optionally diffed).

pub mod client;
pub mod crypto;
pub mod datagram;
pub mod server;
pub mod sshwrap;
pub mod sync;
