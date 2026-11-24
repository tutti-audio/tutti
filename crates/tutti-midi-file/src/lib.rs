#![doc = include_str!("../README.md")]
//! MIDI **file** codec: the Standard MIDI File (SMF, `.mid`) and the MIDI 2.0
//! Clip File (M2-116, `.midi2`), each its own type, read and written back
//! without losing an event.
//!
//! ## Two kinds of file
//!
//! A [`StandardMidiFile`] and a [`ClipFile`] hold different things and can do
//! different things, so they are two types, each with only what is true of it.
//! [`MidiFile`] is the one door that does not know the kind: it reads bytes and
//! answers which kind they were, and the app matches it.
//!
//! | | [`StandardMidiFile`] | [`ClipFile`] |
//! |---|---|---|
//! | holds | a format, a division, [`Track`]s, a copyright notice, the repairs a read made | the division (DCTPQ), a [`ClipHeader`], a [`ClipSequence`] |
//! | an event is | a [`SmfEvent`]: a channel message, a `SysEx` or `Escape` of any length, a [`Meta`] | a packet, as stored |
//! | tempo and meter | Set Tempo and Time Signature meta events | Flex Data Set Tempo and Set Time Signature messages, in the header or the sequence |
//! | built with | [`StandardMidiFile::new`] over a [`Layout`] | [`ClipFile::new`] over a header and a sequence |
//! | read with | [`StandardMidiFile::from_bytes`] ([`SmfReadError`]) | [`ClipFile::from_bytes`] ([`ClipReadError`]) |
//! | converts | [`MetricalView::to_clip`] gives a [`ClipFile`] | |
//!
//! Ticks are integers, so a write is exact. [`StandardMidiFile::timebase`] says
//! what a tick counts, and the metrical view's [`MetricalView::events`] gives the
//! merged timeline as [`Beat`](tutti_units::Beat)s; [`MetricalView::timeline`] and
//! [`ClipFile::timeline`] give the same as packets for a clip, and
//! [`Track::notes`] and [`ClipSequence::notes`] pair Note Ons with Note Offs. The
//! file layer does not translate: a MIDI 1.0 Note On at velocity 0 stays one.
//!
//! ## A Standard MIDI File
//!
//! - **A [`SmfEvent`]** is a `Channel` message (MIDI 1.0, with no Group), a
//!   `SysEx` or `Escape` of any length, or a `Meta` (text,
//!   channel prefix, tempo, time and key signature, sequencer-specific, the rest
//!   as bytes). The four metas that open a track (Sequence Number, Track Name,
//!   SMPTE Offset, copyright notice) are the fields of its track ([`Lead`]).
//! - **Lossless in its events.** A read, a write and a read give the same file:
//!   SysEx and meta included, but for what a read repairs (below). Running status
//!   may differ.
//! - **A track's opening metas are fields, a reader repairs.** A [`Track`]
//!   holds its Track Name ([`Track::named`]) and nothing of the file's other
//!   opening metas, so a track cloned out of a file carries none. The Sequence
//!   Number, SMPTE Offset and copyright notice are those of the [`LeadTrack`]
//!   of a format 0 or 1 file or of a format 2 [`Pattern`] and its [`Layout`]
//!   ([`StandardMidiFile::copyright`], [`StandardMidiFile::sequence_number`],
//!   [`StandardMidiFile::smpte_offset`]). The writer puts them first at tick 0,
//!   so a late or misplaced one is unwritable; a whole SysEx message is
//!   [`SysEx::Complete`], whose `F7` the writer adds. [`StandardMidiFile::new`]
//!   refuses what no type prevents ([`TracksError`]): a divided SysEx message
//!   left open or interrupted, a tempo map outside the first track of a
//!   format 1 file.
//!   A read accepts a file that has such an event out of place, makes it
//!   writable (a late or duplicate one dropped, one at tick 0 but not first
//!   moved into the field, a SysEx message closed) and lists each change in
//!   [`StandardMidiFile::repairs`].
//!   SMF 1.0 section 2.2 asks every file to specify its tempo and time
//!   signature, so a file is never without them: [`StandardMidiFile::new`] and a
//!   read add a Set Tempo and a Time Signature at tick 0 of the first track of a
//!   format 0 or 1 file (its tracks share one timeline, so one in any track
//!   counts) or of each format 2 pattern, when it has none, the 120 beats a
//!   minute in 4/4 a reader assumes without them. A format above 2 is read as
//!   format 2, each track its own pattern (section 2.1). An SMPTE division gets
//!   them too, where they are inert. A read lists each one it added
//!   ([`ReadRepair::DefaultTempo`], [`ReadRepair::DefaultTimeSignature`]).
//! - **Truncation is an error** ([`SmfParseError::ChunkCutOff`] and its siblings, with the position it was found at), not a shorter file.
//! - **Format 2** keeps its sequences apart: [`StandardMidiFile::timebase`] gives
//!   it [`MetricalSequences`] (or [`SmpteSequences`]), whose sequences each have
//!   their own tempo map and no merged `events()`, `timeline` or clip exist for
//!   it. So does a format above 2, which
//!   the specification leaves undefined and a reader may still read as its
//!   track chunks ([`SmfFormat::Other`]); the format word is written back
//!   as read.
//! - **Tempo** is [`MetricalView::tempo_map`], steps at the tick each Set Tempo
//!   meta event lands on; 120 BPM until the first if none is at the start.
//!   [`MetricalView::seconds_at`] integrates it. The map is derived from the
//!   events, so a Set Tempo stays an event of its track and a write gives it back
//!   exactly. It is the file's data: a project adopts a file's tempo only through
//!   an explicit call of its own.
//! - **SMPTE time is read.** A division with its top bit set
//!   ([`SmfDivision::Smpte`]) counts ticks of `fps * ticks per frame` a second,
//!   29.97 being 30000/1001: [`SmpteView::seconds_at`] and [`SmpteView::events`]
//!   answer in seconds. It has no beat grid and no tempo map of its own, so
//!   [`StandardMidiFile::timebase`] gives it a [`SmpteView`] whose
//!   [`timelines`](SmpteView::timelines) takes the tempo map that places its seconds
//!   on beats; the clip conversion is the [`MetricalView`]'s.
//!
//! ## A Clip File
//!
//! - **Its packets are as stored.** A [`ClipSequence`] holds `(Tick, Ump)`
//!   pairs. A read leaves out what is no event: its Null messages (a write puts
//!   them back where a long gap needs them), JR Clock and JR Timestamp messages,
//!   and the Utility messages M2-104 does not define; a packet no definition
//!   covers is kept as its words, so a read, a write and a read give the same
//!   words.
//! - **Its structure is checked as M2-116 orders it.** The messages before
//!   Start of Clip (Set Profile On, the DCTPQ, configuration messages, at most
//!   one Set Tempo and one Set Time Signature) are the [`ClipHeader`], on a clock
//!   of its own and not on the sequence's timeline; each ordering rule of
//!   sections 3.2 to 7.3 is one [`ClipFileError`] variant, read with the word it
//!   was found at ([`ClipReadError::Parse`]). A header Set Tempo is the tempo from
//!   the sequence's first tick, and a header Set Time Signature the meter from
//!   beat 0. The header's leading Set Profile On messages are read typed as
//!   [`ProfileOnRequest`](tutti_midi_ci::ProfileOnRequest)s
//!   ([`ClipHeader::profiles`]) and written with [`ClipHeader::profile_on`]; one
//!   that does not read is refused with [`ClipFileError::ProfileOn`].
//! - **A clip's first bar starts at Start of Clip**, tick 0 of the sequence. A
//!   pickup bar is a first bar with a Set Time Signature of its own length, and
//!   the bar after it starts with a new one; [`MetricalView::to_clip`] keeps both
//!   on the bar line the pickup's signature makes. The first Set Tempo and Set
//!   Time Signature at tick 0 are written under the Delta Clockstamp before Start
//!   of Clip.
//! - **Tempo and meter** are [`ClipFile::tempo_map`] and [`ClipFile::meter_map`],
//!   derived from the Flex Data messages and, like a Standard MIDI File's, the
//!   file's data.
//!
//! ## To a Clip File
//!
//! [`MetricalView::to_clip`] converts a format 0 or 1 SMF: its meta events become
//! the Flex Data messages of M2-116 Appendix B, and what has no UMP form comes
//! back in [`Converted::dropped`], never lost silently. A Set Tempo or Time
//! Signature off the grid a Clip File requires moves to the nearest position and
//! is listed in [`Converted::moved`]. There is no conversion the other way, and a
//! [`ClipFile`] has none to a clip: it is one already.
//!
//! [`MidiFile::read`] takes a path, bytes or an app's own source (any
//! `impl Into<Asset>`) and [`StandardMidiFile::write`] and [`ClipFile::write`] a
//! path or a writer (any `impl Into<Destination>`). [`MidiFileKind::detect`] tells
//! the two kinds apart by magic, not by extension. Nothing here touches an OS MIDI
//! API.
//!
//! ## Quick start
//!
//! Build a file, write it, read it back; the tempo map answers in seconds:
//!
//! ```rust
//! use tutti_midi_file::{LeadTrack, Layout, Meta, SmfDivision, StandardMidiFile, Tick, Track};
//! use tutti_units::{Bpm, Seconds};
//! use tutti_ump::{MidiChannel, NoteNumber, U7};
//!
//! // 120 BPM, then 60 from beat 4, at 480 ticks per quarter.
//! let four_quarters = Tick(4 * 480);
//! let track = Track::new()
//!     .meta(Tick(0), Meta::tempo(Bpm::of(120.0)).unwrap())
//!     .note(Tick(0), Tick(1_440), MidiChannel::FIRST, NoteNumber::of(60), U7::of(100))
//!     .meta(four_quarters, Meta::tempo(Bpm::of(60.0)).unwrap());
//! let file = StandardMidiFile::new(SmfDivision::per_quarter(480), Layout::Format0(LeadTrack::new(track)))?;
//!
//! let mut bytes = Vec::new();
//! file.write(&mut bytes)?;
//! let read = StandardMidiFile::from_bytes(&bytes)?;
//! assert_eq!(read, file);
//! assert_eq!(read.timebase().metrical().unwrap().seconds_at(Tick(8 * 480)), Seconds::new(6.0));
//! let notes = read.tracks()[0].notes();
//! assert_eq!((notes[0].start, notes[0].end), (Tick(0), Tick(1_440)));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! A clip plays a file's messages as they are: [`MetricalView::timeline`] is a [`SmfTimeline`]
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
//! ### The two kinds cannot be mixed
//!
//! What belongs to one kind is not on the other, so a program that puts it there
//! does not compile. A Clip File's configuration header is not a [`Track`]'s:
//!
//! ```compile_fail,E0599
//! use tutti_midi_ci::{ProfileId, ProfileOnRequest, ProfileTarget};
//! use tutti_midi_file::Track;
//!
//! let on = ProfileOnRequest { target: ProfileTarget::Group, profile: ProfileId::MPE };
//! let _ = Track::new().profile_on(on);
//! ```
//!
//! A track name is not a [`ClipSequence`]'s:
//!
//! ```compile_fail,E0599
//! use tutti_midi_file::ClipSequence;
//!
//! let _ = ClipSequence::new().named("Lead");
//! ```
//!
//! A meta event is not a packet, so a Clip File's sequence takes no tempo meta
//! event:
//!
//! ```compile_fail,E0277
//! use tutti_midi_file::{ClipSequence, Meta, Tick};
//! use tutti_units::Bpm;
//!
//! let _ = ClipSequence::new().push(Tick(0), Meta::tempo(Bpm::of(120.0)).unwrap());
//! ```
//!
//! A Clip File is already a clip, so it has no conversion to one:
//!
//! ```compile_fail,E0599
//! use tutti_midi_file::{ClipFile, ClipHeader, ClipSequence};
//! use tutti_ump::TicksPerQuarterNote;
//!
//! let clip = ClipFile::new(TicksPerQuarterNote::of(96), ClipHeader::new(), ClipSequence::new())
//!     .unwrap();
//! let _ = clip.to_clip();
//! ```
//!
//! A Clip File has no format word to answer `None` with:
//!
//! ```compile_fail,E0599
//! use tutti_midi_file::{ClipFile, ClipHeader, ClipSequence};
//! use tutti_ump::TicksPerQuarterNote;
//!
//! let clip = ClipFile::new(TicksPerQuarterNote::of(96), ClipHeader::new(), ClipSequence::new())
//!     .unwrap();
//! let _ = clip.format();
//! ```
//!
//! ### Reading from a path
//!
//! ```rust,no_run
//! use tutti_midi_file::{MidiFile, MidiFileKind};
//!
//! // Which kind? By magic bytes, not by extension: a `.mid` extension on a
//! // Clip File is a real thing that happens.
//! if MidiFileKind::detect("song.mid")?.is_some() {
//!     match MidiFile::read("song.mid")? {
//!         MidiFile::Standard(file) => {
//!             for track in file.tracks() {
//!                 println!("{:?}: {} notes", track.name(), track.notes().len());
//!             }
//!         }
//!         MidiFile::Clip(file) => println!("a clip of {} notes", file.sequence().notes().len()),
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod clip;
mod clip_file;
mod config;
mod error;
mod fault;
mod file;
mod kind;
mod midi_file;
mod ordered;
mod smf;

