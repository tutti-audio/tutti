//! [`MachineControlFollower`]: what a transport that is controlled by MIDI
//! Machine Control does, as a pure state machine.
//!
//! One follower reads one source's UMPs and says what the commands addressed to
//! this device ask of a transport ([`MachineControlEvent`]). It holds no clock
//! and no transport; whoever owns the transport places each event (the session,
//! in `tutti-session`).
//!
//! # Addressing
//!
//! A command is read when its device ID is the all-call (`7F`), this device's
//! own ID, or a group this device has been assigned to by GROUP (RP-013 pp.5 and
//! 39). MMC RESET clears the groups (RP-013 p.25).
//!
//! # What is done
//!
//! - **STOP** and **PAUSE** stop the transport.
//! - **PLAY** and **DEFERRED PLAY** play. A locate is instant here, so a
//!   locate in progress never delays a deferred play.
//! - **LOCATE (target)** locates to the time; **LOCATE (register)** to the time a WRITE
//!   put in the general purpose register it names (`08` to `0F`). A time is
//!   read from `00:00:00:00` with drop-frame labels counted as SMPTE counts them
//!   and the subframes added.
//! - **MOVE** between general purpose registers copies the time.
//! - **VARIABLE PLAY** and **DEFERRED VARIABLE PLAY** at exactly play speed play.
//! - **GROUP** and **MMC RESET** act on the follower itself.
//! - **COMMAND SEGMENT** strings are reassembled (RP-013 p.40) and their
//!   commands acted on as if they had arrived in one message. A segment that
//!   does not belong is [`MachineControlEvent::Segmentation`], which a
//!   device that answers sends as a COMMAND ERROR (`09`); the string is
//!   cancelled. A message to this device that is not a segment cancels a
//!   string in progress, WAIT and RESUME alone excepted (p.40). MMC RESET
//!   cancels one (p.25).
//! - WAIT and RESUME are handshakes for a device that answers; this one
//!   does not, and they change nothing.
//!
//! # What is read and not applied
//!
//! Every other command is reported as [`MachineControlEvent::Ignored`], and the
//! reason is in what the transport is: it plays at one speed, locates
//! instantly and holds no record, no tape and no master time code.
//!
//! - **FAST FORWARD**, **REWIND**, **SHUTTLE** and **SEARCH** move at a speed other than play speed, which a transport that plays
//!   at one speed does not have.
//! - **STEP** moves by STEP LENGTH units, a video field by default: a
//!   distance in a time code frame rate that a follower, which holds none,
//!   cannot turn into time.
//! - **CHASE** (p.24) follows the SELECTED MASTER CODE, a time code stream: a
//!   follower sees no time code stream, only commands. MTC follows through
//!   `MtcDecoder`.
//! - **RECORD STROBE**, **RECORD EXIT**, **RECORD PAUSE** and **EJECT**: the
//!   transport records nothing and holds no medium.
//! - **COMMAND ERROR RESET** clears an error field that a follower, which sends
//!   no COMMAND ERROR, does not keep.
//! - READ and UPDATE ask for responses, and the follower answers none.

use tutti_midi_protocol::{
    Addressed, MmcCommand, MmcCommandReassembler, MmcCommands, MmcFields, MmcGroup, MmcInfoField,
    MmcLocate, MmcMove, MmcPosition, MmcSegment, MmcSegmentError, MmcSegmentPlace, MmcSign,
    MmcSpeed, Recipient, SysexDeviceId, Universal, UniversalRead, read,
};
use tutti_ump::{SysexAssembler, SysexStep, Ump};
use tutti_units::Seconds;

/// What a Machine Control source asks of the transport.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum MachineControlEvent {
    /// Roll from where the transport stands.
    Play,
    /// Stop where the transport stands.
    Stop,
    /// Jump to `time` since `00:00:00:00`, keeping whatever the transport does
    /// after the locate.
    Locate(Seconds),
    /// A command this follower does not act on.
    Ignored(MmcCommand),
    /// A COMMAND SEGMENT that did not belong, or a string that did not make
    /// whole commands (RP-013 p.40): the string in progress is cancelled.
    Segmentation(SegmentFault),
}

