//! Every piece of the session's MIDI in one program, offline and with no device.
//!
//! It builds a session, adds a synth as an instrument, sets MPE once, builds a
//! MIDI file and plays it as a clip, sends live notes, routes the hardware
//! input by Group, records a take from the live input and drains it, reads the
//! session's MIDI events, lists the devices, and opens an input port for the
//! first one listed. It needs no hardware: a session with no OS MIDI layer
//! lists no devices, and the program goes on without a port.
//!
//! ```text
//! cargo run -p tutti --features midi-hardware,synth --example midi_all
//! ```
//!
//! CI runs it as a test (`test = true`).
//!
//! **Not covered here:** MIDI Time Code out, and the MIDI-CI session
//! ([`MidiHardwareConfig::ci`](tutti::midi::MidiHardwareConfig::ci) and the peers a
//! [`MidiInput`](tutti::midi::MidiInput) lists), which needs a peer device.

use tutti::midi::{
    AddedInstrument, FileEvent, Layout, LeadTrack, Message, MidiChannel, MidiFile, MidiGroup,
    MidiHardwareConfig, MidiRouteRule, MpeMode, MpeZoneConfig, MpeZones, NoteNumber, NoteVelocity,
    PitchBendSensitivity, SmfDivision, Tick, TicksPerQuarterNote, Track, U7, Voice1,
};
use tutti::polysynth::{PolySynth, SynthConfig};
use tutti::session::{Start, TakeLatency, Trim};
use tutti::{
    ChannelLayout, Error, EventCapacity, Frame, InterleavedMut, MaxBlock, SampleRate, Samples,
    Session, SessionEvent, Stamp,
};

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 512;
const DIVISION: TicksPerQuarterNote = TicksPerQuarterNote::of(480);
const SMF_DIVISION: SmfDivision = SmfDivision::per_quarter(480);

/// What the run found, for the test below to hold it to.
struct Outcome {
    /// The loudest sample rendered.
    peak: f32,
    /// What the take read from the live input, with where each was placed.
    take: Vec<(Frame, Frame)>,
    /// The session's MIDI events, as their debug text.
    events: Vec<String>,
    /// The devices listed, and whether an input port was opened for the first
    /// of them.
    devices: usize,
    opened: bool,
}

/// A session for the program: the hardware's OS layer if it opens, else none.
/// Hears Groups 1 and 2 (`midi_groups`), so a route can name Group 2.
fn session() -> Result<Session, Error> {
    let build = |hardware: bool| {
        let builder = Session::builder().midi_groups(MidiGroup::of(1));
        let builder = if hardware {
            builder.midi_hardware(MidiHardwareConfig::new())
        } else {
            builder
        };
        builder
            .sample_rate(RATE)
            .max_block(MaxBlock::of(Samples::new(BLOCK)))
            .build()
    };
    // A machine with no MIDI sequencer fails to open the OS layer: say so, and
    // run on without it.
    build(true).or_else(|e| {
        println!("no OS MIDI layer ({e}); running without devices");
        build(false)
    })
}

/// A file built from a `Track`: four quarter notes up a scale, collected from
/// `(Tick, event)` pairs, then written and read back as bytes (an SMF).
fn file() -> Result<MidiFile, Box<dyn std::error::Error>> {
    let track: Track = (0..4u8)
        .flat_map(|n| {
            let at = Tick::quarters(n.into(), DIVISION).expect("four quarters fit");
            let key = NoteNumber::of(60 + 2 * n);
            let (on, off): (FileEvent, FileEvent) = (
                Message::midi1(MidiChannel::FIRST, Voice1::note_on(key, U7::of(100))).into(),
                Message::midi1(MidiChannel::FIRST, Voice1::note_off(key, U7::of(64))).into(),
            );
            [
                (at, on),
                (at.checked_add(240).expect("a quarter fits"), off),
            ]
        })
        .collect();
    let built = MidiFile::smf(SMF_DIVISION, Layout::Format0(LeadTrack::new(track)))?;
    let mut bytes = Vec::new();
    built.write(&mut bytes)?;
    Ok(MidiFile::read(bytes)?)
}

