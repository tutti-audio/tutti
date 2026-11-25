use tutti_midi_file::{ClipFile, MidiFile, StandardMidiFile, Timebase};

/// Reads `data` as a Standard MIDI File or a MIDI 2.0 Clip File (by its magic).
///
/// A file that reads writes, and what it writes reads back equal to it (the
/// crate's lossless promise); its events, timeline and tempo maps answer without a panic.
pub(crate) fn midi_file_read(data: &[u8]) {
    let Ok(file) = MidiFile::from_bytes(data) else {
        return;
    };
    match file {
        MidiFile::Standard(file) => standard(&file),
        MidiFile::Clip(file) => clip(&file),
    }
}

fn standard(file: &StandardMidiFile) {
    match file.timebase() {
        Timebase::Metrical(metrical) => {
            let _ = metrical.events().count();
            let _ = metrical.timeline();
            let _ = metrical.seconds_at(metrical.end());
        }
        Timebase::Smpte(smpte) => {
            let _ = smpte.events().count();
        }
        Timebase::MetricalSequences(file) => {
            for sequence in file.sequences() {
                let _ = sequence.events().count();
                let _ = sequence.seconds_at(sequence.end());
            }
        }
        Timebase::SmpteSequences(file) => {
            for sequence in file.sequences() {
                let _ = sequence.events().count();
            }
        }
    }
    for track in file.tracks() {
        let _ = track.notes();
    }
    let mut bytes = Vec::new();
    file.write(&mut bytes).expect("a Vec takes the write");
    let again = StandardMidiFile::from_bytes(&bytes).expect("a file this crate wrote reads back");
    assert_eq!(&again, file, "write then read changed the file");
}

fn clip(file: &ClipFile) {
    let _ = file.timeline();
    let _ = file.meter_map();
    let _ = file.seconds_at(file.end());
    let _ = file.sequence().notes();
    let mut bytes = Vec::new();
    file.write(&mut bytes).expect("a Vec takes the write");
    let again = ClipFile::from_bytes(&bytes).expect("a file this crate wrote reads back");
    assert_eq!(&again, file, "write then read changed the file");
}