/// Why a COMMAND SEGMENT was not taken in (RP-013 p.40): what a device that
/// answers sends as a COMMAND ERROR Segmentation Error (`09`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum SegmentFault {
    /// The segment does not belong to its string, or the string is not whole
    /// commands (p.40 c and d).
    #[error(transparent)]
    String(#[from] MmcSegmentError),
    /// A COMMAND SEGMENT came after another command in its message (p.40 a).
    #[error("a segment is not the first command of its message")]
    NotFirst,
    /// A COMMAND SEGMENT's byte count is not the number of bytes remaining in
    /// its message (p.40 b): more commands follow it, or the message ends
    /// before its count does.
    #[error("a segment's byte count is not the bytes remaining in its message")]
    CountMismatch,
    /// A COMMAND SEGMENT inside a reassembled string: a segment must be the
    /// only command of its message (p.40).
    #[error("a segment inside a segmented string")]
    Nested,
}

/// Longest message read: 3 header bytes and 48 of data.
const SYSEX_MAX: usize = 56;
const SYSEX_ROOM: usize = 2 * SYSEX_MAX + 8;

/// The seconds a time code position is at: the frames since `00:00:00:00`
/// and the subframes' hundredths of the next.
fn seconds_of(position: MmcPosition) -> Seconds {
    let timecode = position.label.time_code().timecode();
    let subframes = f64::from(position.subframes.get()) / 100.0;
    let (num, den) = timecode.frame_rate().ratio();
    let seconds = timecode.to_seconds().get() + subframes * den as f64 / num as f64;
    Seconds::new(match position.label.sign() {
        MmcSign::Positive => seconds,
        MmcSign::Negative => -seconds,
    })
}

/// Follows Machine Control commands; the rules are in the module docs.
///
/// ```
/// use tutti_midi_runtime::{MachineControlEvent, MachineControlFollower};
/// use tutti_midi_protocol::SysexDeviceId;
/// use tutti_ump::{MidiGroup, Sysex7Packets};
///
/// let mut follower = MachineControlFollower::new(SysexDeviceId::of(1));
/// // F0 7F 01 06 02 F7: PLAY to device 1.
/// let mut heard = Vec::new();
/// for packet in Sysex7Packets::from_payload(MidiGroup::FIRST, &[0x7F, 0x01, 0x06, 0x02]).unwrap() {
///     follower.step(packet, &mut |e| heard.push(e));
/// }
/// assert_eq!(heard, [MachineControlEvent::Play]);
/// ```
#[derive(Debug)]
pub struct MachineControlFollower {
    state: State,
    sysex: SysexAssembler<[u8; SYSEX_ROOM]>,
    /// The COMMAND SEGMENT string in progress.
    segments: MmcCommandReassembler,
}

/// What GROUP and WRITE set: the part of the follower that commands act on.
#[derive(Debug)]
struct State {
    /// The device ID the follower answers to.
    device: SysexDeviceId,
    /// The groups this device is assigned to, a bit a device ID.
    groups: u128,
    /// The general purpose registers `08` to `0F`.
    registers: [Option<MmcPosition>; 8],
}

impl MachineControlFollower {
    /// A follower that answers to `device`, and to the all-call, in no group.
    #[must_use]
    pub fn new(device: SysexDeviceId) -> Self {
        Self {
            state: State {
                device,
                groups: 0,
                registers: [None; 8],
            },
            sysex: SysexAssembler::new([0; SYSEX_ROOM], SYSEX_MAX, 1),
            segments: MmcCommandReassembler::new(),
        }
    }

    /// The device ID the follower answers to besides the all-call.
    #[must_use]
    pub fn device(&self) -> SysexDeviceId {
        self.state.device
    }

    /// Whether the follower is in `group`.
    #[must_use]
    pub fn in_group(&self, group: SysexDeviceId) -> bool {
        self.state.groups >> group.get() & 1 != 0
    }

    /// Whether a command to `to` is for this device: the all-call, its own ID
    /// or one of its groups.
    fn is_for_us(&self, to: Recipient) -> bool {
        match to {
            Recipient::AllCall => true,
            Recipient::Device(id) => id == self.state.device || self.in_group(id),
        }
    }

