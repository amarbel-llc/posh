//! The binding seam between server-side frame production and client-side
//! application (github #75). Production wires a server's `ServerFrame` stream to
//! a client over the UDP `Connection`; a deterministic test instead implements
//! [`FrameChannel`] as an in-memory queue it controls (delivery order, loss,
//! ack timing), so the same codecs round-trip without a socket, a PTY, or a
//! clock. The `poshterity` harness owns the in-memory implementation; this crate
//! only defines the seam.

use crate::frame::ServerFrame;

/// What the client sends back upstream after applying (or rejecting) a frame:
/// the frame number it is now anchored at. The server advances its acked
/// baseline to this. A minimal projection of `posh`'s `ClientMessage` — the
/// only field the frame round-trip needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientAck {
    pub acked_frame: u64,
}

/// The seam a harness binds a server-side [`FrameEncoder`] to a client-side
/// [`FrameApplier`] through. Production's `Connection` and a test's in-memory
/// queue are interchangeable implementations.
///
/// [`FrameEncoder`]: crate::framesync::FrameEncoder
/// [`FrameApplier`]: crate::framesync::FrameApplier
pub trait FrameChannel {
    /// Enqueue a freshly produced frame toward the client.
    fn send_frame(&mut self, frame: ServerFrame);
    /// Take the next ack the client has made available, if any.
    fn recv_ack(&mut self) -> Option<ClientAck>;
}
