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
//! [`Beat`](tutti_units::Beat)s, [`MetricalView::timeline`] the same as packets for a clip, and [`Track::notes`] pairs Note Ons with Note
//! Offs.
//!
//! - **A [`FileEvent`]** is a `Message` (an SMF's channel message is MIDI 1.0 on
//!   the first Group; a Clip File's packet is as stored), a `SysEx` or `Escape` of any
//!   length, or a `Meta` (text, channel prefix, tempo, time and key signature,
//!   sequencer-specific, the rest as bytes). The four metas that open a track
//!   (Sequence Number, Track Name, SMPTE Offset, copyright notice) are the
//!   fields of its track ([`Lead`]).
//!   The file layer does not translate: a MIDI 1.0 Note On at velocity 0 stays one.
//! - **Lossless in its events.** A read, a write and a read give the same file:
//!   SysEx and meta included, but for what a read normalizes (below). Running status and a Delta Clockstamp's spelling
//!   may differ. A Clip File read leaves out what is no event: its Null messages
//!   (a write puts them back where a long gap needs them), JR Clock and JR
//!   Timestamp messages, and the Utility messages M2-104 does not define.
//! - **A track's opening metas are fields, a reader normalizes.** A [`Track`]
//!   holds its Track Name ([`Track::named`]) and nothing of the file's other
//!   opening metas, so a track cloned out of a file carries none. The Sequence
//!   Number, SMPTE Offset and copyright notice are those of the [`LeadTrack`]
//!   of a format 0 or 1 file or of a format 2 [`Pattern`] and its [`Layout`]
//!   ([`MidiFile::copyright`], [`MidiFile::sequence_number`],
//!   [`MidiFile::smpte_offset`]). The writer puts them first at tick 0, so a
//!   late or misplaced one is unwritable; a whole SysEx message is
//!   [`SysEx::Complete`], whose `F7` the writer adds. [`MidiFile::smf`]
//!   refuses what no type prevents ([`TracksError`]): a divided SysEx message
//!   left open or interrupted, a tempo map outside the first track of a
//!   format 1 file.
//!   A read accepts a file that has such an event out of place, makes it
//!   writable (a late or duplicate one dropped, one at tick 0 but not first
//!   moved into the field, a SysEx message closed) and lists each change in
//!   [`MidiFile::normalized`].
//!   SMF 1.0 section 2.2 asks every file to specify its tempo and time
//!   signature, so a file is never without them: [`MidiFile::smf`] and a read
//!   add a Set Tempo and a Time Signature at tick 0 of the first track of a
//!   format 0 or 1 file (its tracks share one timeline, so one in any track
//!   counts) or of each format 2 pattern, when it has none, the 120 beats a
//!   minute in 4/4 a reader assumes without them. A format above 2 is read as
//!   format 2, each track its own pattern (section 2.1). An SMPTE division gets
//!   them too, where they are inert. A read lists each one it added
//!   ([`Normalized::DefaultTempo`], [`Normalized::DefaultTimeSignature`]).
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
//!   [`ProfileOnRequest`](tutti_midi_ci::ProfileOnRequest)s
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
//! [`MidiFile::read`] takes a path, bytes or an app's own source (any
//! `impl Into<Asset>`) and [`MidiFile::write`] a path or a writer (any
//! `impl Into<Destination>`). [`MidiFileKind::detect`] tells the two containers
//! apart by magic, not by extension. Nothing here touches an OS MIDI API.
//!
//! ## Quick start
//!
//! Build a file, write it, read it back; the tempo map answers in seconds:
//!
//! ```rust
//! use tutti_midi_file::{LeadTrack, Layout, Meta, MidiFile, SmfDivision, Tick, Track};
//! use tutti_units::{Bpm, Seconds};
//! use tutti_ump::{MidiChannel, NoteNumber, U7};
//!
//! // 120 BPM, then 60 from beat 4, at 480 ticks per quarter.
//! let four_quarters = Tick(4 * 480);
//! let track = Track::new()
//!     .meta(Tick(0), Meta::tempo(Bpm::of(120.0)))
//!     .note(Tick(0), Tick(1_440), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100))
//!     .meta(four_quarters, Meta::tempo(Bpm::of(60.0)));
//! let file = MidiFile::smf(SmfDivision::per_quarter(480), Layout::Format0(LeadTrack::new(track)))?;
//!
//! let mut bytes = Vec::new();
//! file.write(&mut bytes)?;
//! let read = MidiFile::read(bytes)?;
//! assert_eq!(read, file);
//! assert_eq!(read.timebase().metrical().unwrap().seconds_at(Tick(8 * 480)), Seconds::new(6.0));
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
//! if MidiFileKind::detect("song.mid")?.is_some() {
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

pub use clip_file::ClipFileError;
pub use config::{OutOfOrder, Refusal, TrackPart, TracksError};
pub use error::{ReadError, WriteError};
pub use fault::SmfParseError;
pub use file::{
    Altered, Converted, DropReason, Dropped, FileEvent, Layout, Lead, LeadTrack, LyricChange,
    MalformedMeta, Meta, MetricalSequence, MetricalSequences, MetricalView, MicrosPerQuarter,
    MicrosPerQuarterOutOfRange, MidiFile, Moved, Normalized, OtherMetaKind,
    OtherMetaKindOutOfRange, OtherSmfFormat, OtherSmfFormatOutOfRange, PairedNote, Pattern,
    Placement, RefusedTimeSignature, SmfFormat, SmfPart, SmpteFractionalFrames,
    SmpteFractionalFramesOutOfRange, SmpteOffset, SmpteSequence, SmpteSequences, SmpteView, SysEx,
    TextKind, Tick, Timebase, Timeline, TimelineDrop, ToClipError, Track,
};
pub use kind::MidiFileKind;
pub use smf::{
    SmfDivision, SmfTicksPerFrame, SmfTicksPerFrameOutOfRange, SmfTicksPerQuarter,
    SmfTicksPerQuarterOutOfRange,
};