    /// Forgets the groups and registers, as MMC RESET does.
    pub fn reset(&mut self) {
        self.state.reset();
        self.segments.cancel();
        self.sysex.reset();
    }

    /// Takes in one UMP of the source's stream and passes each event it makes
    /// to `out`, in order. Allocates nothing itself.
    ///
    /// A message that completes a segmented string makes an event for each of
    /// its commands: up to [`MMC_MAX_SEGMENTED`](tutti_midi_protocol::MMC_MAX_SEGMENTED) of them (2880 one-byte
    /// commands) in one call. `out` runs on the calling thread, and a follower
    /// is driven from a control thread (never a graph node), so a collector that
    /// grows a `Vec` may allocate.
    pub fn step(&mut self, ump: Ump, out: &mut impl FnMut(MachineControlEvent)) {
        self.sysex.begin_block();
        let SysexStep::Complete(done) = self.sysex.push(ump) else {
            return;
        };
        match read(self.sysex.message(done)) {
            UniversalRead::Universal(Addressed {
                to,
                message: Universal::MachineControl(commands),
            }) if self.is_for_us(to) => self.segmented_or_plain(&commands, out),
            // The segment is cut short (p.40 b): the commands before it are not
            // acted on, as for any segment that is not first.
            UniversalRead::MachineControlTruncated { to, before } if self.is_for_us(to) => {
                self.segments.cancel();
                out(MachineControlEvent::Segmentation(if before.is_empty() {
                    SegmentFault::CountMismatch
                } else {
                    SegmentFault::NotFirst
                }));
            }
            _ => {}
        }
    }

    /// The commands of one message to this device: a COMMAND SEGMENT goes to
    /// the string in progress, anything else is acted on, and cancels a string
    /// unless it is only WAIT and RESUME (RP-013 p.40).
    fn segmented_or_plain(
        &mut self,
        commands: &MmcCommands,
        out: &mut impl FnMut(MachineControlEvent),
    ) {
        let mut items = commands.iter();
        let Some(first) = items.next() else {
            return;
        };
        if let MmcCommand::CommandSegment(segment) = first {
            if items.next().is_some() {
                self.segments.cancel();
                out(MachineControlEvent::Segmentation(
                    SegmentFault::CountMismatch,
                ));
                return;
            }
            self.segment(segment, out);
            return;
        }
        if items.any(|c| matches!(c, MmcCommand::CommandSegment(_))) {
            self.segments.cancel();
            out(MachineControlEvent::Segmentation(SegmentFault::NotFirst));
            return;
        }
        if commands
            .iter()
            .any(|c| !matches!(c, MmcCommand::Wait | MmcCommand::Resume))
        {
            self.segments.cancel();
        }
        for command in commands {
            self.state.act(command, out);
        }
    }
}

impl MachineControlFollower {
    /// Takes one COMMAND SEGMENT in. A first segment inside a string is
    /// reported, and then starts its own string. A finished string's commands
    /// are acted on in order; a segment inside it is reported, and skipped.
    fn segment(&mut self, segment: MmcSegment, out: &mut impl FnMut(MachineControlEvent)) {
        if segment.place() == MmcSegmentPlace::First && self.segments.in_progress() {
            // The reassembler refuses a first inside a string, and takes it in
            // again once the string is cancelled.
            let refused = self.segments.push(segment).err();
            debug_assert_eq!(refused, Some(MmcSegmentError::FirstInsideString));
            out(MachineControlEvent::Segmentation(
                MmcSegmentError::FirstInsideString.into(),
            ));
        }
        match self.segments.push(segment) {
            Ok(Some(string)) => {
                for command in string {
                    if matches!(command, MmcCommand::CommandSegment(_)) {
                        out(MachineControlEvent::Segmentation(SegmentFault::Nested));
                    } else {
                        self.state.act(command, out);
                    }
                }
            }
            Ok(None) => {}
            Err(error) => out(MachineControlEvent::Segmentation(error.into())),
        }
    }
}

