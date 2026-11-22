use super::*;

/// A real-time stamp reads as its two words; a tick stamp, or none, reads as
/// `None`.
#[test]
fn a_real_time_stamp_reads_from_the_time_blob() {
    let mut ev = snd_seq_ump_event_t::default();
    ev.time[..4].copy_from_slice(&7u32.to_ne_bytes());
    ev.time[4..].copy_from_slice(&500u32.to_ne_bytes());
    assert_eq!(ev.real_time(), None, "flags say tick");
    ev.flags = SND_SEQ_TIME_STAMP_REAL;
    assert_eq!(
        ev.real_time(),
        Some(snd_seq_real_time_t {
            tv_sec: 7,
            tv_nsec: 500
        })
    );
    ev.flags = 0b10; // relative-time mode, tick stamp
    assert_eq!(ev.real_time(), None);
}
