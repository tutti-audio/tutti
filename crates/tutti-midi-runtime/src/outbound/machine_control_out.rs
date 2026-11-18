//! [`MachineControlSender`]: the MIDI Machine Control commands that make a
//! device follow this transport (RP-013 pp.20 and 28), from what the transport
//! did between two looks at it.
//!
//! The sender holds no clock and sends nothing: each [`observe`](MachineControlSender::observe)
//! takes the transport as it is at one frame and returns the message the
//! change since the last look calls for, held inline ([`MmcPayload`]), so a look
//! allocates nothing and can run on the audio thread.
//!
//! - A transport that started sends **PLAY**; one that stopped, **STOP**.
//! - A position that is not where rolling or standing still would have left it
//!   is a locate: **LOCATE (target)** to its time code, and while the transport
//!   rolls (or starts rolling in the same look) **DEFERRED PLAY** after it, so
//!   the device plays once it has got there.
//! - The first look sends nothing: there is no change to report.
//!
//! A stop that also moved the position sends STOP alone. MMC carries no time of
//! its own: the message belongs on the frame the look was made at.
//! [`SyncOut`](crate::SyncOut) looks once per transport segment of a block, so a
//! change on frame `N` of a block is a message on frame `N`.

use tutti_midi_protocol::{MmcCommands, MmcPayload, MmcPosition, MtcRate, Recipient};
use tutti_units::{Frame, SampleRate, Seconds};

/// Why a look at the transport could not be sent as Machine Control: the
/// transport moved to a time no time code names (before zero, NaN, or a day
/// or more). The look is taken (the next one compares against it), and nothing
/// is sent.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("{0:?} has no time code")]
pub struct NoTimeCode(pub Seconds);

/// What the transport was at a look.
#[derive(Clone, Copy, Debug)]
struct Seen {
    playing: bool,
    seconds: Seconds,
    frame: Frame,
}

/// Turns the transport's changes into Machine Control commands.
///
/// ```
/// use tutti_midi_protocol::{MtcRate, Recipient};
/// use tutti_midi_runtime::MachineControlSender;
/// use tutti_units::{Frame, SampleRate, Seconds};
///
/// let mut sender = MachineControlSender::new(Recipient::AllCall, MtcRate::Fps30);
/// let rate = SampleRate::SR_48K;
/// assert_eq!(sender.observe(false, Seconds::new(0.0), Frame::new(0), rate), Ok(None));
/// let started = sender.observe(true, Seconds::new(0.0), Frame::new(512), rate).unwrap().unwrap();
/// assert_eq!(started, [0x7F, 0x7F, 0x06, 0x02]);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct MachineControlSender {
    to: Recipient,
    rate: MtcRate,
    last: Option<Seen>,
}

impl MachineControlSender {
    /// How far a position may be from where it should be before it is a locate:
    /// a millisecond (tutti's choice: it is far over the rounding of a
    /// position, and far under a frame).
    pub const LOCATE_TOLERANCE: Seconds = Seconds::new(0.001);

    /// A sender to `to` that states time codes at `rate`.
    #[must_use]
    pub fn new(to: Recipient, rate: MtcRate) -> Self {
        Self {
            to,
            rate,
            last: None,
        }
    }

    /// Sends to `to` and states time codes at `rate` from the next look on,
    /// keeping what the last look saw: a change of either is no transport
    /// change.
    pub(crate) fn retarget(&mut self, to: Recipient, rate: MtcRate) {
        self.to = to;
        self.rate = rate;
    }

    /// Forgets the last look: the next one compares against nothing and
    /// sends nothing, as the first of a new sender.
    pub(crate) fn reset(&mut self) {
        self.last = None;
    }

    /// Where the commands go.
    #[must_use]
    pub fn to(&self) -> Recipient {
        self.to
    }

    /// The frame rate time codes are stated at.
    #[must_use]
    pub fn rate(&self) -> MtcRate {
        self.rate
    }

    /// Looks at the transport: whether it `playing`, its position in `seconds`
    /// and the graph `frame` count the look was made at, on a graph at `rate`.
    /// The payload of the message the change since the last look calls for, if
    /// any, to be sent on `frame`: pack it with `Sysex7Packets::from_payload`.
    /// Allocates nothing.
    ///
    /// # Errors
    ///
    /// [`NoTimeCode`] when the transport moved to a time no time code names
    /// (`MmcPosition::from_seconds` is `None`: before zero, NaN, a day or
    /// more); nothing is sent, and [`SyncOut`](crate::SyncOut) counts it refused.
    pub fn observe(
        &mut self,
        playing: bool,
        seconds: Seconds,
        frame: Frame,
        rate: SampleRate,
    ) -> Result<Option<MmcPayload>, NoTimeCode> {
        let now = Seen {
            playing,
            seconds,
            frame,
        };
        let Some(before) = self.last.replace(now) else {
            return Ok(None);
        };
        if before.playing && !playing {
            return Ok(Some(MmcCommands::stop().payload(self.to)));
        }
        let expected = if before.playing {
            before.seconds
                + Frame::new(frame.get().saturating_sub(before.frame.get())).to_seconds(rate)
        } else {
            before.seconds
        };
        let moved = (now.seconds - expected).get().abs() > Self::LOCATE_TOLERANCE.get();
        if moved {
            let target =
                MmcPosition::from_seconds(now.seconds, self.rate).ok_or(NoTimeCode(now.seconds))?;
            Ok(Some(MmcCommands::locate(target, playing).payload(self.to)))
        } else if playing && !before.playing {
            Ok(Some(MmcCommands::play().payload(self.to)))
        } else {
            Ok(None)
        }
    }
}
