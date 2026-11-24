//! Wrong programs the one `MidiFile` type lets a caller write: each compiles and fails
//! only when the file is built, or answers `None` for the kind it is not.

use tutti_midi_ci::{ProfileId, ProfileOnRequest, ProfileTarget};
use tutti_midi_file::{
    Layout, LeadTrack, Meta, MidiFile, Refusal, SmfDivision, Tick, ToClipError, Track, TracksError,
};
use tutti_ump::TicksPerQuarterNote;
use tutti_units::Bpm;

use super::common::metrical;

/// W1: a Clip File's configuration header in a Standard MIDI File.
#[test]
fn a_clip_header_in_an_smf_is_refused_when_built() {
    let on = ProfileOnRequest {
        target: ProfileTarget::Group,
        profile: ProfileId::MPE,
    };
    let smf = MidiFile::smf(
        SmfDivision::per_quarter(480),
        Layout::Format0(LeadTrack::new(Track::new().profile_on(on))),
    );
    assert_eq!(smf.unwrap_err(), TracksError::HeaderInSmf { track: 0 });
}

/// W2: a Standard MIDI File's track name in a Clip File.
#[test]
fn an_smf_track_name_in_a_clip_file_is_refused_when_built() {
    let clip = MidiFile::clip(TicksPerQuarterNote::of(96), Track::new().named("Lead"));
    assert!(matches!(
        clip.unwrap_err(),
        TracksError::Refused {
            refusal: Refusal::ClipSmfEvent,
            ..
        }
    ));
}

/// W3: a tempo meta event in a Clip File.
#[test]
fn a_tempo_meta_event_in_a_clip_file_is_refused_when_built() {
    let track = Track::new().meta(Tick(0), Meta::tempo(Bpm::of(120.0)).unwrap());
    let clip = MidiFile::clip(TicksPerQuarterNote::of(96), track);
    assert!(matches!(
        clip.unwrap_err(),
        TracksError::Refused {
            refusal: Refusal::ClipSmfEvent,
            ..
        }
    ));
}

/// W6: converting a Clip File compiles and fails when asked.
#[test]
fn a_clip_file_has_a_to_clip_that_refuses() {
    let smf = MidiFile::smf(
        SmfDivision::per_quarter(480),
        Layout::Format0(LeadTrack::new(Track::new())),
    )
    .unwrap();
    let clip = metrical(&smf).to_clip().unwrap().clip;
    assert_eq!(
        metrical(&clip).to_clip().unwrap_err(),
        ToClipError::AlreadyClip
    );
}

/// W8: the Standard MIDI File's facts asked of a Clip File answer `None`.
#[test]
fn a_clip_file_answers_none_for_an_smf_fact() {
    let clip = MidiFile::clip(TicksPerQuarterNote::of(96), Track::new()).unwrap();
    assert_eq!(clip.format(), None);
    assert_eq!(clip.division(), None);
    assert_eq!(clip.copyright(), None);
    assert_eq!(clip.sequence_number(0), None);
    assert_eq!(clip.smpte_offset(0), None);
    assert!(clip.repairs().is_empty());
}