impl State {
    fn reset(&mut self) {
        self.groups = 0;
        self.registers = [None; 8];
    }

    fn act(&mut self, command: MmcCommand, out: &mut impl FnMut(MachineControlEvent)) {
        match command {
            MmcCommand::Stop | MmcCommand::Pause => out(MachineControlEvent::Stop),
            MmcCommand::Play | MmcCommand::DeferredPlay => out(MachineControlEvent::Play),
            MmcCommand::VariablePlay(speed) | MmcCommand::DeferredVariablePlay(speed)
                if speed == MmcSpeed::PLAY =>
            {
                out(MachineControlEvent::Play);
            }
            MmcCommand::Locate(MmcLocate::Target(position)) => {
                out(MachineControlEvent::Locate(seconds_of(position)));
            }
            MmcCommand::Locate(MmcLocate::Field(name)) => {
                match register(name).and_then(|n| self.registers[n]) {
                    Some(position) => {
                        out(MachineControlEvent::Locate(seconds_of(position)));
                    }
                    None => out(MachineControlEvent::Ignored(command)),
                }
            }
            MmcCommand::Write(fields) => self.write(&fields),
            MmcCommand::Move(MmcMove {
                destination,
                source,
            }) => match (register(destination), register(source)) {
                (Some(to), Some(from)) => self.registers[to] = self.registers[from],
                _ => out(MachineControlEvent::Ignored(command)),
            },
            MmcCommand::Group(MmcGroup::Assign { group, devices }) => {
                if devices.iter().any(|d| d == Recipient::Device(self.device)) {
                    self.groups |= 1 << group.get();
                }
            }
            MmcCommand::Group(MmcGroup::DisAssign { group, devices }) => {
                if devices
                    .iter()
                    .any(|d| d == Recipient::AllCall || d == Recipient::Device(self.device))
                {
                    match group {
                        Recipient::AllCall => self.groups = 0,
                        Recipient::Device(group) => self.groups &= !(1 << group.get()),
                    }
                }
            }
            MmcCommand::MmcReset => self.reset(),
            MmcCommand::Wait | MmcCommand::Resume => {}
            MmcCommand::FastForward
            | MmcCommand::Rewind
            | MmcCommand::RecordStrobe
            | MmcCommand::RecordExit
            | MmcCommand::RecordPause
            | MmcCommand::Eject
            | MmcCommand::Chase
            | MmcCommand::CommandErrorReset
            | MmcCommand::MaskedWrite(_)
            | MmcCommand::Read(_)
            | MmcCommand::Update(_)
            | MmcCommand::VariablePlay(_)
            | MmcCommand::Search(_)
            | MmcCommand::Shuttle(_)
            | MmcCommand::Step(_)
            | MmcCommand::AssignSystemMaster(_)
            | MmcCommand::GeneratorCommand(_)
            | MmcCommand::MtcCommand(_)
            | MmcCommand::Add(_)
            | MmcCommand::Subtract(_)
            | MmcCommand::DropFrameAdjust(_)
            | MmcCommand::Procedure(_)
            | MmcCommand::Event(_)
            | MmcCommand::CommandSegment(_)
            | MmcCommand::DeferredVariablePlay(_)
            | MmcCommand::RecordStrobeVariable(_)
            | MmcCommand::Reserved(_) => out(MachineControlEvent::Ignored(command)),
        }
    }

    /// A WRITE: the general purpose registers take their time; the other
    /// fields are state this follower does not keep.
    fn write(&mut self, fields: &MmcFields) {
        for field in fields {
            if let MmcInfoField::TimeCode(name, code) = field
                && let Some(n) = register(name.into())
                && let Ok(position) = MmcPosition::try_from(code)
            {
                self.registers[n] = Some(position);
            }
        }
    }
}

/// The general purpose register `08` to `0F` a name is, as an index.
fn register(name: tutti_midi_protocol::MmcName) -> Option<usize> {
    let code = name.code().get();
    (name.extension() == tutti_midi_protocol::MmcExtension::Base && (0x08..=0x0F).contains(&code))
        .then(|| usize::from(code - 0x08))
}
