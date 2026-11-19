//! A MIDI file as a job: [`Midi::load_midi`] reads the file at a path on a
//! worker and lands a [`LoadedMidi`], each track already a clip value.
//!
//! The reading, the conversion of every track's events to a [`ClipEvents`], and
//! the tempo map and meter of its timeline are all the worker's; the control
//! thread receives finished values and publishes them
//! ([`Midi::play_track`]). A file's ticks become beats over its division, so no
//! sample rate enters and the job never converts to frames.
//!
//! What the read repaired ([`Normalized`]) and what the conversion left out
//! (a [`DroppedEvent`]) is the landed value's [`report`](LoadedMidi::report),
//! never a log line.

use std::path::PathBuf;
use std::sync::Arc;

use tutti_buffers::Float;
use tutti_midi_file::{
    FileEvent, Meta, MidiFile, Normalized, ReadError, Timebase, TimelineDrop, Track,
};
use tutti_midi_runtime::{ClipEvents, TimedMidiEvent};
use tutti_music::{MeterChange, MeterMap, TimeSignature};
use tutti_tempo::TempoMap;
use tutti_transport::ProjectMeter;
use tutti_ump::TicksPerQuarterNote;
use tutti_units::Beat;

use crate::job::job_kind;
use crate::{Midi, WaitError, WorkerError};

job_kind! {
    /// A MIDI file read and converted to clip values ([`Midi::load_midi`]); the
    /// job lands a [`LoadedMidi`].
    pub LoadMidi => LoadedMidi, LoadMidiError
}

/// Why a [`LoadMidi`] job ended without a [`LoadedMidi`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoadMidiError {
    /// The file could not be read.
    #[error("MIDI file: {0}")]
    Read(#[from] ReadError),
    /// The worker did not finish: its work panicked, or the spawner dropped it.
    #[error(transparent)]
    Worker(#[from] WorkerError),
    /// The wait could not run ([`Job::wait`](crate::Job::wait)).
    #[error(transparent)]
    Wait(#[from] WaitError),
}

/// A [`LoadedMidi`] has no track `index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the file has no track {index}: {tracks} tracks are loaded")]
pub struct NoSuchTrackError {
    /// The index asked for.
    pub index: usize,
    /// How many tracks were loaded: none for a file in SMPTE time, which has
    /// no beats to place a clip on.
    pub tracks: usize,
}

/// A [`LoadedMidi`] has no one tempo map and meter: its tracks are independent
/// sequences (format 2), each with its own ([`LoadedTrack`]), or it is in SMPTE
/// time and has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the file has no one timeline: its tracks are independent sequences, or it has no beats")]
pub struct NoCommonTimelineError;

/// One track of a [`LoadedMidi`] as the values a session plays and follows.
#[derive(Clone, Debug)]
pub struct LoadedTrack {
    /// The track's events in beats, ready to play ([`Midi::play_track`]).
    pub events: Arc<ClipEvents>,
    /// The tempo map the track plays on: the file's one map in a format 0 or 1
    /// file, the sequence's own in a format 2 file.
    pub tempo: Arc<TempoMap>,
    /// The meter its Time Signature events declare, bars counted from beat 0;
    /// 4/4 where the track declares none.
    pub meter: Arc<ProjectMeter>,
}

/// An event of a track's timeline that has no packet, and so is not played
/// ([`Timeline::dropped`](tutti_midi_file::Timeline::dropped)).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct DroppedEvent {
    /// The track the event is in.
    pub track: usize,
    /// Where the event lands.
    pub beat: Beat,
    /// The event.
    pub event: FileEvent,
    /// Why it has no packet.
    pub why: TimelineDrop,
}

/// What loading a file changed or left out: the app's to show or to act on.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct LoadReport {
    /// What the read repaired so that the file can be written
    /// ([`MidiFile::normalized`]).
    pub normalized: Vec<Normalized>,
    /// The events the conversion to clips left out, in track order.
    pub dropped: Vec<DroppedEvent>,
}

impl LoadReport {
    /// Whether the file was read as it is and every event is in a clip.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.normalized.is_empty() && self.dropped.is_empty()
    }
}

/// The landing of a [`LoadMidi`] job: the file as read, and each track as a
/// [`LoadedTrack`].
#[derive(Debug)]
pub struct LoadedMidi {
    file: MidiFile,
    tracks: Vec<LoadedTrack>,
    /// The timeline every track shares: a format 0 or 1 file's.
    common: Option<(Arc<TempoMap>, Arc<ProjectMeter>)>,
    report: LoadReport,
}

impl LoadedMidi {
    /// The file as read.
    #[must_use]
    pub fn file(&self) -> &MidiFile {
        &self.file
    }

    /// Track `i`, in the file's track order.
    ///
    /// # Errors
    ///
    /// [`NoSuchTrackError`] when the file has no track `i`, or is in SMPTE
    /// time and so has no clip.
    pub fn track(&self, i: usize) -> Result<&LoadedTrack, NoSuchTrackError> {
        self.tracks.get(i).ok_or(NoSuchTrackError {
            index: i,
            tracks: self.tracks.len(),
        })
    }

