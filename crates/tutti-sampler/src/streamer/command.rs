//! Streamer thread command enum and the stream's identity.

use std::sync::Arc;

use tutti_buffers::Wave;
use tutti_platform::Asset;

use tutti_buffers::ChannelLayout;
use tutti_rt::RingFloat;

use super::open::Opened;
use crate::OpenError;
use tutti_rt::OneshotSender;

tutti_rt::id! {
    /// One open stream's identity: minted on the control side when the open is
    /// asked for ([`DiskStreamer::open`](crate::DiskStreamer::open)), never
    /// reused, and what every command about the stream names. A host is
    /// handed one ([`Stream::id`](crate::Stream::id),
    /// [`StreamReader::id`](crate::StreamReader::id)); a number it makes up
    /// names no open stream, which the streamer ignores.
    pub struct StreamId;
}

/// Where the streamer answers an open: one cell, written once by the streamer
/// and read once by [`PendingOpen::poll`](crate::PendingOpen::poll).
pub(crate) type Reply<F> = OneshotSender<Result<Opened<F>, OpenError>>;

/// What the decode thread made of a file: the whole wave, or why there is none.
pub(crate) struct Decoded<F: RingFloat>(pub(crate) Result<Arc<Wave<F>>, OpenError>);

impl<F: RingFloat> std::fmt::Debug for Decoded<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Ok(wave) => write!(f, "Decoded({} frames)", wave.len()),
            Err(error) => write!(f, "Decoded({error})"),
        }
    }
}

/// Command sent to the Streamer thread.
#[derive(Debug)]
pub(crate) enum StreamerCommand<F: RingFloat> {
    /// Open `asset` as stream `id`, its window starting at frame `start`,
    /// and answer on `reply`.
    Open {
        id: StreamId,
        asset: Asset,
        start: u64,
        /// The voice's width, or the file's own when `None`.
        width: Option<ChannelLayout>,
        reply: Reply<F>,
    },
    /// The decode thread finished the whole-file decode an open of a file that
    /// cannot seek waits on: the streamer builds the stream's ring from it and
    /// answers the open. Sent by the decode thread; an
    /// id the streamer holds no pending open for is ignored.
    #[cfg_attr(
        not(codec),
        allow(dead_code, reason = "sent only by the codec-gated decode lane")
    )]
    Decoded { id: StreamId, wave: Decoded<F> },
    /// End a stream: its record, and with it its ring and region, goes in the
    /// step that handles this. Sent by `Drop for Stream`. A stream the
    /// streamer no longer holds ignores it.
    Close { stream: StreamId },

    /// The session rate moved (`DiskStreamer::set_sample_rate` stored it before
    /// sending this): re-derive every open stream's conversion ratio against
    /// the rate as it now is.
    SessionRate,
}
