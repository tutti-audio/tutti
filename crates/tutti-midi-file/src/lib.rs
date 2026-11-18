#![doc = include_str!("../README.md")]
//! MIDI **file** codec: Standard MIDI File (SMF, `.mid`) and MIDI 2.0 Clip File
//! (M2-116, `.midi2`) read into one model and written back from it without losing an event.
//!
//! ## The model
//!
//! A [`MidiFile`] is its container (an SMF's format 0, 1, 2 or above, or a Clip File),
//! its division, its [`Track`]s and the `TempoMap` its tempo events declare. A
//! track is `(Tick, FileEvent)` in order and where it ends. Ticks are integers,
//! so a write is exact; [`MidiFile::timebase`] says what a tick counts, and the
//! metrical view's [`MetricalView::events`] gives the merged timeline as
//! [`Beat`](tutti_types::Beat)s, [`MetricalView::timeline`] the same as packets for a clip, and [`Track::notes`] pairs Note Ons with Note
//! Offs.
//!
//! - **A [`FileEvent`]** is a `Message` (an SMF's channel message is MIDI 1.0 on
//!   the first Group; a Clip File's packet is as stored), a `SysEx` or `Escape` of any
//!   length, or a `Meta` (text, sequence number, channel prefix, tempo, SMPTE
//!   offset, time and key signature, sequencer-specific, the rest as bytes).
//!   The file layer does not translate: a MIDI 1.0 Note On at velocity 0 stays one.
//! - **Lossless in its events.** A read, a write and a read give the same file:
//!   SysEx and meta included. Running status and a Delta Clockstamp's spelling
//!   may differ. A Clip File read leaves out what is no event: its Null messages
//!   (a write puts them back where a long gap needs them), JR Clock and JR
//!   Timestamp messages, and the Utility messages M2-104 does not define.
//! - **A writer keeps SMF 1.0's placement rules, a reader keeps the file.**
//!   [`MidiFile::smf`] refuses a SysEx message that does not end in `F7`, a
//!   Sequence Number or Track Name after time 0, and a format 1 file whose tempo
//!   map, SMPTE Offset or Sequence Number is not in its first track
//!   ([`TracksError`]); a read keeps such a file, its Sequence Number and Track
//!   Name counting only at the start. [`Track::numbered`],
//!   [`Track::smpte_start`] and [`Track::copyright`] place their event where the
//!   specification puts it, and [`MidiFile::specifying_tempo_and_meter`] adds the
//!   tempo and time signature a file without them is read with.
//! - **Truncation is an error** ([`SmfParseError::ChunkCutOff`] and its siblings, with the position it was found at), not a shorter file.
//! - **Format 2** keeps its sequences apart: [`MidiFile::timebase`] gives it
//!   [`MetricalSequences`] (or [`SmpteSequences`]), whose sequences each have
//!   their own tempo map and no merged `events()`, `timeline` or clip exist for
//!   it. So does a format above 2, which
//!   the specification leaves undefined and a reader may still read as its
//!   track chunks ([`SmfFormat::Other`]); the format word is written back
//!   as read.
//! - **Tempo** is [`MetricalView::tempo_map`], steps at the tick each Set Tempo (an
//!   SMF's meta event, a Clip File's Flex Data message) lands on; 120 BPM until
//!   the first if none is at the start. [`MetricalView::seconds_at`] integrates it. The map is
//!   derived from the events, so a Set Tempo stays an event of its track and a
//!   write gives it back exactly.
//! - **A Clip File's structure is checked as M2-116 orders it.** The messages
//!   before Start of Clip (Set Profile On, the DCTPQ, configuration messages, at
//!   most one Set Tempo and one Set Time Signature) are the [`Track`]'s `header`,
//!   on a clock of their own and not on the sequence's timeline; each ordering
//!   rule of sections 3.2 to 7.3 is one [`ClipFileError`] variant, read with the word it was found at ([`ReadError::Clip`]). A header Set
//!   Tempo is the tempo from the sequence's first tick. The header's leading Set
//!   Profile On messages are read typed as
//!   [`ProfileOnRequest`](tutti_midi_device::ProfileOnRequest)s
//!   ([`Track::profiles`]) and written with [`Track::profile_on`]; one that does
//!   not read is refused with [`ClipFileError::ProfileOn`].
//! - **A clip's first bar starts at Start of Clip**, tick 0 of the sequence. A
//!   pickup bar is a first bar with a Set Time Signature of its own length, and
//!   the bar after it starts with a new one; [`MetricalView::to_clip`] keeps both on
//!   the bar line the pickup's signature makes. The first Set Tempo and Set Time
//!   Signature at tick 0 are written under the Delta Clockstamp before Start of
//!   Clip.
//! - **SMPTE time is read.** A division with its top bit set
//!   ([`SmfDivision::Smpte`]) counts ticks of `fps * ticks per frame` a second,
//!   29.97 being 30000/1001: [`SmpteView::seconds_at`] and [`SmpteView::events`]
//!   answer in seconds. It has no beat grid, so [`MidiFile::timebase`] gives it
//!   a [`SmpteView`] with no tempo map, beats, timeline or clip: those are
//!   the [`MetricalView`]'s.
//!
//! - **To a Clip File.** [`MetricalView::to_clip`] converts a format 0 or 1 SMF: its meta
//!   events become the Flex Data messages of M2-116 Appendix B, and what has no UMP
//!   form comes back in [`Converted::dropped`], never lost silently. A Set Tempo or Time
//!   Signature off the grid a Clip File requires moves to the nearest position and is
//!   listed in [`Converted::moved`].
//!
//! [`MidiFileKind::detect`] tells the two containers apart by magic, not by
//! extension. Nothing here touches an OS MIDI API.
//!
//! ## Quick start
//!
//! Build a file, write it, read it back; the tempo map answers in seconds:
//!
//! ```rust
//! use tutti_midi_file::{Meta, MidiFile, SmfDivision, SmfFormat, Tick, Track};
//! use tutti_types::{Bpm, Seconds};
//! use tutti_ump::{MidiChannel, NoteNumber, U7};
//!
//! // 120 BPM, then 60 from beat 4, at 480 ticks per quarter.
//! let four_quarters = Tick(4 * 480);
//! let track = Track::new()
//!     .meta(Tick(0), Meta::tempo(Bpm::of(120.0)))
//!     .note(Tick(0), Tick(1_440), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100))
//!     .meta(four_quarters, Meta::tempo(Bpm::of(60.0)));
//! let file = MidiFile::smf(SmfFormat::SingleTrack, SmfDivision::per_quarter(480), vec![track])?;
//!
//! let read = MidiFile::from_bytes(&file.to_bytes())?;
//! assert_eq!(read, file);
//! assert_eq!(read.timebase().metrical().unwrap().seconds_at(Tick(8 * 480)), Seconds(6.0));
//! let notes = read.tracks()[0].notes();
//! assert_eq!((notes[0].start, notes[0].end), (Tick(0), Tick(1_440)));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! A clip plays a file's messages as they are: [`MetricalView::timeline`] is a [`Timeline`]
//! whose `packets` are `(Beat, Ump)` pairs (channel messages as their packets, SysEx
//! as SysEx7 packets, a divided SysEx as a Start, Continues and an End each at its
//! own event's beat, meta events left out: the tempo is [`MetricalView::tempo_map`]) and
//! whose `dropped` lists the SysEx and escape events no packet holds. A format 2
//! file has no one timeline, so it has no [`MetricalView`] to ask.
//!
//! ### Tempo is the wire's
//!
//! SMF stores tempo as microseconds per quarter note, an integer: `174.0` BPM is
//! not representable and reads back as `174.0002958...`. That is the format.
//!
//! ### Reading from a path
//!
//! ```rust,no_run
//! use tutti_midi_file::{MidiFile, MidiFileKind};
//!
//! // Which container? By magic bytes, not by extension: a `.mid` extension on a
//! // Clip File is a real thing that happens.
//! if MidiFileKind::detect_path("song.mid")?.is_some() {
//!     let file = MidiFile::read("song.mid")?;
//!     for track in file.tracks() {
//!         println!("{:?}: {} notes", track.name(), track.notes().len());
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod clip_file;
mod config;
mod error;
mod fault;
mod file;
mod kind;
mod smf;
mod write;

pub use clip_file::ClipFileError;
pub use config::{OutOfOrder, Refusal, TrackPart, TracksError};
pub use error::{ReadError, WriteError};
pub use fault::SmfParseError;
pub use file::{
    Altered, Converted, DropReason, Dropped, FileEvent, LyricChange, MalformedMeta, Meta,
    MetricalSequence, MetricalSequences, MetricalView, MicrosPerQuarter,
    MicrosPerQuarterOutOfRange, MidiFile, Moved, OtherMetaKind, OtherMetaKindOutOfRange,
    OtherSmfFormat, OtherSmfFormatOutOfRange, PairedNote, SmfFormat, SmpteFractionalFrames,
    SmpteFractionalFramesOutOfRange, SmpteOffset, SmpteSequence, SmpteSequences, SmpteView,
    TextKind, Tick, Timebase, Timeline, TimelineDrop, ToClipError, Track,
};
pub use kind::MidiFileKind;
pub use smf::{
    SmfDivision, SmfTicksPerFrame, SmfTicksPerFrameOutOfRange, SmfTicksPerQuarter,
    SmfTicksPerQuarterOutOfRange,
};
pub use write::WriteOptions;
