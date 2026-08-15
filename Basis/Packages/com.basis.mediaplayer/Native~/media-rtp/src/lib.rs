#![forbid(unsafe_code)]

//! Sans-IO RTP receive substrate (§6.13): sequence unwrap, bounded
//! reorder, RFC 3550 jitter/loss accounting, sender-report tracking and
//! receiver-report composition. Shared by the RTSP-UDP lane today and
//! the WHEP lane later, so nothing here knows about sockets, retina or
//! any transport: the caller feeds datagrams and an explicit `now`
//! (monotonic microseconds as [`MediaTime`]) and polls for in-order
//! packets, outbound RTCP and the next wake deadline.
//!
//! One [`RtpReceiver`] per RTP stream (one media sender per stream —
//! the RTSP and WHEP shapes). The remote source is pinned to the first
//! RTP packet's SSRC, not to any advertised value: advertised SSRCs are
//! placeholder-prone (all-zero on some edges) while the stream itself
//! is authoritative.

mod receiver;
mod reports;

pub use receiver::{
    OrderedPacket, PacketRejected, ReceiverConfig, ReceiverStats, RtpFields, RtpReceiver,
};
pub use reports::SenderInfo;
