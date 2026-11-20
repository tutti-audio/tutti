//! The loop region as a value (`transport_handle().looping()`): a write is
//! staged, the update publishes it as one edit for the next block, and a
//! block that adopts the published tempo, meter and loop allocates nothing.

use std::sync::Arc;

use tutti_buffers::{ChannelLayout, InterleavedMut, MaxBlock};
use tutti_music::{LoopRange, MeterChange, MeterMap, TimeSignature};
use tutti_session::Session;
use tutti_tempo::TempoMap;
use tutti_timeline::LoopRegion;
use tutti_units::{Beat, Bpm, Samples};

fn offline() -> (Session, tutti_session::Background) {
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("builds")
}

fn render(s: &Session, blocks: usize) {
    let mut block = vec![0.0f32; 512 * 2];
    for _ in 0..blocks {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    }
}

fn region(start: f64, end: f64, armed: bool) -> LoopRegion {
    LoopRegion::new(
        LoopRange::new(Beat::new(start), Beat::new(end)).unwrap(),
        armed,
    )
}

/// **A loop written and updated is the transport's loop region once a block
/// has applied it.**
#[test]
fn a_loop_set_and_updated_is_the_transports_region() {
    let (mut s, _bg) = offline();
    let looping = s.transport_handle().looping();
    s.set(looping.to(region(2.0, 6.0, true))).unwrap();
    let _ = s.update();
    render(&s, 1);
    assert_eq!(s.transport_handle().loop_region(), region(2.0, 6.0, true));
}

/// **A read before the update is the pending region, and the engine's is
/// untouched**: even a rendered block does not apply what the update has not
/// published.
#[test]
fn a_loop_read_before_the_update_is_the_pending_region() {
    let (mut s, _bg) = offline();
    let looping = s.transport_handle().looping();
    let default = s.get(looping).unwrap();
    assert!(!default.is_armed(), "the base is the transport's reading");

    s.set(looping.to(region(2.0, 6.0, true))).unwrap();
    assert_eq!(s.get(looping).unwrap(), region(2.0, 6.0, true));
    render(&s, 1);
    assert_eq!(s.transport_handle().loop_region(), default);
    let _ = s.update();
    render(&s, 1);
    assert_eq!(s.get(looping).unwrap(), region(2.0, 6.0, true));
}

/// **A loop read after the update and before any block is the region sent**,
/// not the engine's earlier one; once a block applies it the reading agrees.
#[test]
fn a_loop_read_after_the_update_and_before_a_block_is_the_region_sent() {
    let (mut s, _bg) = offline();
    let looping = s.transport_handle().looping();
    s.set(looping.to(region(2.0, 6.0, true))).unwrap();
    let _ = s.update();
    assert_eq!(s.get(looping).unwrap(), region(2.0, 6.0, true));
    assert!(
        !s.transport_handle().loop_region().is_armed(),
        "no block yet"
    );
    render(&s, 1);
    assert_eq!(s.get(looping).unwrap(), region(2.0, 6.0, true));
    assert_eq!(s.transport_handle().loop_region(), region(2.0, 6.0, true));
}

/// **Writing the region disarmed disarms the loop and keeps its bounds**, in
/// the one edit that carries both.
#[test]
fn a_loop_written_disarmed_disarms_and_keeps_the_bounds() {
    let (mut s, _bg) = offline();
    let looping = s.transport_handle().looping();
    s.set(looping.to(region(2.0, 6.0, true))).unwrap();
    let _ = s.update();
    render(&s, 1);
    s.set(looping.modify(|r| LoopRegion::new(r.bounds(), false)))
        .unwrap();
    let _ = s.update();
    render(&s, 1);
    let now = s.transport_handle().loop_region();
    assert!(!now.is_armed());
    assert_eq!(now.bounds(), region(2.0, 6.0, true).bounds());
}

/// **A block that adopts a published tempo, meter and loop allocates
/// nothing**: the writes and the update run outside the gate, and the audio
/// side only lends.
#[test]
fn blocks_adopting_published_tempo_meter_and_loop_do_not_allocate() {
    let (mut s, _bg) = offline();
    let handle = s.transport_handle();
    s.set((
        handle
            .tempo()
            .to(TempoMap::constant(Bpm::of(90.0)).unwrap()),
        handle.meter().to(Arc::new(MeterMap::new([MeterChange::new(
            Beat::new(0.0),
            TimeSignature::of(3, 4),
        )]))),
        handle.looping().to(region(0.0, 4.0, true)),
    ))
    .unwrap();
    let _ = s.update();
    handle.play().unwrap();
    let mut block = vec![0.0f32; 512 * 2];
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..4 {
            s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        }
    });
    assert_eq!(handle.loop_region(), region(0.0, 4.0, true));
}