pub use clip::{ClipFile, ClipHeader, ClipSequence, ClipTimeline};
pub use clip_file::ClipFileError;
pub use config::{OutOfOrder, TracksError};
pub use error::{
    ClipBuildError, ClipPart, ClipReadError, ClipRefusal, ReadError, SmfReadError, WriteError,
};
pub use fault::SmfParseError;
pub use file::{
    Altered, Converted, DropReason, Dropped, Layout, Lead, LeadRepair, LeadTrack, LyricChange,
    MalformedMeta, Meta, MetricalSequence, MetricalSequences, MetricalView, MicrosPerQuarter,
    MicrosPerQuarterOutOfRange, Moved, OtherMetaKind, OtherMetaKindOutOfRange, OtherSmfFormat,
    OtherSmfFormatOutOfRange, PairedNote, Pattern, ReadRepair, RefusedTimeSignature, SmfEvent,
    SmfFormat, SmfPart, SmfTimeline, SmpteFractionalFrames, SmpteFractionalFramesOutOfRange,
    SmpteOffset, SmpteSequence, SmpteSequences, SmpteView, StandardMidiFile, SysEx, TextKind, Tick,
    Timebase, TimelineDrop, ToClipError, Track,
};
pub use kind::MidiFileKind;
pub use midi_file::MidiFile;
pub use smf::{
    SmfDivision, SmfTicksPerFrame, SmfTicksPerFrameOutOfRange, SmfTicksPerQuarter,
    SmfTicksPerQuarterOutOfRange,
};