    /// The tempo map every track shares: a format 0 or 1 file's.
    ///
    /// # Errors
    ///
    /// [`NoCommonTimelineError`] for a format 2 file, whose tracks each keep
    /// their own ([`LoadedTrack::tempo`]), and for a file in SMPTE time.
    pub fn tempo(&self) -> Result<&Arc<TempoMap>, NoCommonTimelineError> {
        self.common
            .as_ref()
            .map(|(tempo, _)| tempo)
            .ok_or(NoCommonTimelineError)
    }

    /// The meter every track shares: a format 0 or 1 file's.
    ///
    /// # Errors
    ///
    /// As [`tempo`](Self::tempo).
    pub fn meter(&self) -> Result<&Arc<ProjectMeter>, NoCommonTimelineError> {
        self.common
            .as_ref()
            .map(|(_, meter)| meter)
            .ok_or(NoCommonTimelineError)
    }

    /// What the read repaired and the conversion left out.
    #[must_use]
    pub fn report(&self) -> &LoadReport {
        &self.report
    }
}

impl<F: Float> Midi<'_, F> {
    /// Reads the MIDI file at `path` on the session's spawner and converts each
    /// track to a clip value there. Returns at once; the [`LoadedMidi`] lands
    /// in a later [`update`](crate::Session::update) as a
    /// [`SessionEvent::Landed`](crate::SessionEvent::Landed), or
    /// [`Job::wait`](crate::Job::wait) returns it. A file that cannot be read
    /// lands [`LoadMidiError::Read`]. **Dropping the [`Job`](crate::Job)
    /// discards the result.**
    ///
    /// A file in SMPTE time has seconds and no beats: it lands with no track
    /// and no common timeline.
    pub fn load_midi(&mut self, path: impl Into<PathBuf>) -> crate::Job<LoadMidi> {
        let path = path.into();
        self.jobs.submit_job(
            move |_| load(&path),
            |_, done, ticket| {
                ticket.land(match done {
                    Ok(loaded) => loaded,
                    Err(failed) => Err(failed.into()),
                });
            },
        )
    }
}

/// Reads `path` and builds its [`LoadedMidi`].
fn load(path: &PathBuf) -> Result<LoadedMidi, LoadMidiError> {
    Ok(convert(MidiFile::read(path)?))
}

/// The clips, timeline values and report of `file`.
fn convert(file: MidiFile) -> LoadedMidi {
    let mut report = LoadReport {
        normalized: file.normalized().to_vec(),
        dropped: Vec::new(),
    };
    let mut tracks = Vec::new();
    let mut common = None;
    match file.timebase() {
        Timebase::Metrical(view) => {
            let tempo = Arc::clone(view.tempo_map());
            let meter = Arc::new(meter_of(view.events()));
            for (index, track) in file.tracks().iter().enumerate() {
                tracks.push(LoadedTrack {
                    events: clip_of(&mut report, index, track, view.ticks_per_quarter()),
                    tempo: Arc::clone(&tempo),
                    meter: Arc::clone(&meter),
                });
            }
            common = Some((tempo, meter));
        }
        Timebase::MetricalSequences(sequences) => {
            for (index, sequence) in sequences.sequences().enumerate() {
                tracks.push(LoadedTrack {
                    events: clip_of(
                        &mut report,
                        index,
                        sequence.track(),
                        sequences.ticks_per_quarter(),
                    ),
                    tempo: Arc::clone(sequence.tempo_map()),
                    meter: Arc::new(meter_of(sequence.events())),
                });
            }
        }
        Timebase::Smpte(_) | Timebase::SmpteSequences(_) => {}
    }
    LoadedMidi {
        file,
        tracks,
        common,
        report,
    }
}

/// `track`'s events as a clip, its dropped events added to `report`.
fn clip_of(
    report: &mut LoadReport,
    index: usize,
    track: &Track,
    ticks_per_quarter: TicksPerQuarterNote,
) -> Arc<ClipEvents> {
    let timeline = track.timeline(ticks_per_quarter);
    report.dropped.extend(
        timeline
            .dropped
            .into_iter()
            .map(|(beat, event, why)| DroppedEvent {
                track: index,
                beat,
                event,
                why,
            }),
    );
    // A timeline's beats are finite, which is all a clip event asks.
    Arc::new(
        timeline
            .packets
            .into_iter()
            .filter_map(|(beat, packet)| TimedMidiEvent::new(beat, packet))
            .collect(),
    )
}

/// The meter the Time Signature events declare, a change at the beat each
/// lands on. A signature no [`TimeSignature`] holds (a zero numerator, a
/// denominator past a 32-bit power of two) is not a change.
fn meter_of<'a>(events: impl Iterator<Item = (Beat, &'a FileEvent)>) -> ProjectMeter {
    ProjectMeter::new(MeterMap::new(events.filter_map(|(beat, event)| {
        let FileEvent::Meta(Meta::TimeSignature {
            numerator,
            denominator_power,
            ..
        }) = event
        else {
            return None;
        };
        let denominator = 1u32.checked_shl(u32::from(*denominator_power))?;
        let signature = TimeSignature::from_numbers(u32::from(*numerator), denominator)?;
        Some(MeterChange::new(beat, signature))
    })))
}
