//! Roaming remote terminal over encrypted UDP (mosh port, simplified state
//! sync: frames are complete dump_vt screen states, optionally diffed).

pub mod agent;
/// Re-export shim: the RFC 0001 capability table moved to posh-proto (github
/// #75) so poshterity can share it; existing `crate::remote::caps::…` paths
/// keep resolving through here.
pub mod caps {
    pub use posh_proto::caps::*;
}
pub mod client;
pub mod crypto;
pub mod datagram;
pub mod diag;
/// Re-export shim: the `Snapshot` model and `new_frame` renderer moved to
/// posh-proto (github #75). `open`/`close` stay here because they inject this
/// crate's terminfo smcup/rmcup pair into posh-proto's param-based
/// `open_with`/`close_with`.
pub mod display {
    pub use posh_proto::display::*;

    /// Takes over the outer terminal on startup (mosh Display::open's smcup),
    /// injecting the terminfo ca_mode bracket for `$TERM`.
    pub fn open() -> Vec<u8> {
        posh_proto::display::open_with(&crate::terminfo::ca_mode_bracket())
    }

    /// Restores the outer terminal on exit (mosh Display::close's rmcup).
    pub fn close() -> Vec<u8> {
        posh_proto::display::close_with(&crate::terminfo::ca_mode_bracket())
    }
}
/// Re-export shim: the swappable frame-sync codecs (DumpDiff/MorphDelta) and
/// their FrameEncoder/FrameApplier traits moved to posh-proto (github #75).
pub mod framesync {
    pub use posh_proto::framesync::*;
}
pub mod hostmetrics;
pub mod palette;
#[cfg(test)]
mod perf_probe;
pub mod predict;
/// Shared scrollback scroll-view machinery (FDR 0005): the wheel-intercept
/// `MouseFilter`, the scroll-offset math, and the frozen-history compose. Used
/// by both this crate's roaming client (`remote::client`) and the local session
/// frame client (`session::client`) so the two share one implementation.
pub mod scrollview;
pub mod server;
pub mod sshwrap;
pub mod stats;
pub mod sync;