fn run() -> Result<Outcome, Box<dyn std::error::Error>> {
    let mut s = session()?;

    // MPE is set once, on the session: the hardware input node and every clip's
    // transform read it, and no synth holds a zone.
    let zone = MpeZones::new(
        MpeZoneConfig::lower(
            7,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        ),
        None,
    )
    .ok_or("one lower zone")?;
    s.set(s.mpe().to(MpeMode::Zones(zone)))?;

    // A synth as an instrument: its strip is wired to the master.
    let AddedInstrument {
        handle: mut synth, ..
    } = s.add_instrument(PolySynth::new(SynthConfig::default()))?;

    // A tap on what the instrument's handle plays, read with `drain`.
    let mut take = s.midi().tap(synth.node(), EventCapacity::of(256))?;

    // A clip from a file's timeline: the packets it holds at their beats.
    // The load is a value the app runs: `run` reads the file and converts each
    // track to a clip value on this thread.
    let path = std::env::temp_dir().join(format!("tutti-midi-all-{}.mid", std::process::id()));
    file()?.write(&path)?;
    let loaded = s.midi().load_midi(&path).run();
    std::fs::remove_file(&path)?;
    let loaded = loaded?;
    s.midi().play_track(synth.node(), loaded.track(0)?)?;

    // Live notes, through the handle's keys: a MIDI 2.0 note with its
    // 16-bit velocity, and a decoded `Message`, which `push` takes as it takes
    // a packet.
    let live = synth.keys_mut();
    live.note_on(
        Stamp::NextBlock,
        MidiChannel::of(1),
        NoteNumber::of(72),
        NoteVelocity::new(0xC000),
    )?;
    live.push(
        Stamp::NextBlock,
        Message::midi1(
            MidiChannel::of(1),
            Voice1::note_on(NoteNumber::of(76), U7::of(90)),
        ),
    )?;

    // The hardware input, routed by Group: Group 2's channel 3 to the synth. A
    // rule is an id for `unroute`; nothing arrives with no device, and the
    // call refuses only on a session built with no input node.
    let rule = MidiRouteRule::for_channel(MidiChannel::of(3))
        .in_group(MidiGroup::of(1))
        .to(synth.node());
    let route = s.midi().route(rule)?;

    // Devices, behind a check that there are any. A saved device, a `DeviceInfo`
    // listed or kept from an earlier run, is opened as an input port: it opens
    // with the port reading `Lost` if the device is away, reopens when it arrives
    // and is surely the same one, and fails to open, with the candidates, only
    // when several present devices could be it. The port closes when its last
    // handle is dropped.
    //
    //     let keys = s.open_midi_input(saved)?;
    //     keys.presence();                     // Live, Lost(PortCause::Unplugged), ...
    let devices = list_devices(&mut s);
    let first = s
        .midi()
        .devices()
        .into_iter()
        .find(|device| device.direction.is_input());
    let keys = first.map(|device| s.open_midi_input(device)).transpose()?;
    let opened = keys.is_some();
    s.midi().unroute(route)?;

    // Play, render, and read what the session reports.
    s.play()?;
    let mut events = Vec::new();
    let mut peak = 0.0f32;
    let mut block = vec![0.0f32; BLOCK * 2];
    for _ in 0..200 {
        for event in s.update() {
            match event {
                SessionEvent::Midi(e) => events.push(format!("{e:?}")),
                other => events.push(format!("{other:?}")),
            }
        }
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        peak = block.iter().fold(peak, |p, x| p.max(x.abs()));
    }

    // The take: what was played, each event on its frame of the graph, placed
    // where the performer meant it for this device's latencies (figures a host
    // reads off its devices; here none reported, and a 10 ms trim).
    let latency = TakeLatency::new(None, None, Samples::ZERO, Trim::Later(Samples::new(480)));
    let take = take
        .drain()
        .map(|e| {
            let placed = match latency.place(e.frame) {
                Start::At(frame) => frame,
                // `place` never clamps; a start before the clock's origin
                // is clamped to the first frame here.
                Start::BeforeOrigin(_) => Frame::new(0),
            };
            (e.frame, placed)
        })
        .collect();
    Ok(Outcome {
        peak,
        take,
        events,
        devices,
        opened,
    })
}

/// The devices present, inputs and outputs. Empty with no OS layer.
fn list_devices(s: &mut Session) -> usize {
    let devices = s.midi().devices();
    for d in &devices {
        println!("device {:?}: {} ({:?})", d.id, d.name, d.direction);
    }
    println!("{} MIDI device(s)", devices.len());
    devices.len()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let o = run()?;
    println!("peak {:.3}", o.peak);
    println!("take: {} events", o.take.len());
    for (frame, placed) in &o.take {
        println!("  played at frame {frame}, placed at {placed}");
    }
    for e in &o.events {
        println!("session event: {e}");
    }
    let _ = (o.devices, o.opened);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The program runs offline and hears what it played**: the clip and the
    /// live notes sound (a peak), the take holds the two live notes on their
    /// frames and each is placed 480 frames later (the trim), and a port is
    /// opened exactly when the runner lists an input (a runner that has some
    /// lists them, so only what the session saw is held to its own list).
    #[test]
    fn the_program_runs_and_hears_what_it_played() {
        let o = run().expect("the program runs");
        assert!(o.peak > 0.01, "the synth sounded: {}", o.peak);
        assert_eq!(o.take.len(), 2, "the two live notes: {:?}", o.take);
        for (frame, placed) in &o.take {
            assert_eq!(placed.get(), frame.get() + 480, "the trim moves it later");
        }
        assert!(
            !o.opened || o.devices > 0,
            "a port is opened only for a listed device"
        );
        assert!(
            o.events.iter().all(|e| !e.contains("EventsDropped")),
            "nothing dropped: {:?}",
            o.events
        );
    }
}
