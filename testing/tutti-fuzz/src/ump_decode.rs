use tutti_ump::{Ump, split_stream};

/// Reads `data` as little-endian words and decodes them as packets, both
/// stream-split and as four-word windows at every offset.
///
/// A decoded message re-encodes to a packet that decodes to the same message
/// (reserved bits are dropped, reserved values are kept), and that packet is
/// then a fixed point of decode and encode.
pub(crate) fn ump_decode(data: &[u8]) {
    let words = crate::words(data);
    for ump in split_stream(&words) {
        check(ump);
    }
    for at in 0..words.len() {
        let mut packet = [0u32; 4];
        for (slot, word) in packet.iter_mut().zip(&words[at..]) {
            *slot = *word;
        }
        check(Ump(packet));
    }
}

fn check(ump: Ump) {
    let _ = ump.words();
    let _ = ump.is_note_off();
    let _ = ump.to_string();
    if let Ok(message) = ump.decode() {
        let canonical = Ump::from(message);
        assert_eq!(
            canonical.decode(),
            Ok(message),
            "{ump} decodes to {message:?}, whose packet {canonical} reads differently"
        );
    }
}
