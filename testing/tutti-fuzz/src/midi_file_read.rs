use tutti_midi_file::{MidiFile, Timebase};

/// Reads `data` as a Standard MIDI File or a MIDI 2.0 Clip File (by its magic).
///
/// A file that reads writes, and what it writes reads back equal to it (the
/// crate's lossless promise); its timeline and tempo map answer without a panic.
pub fn midi_file_read(data: &[u8]) {
    let Ok(file) = MidiFile::from_bytes(data) else {
        return;
    };
    match file.timebase() {
        Timebase::Metrical(metrical) => {
            let _ = metrical.events().map(Iterator::count);
            let _ = metrical.timeline();
        }
        Timebase::Smpte(smpte) => {
            let _ = smpte.events().map(Iterator::count);
        }
    }
    for track in file.tracks() {
        let _ = track.notes();
    }
    let bytes = file.to_bytes();
    let again = MidiFile::from_bytes(&bytes).expect("a file this crate wrote reads back");
    assert_eq!(again, file, "write then read changed the file");
}
