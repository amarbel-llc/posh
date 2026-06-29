//! posh-proto: the shared frame/display protocol layer, extracted from `posh`'s
//! `remote` module so both the `posh` binary and the `poshterity` recorder can
//! drive it (github #75). `posh` depends on `poshterity`, so these types could
//! not live in `posh` without a dependency cycle.
//!
//! What lives here is exactly the seam a deterministic remote test needs:
//!   * [`display`] — the `Snapshot` screen model and `new_frame` diff renderer
//!     (mosh terminaldisplay), plus the connection-status banner;
//!   * [`frame`] — the `ServerFrame`/`FrameBody` wire types, their encode/decode,
//!     the prefix/suffix byte diff, and the base checksum;
//!   * [`framesync`] — the swappable encode/apply codecs (`DumpDiff`,
//!     `MorphDelta`) behind the `FrameEncoder`/`FrameApplier` traits;
//!   * [`caps`] — the RFC 0001 capability table;
//!   * [`channel`] — the [`FrameChannel`] seam a harness binds a server-side
//!     encoder to a client-side applier through.
//!
//! The transport machinery that is NOT shared — datagram fragmentation, the
//! reliable input/echo/agent streams, `ClientMessage` — stays in `posh`'s
//! `remote::sync`, which re-imports the frame types from here.
#![forbid(unsafe_code)]

pub mod caps;
pub mod channel;
pub mod display;
pub mod error;
pub mod frame;
pub mod framesync;

pub use channel::{ClientAck, FrameChannel};
pub use error::{Error, Result};
