//! A declared instrument: its node, the queue its keys fill and the strip after
//! its main pair are one declaration. The session lends the keys once, a patch
//! that changes the key's kind or drops it takes all three parts away, and a
//! wire that names the key leaves from its strip.

use tutti_buffers::{ChannelLayout, ChannelTopology, InterleavedMut};
use tutti_graph::{Cx, InstrumentNode, Io, Node, Prepare, Shape, Stamp, Status, declare_params};
use tutti_midi_runtime::MidiKeys;
use tutti_rt::PushError;
use tutti_session::{
    ConnectError, InstrumentDecl, KeyError, LeaseError, MainOutput, Mention, NodeDecl, PatchError,
    Session, WireEnds, WireStart,
};
use tutti_ump::{MidiChannel, NoteNumber, NoteVelocity};
use tutti_units::Amplitude;

const BLOCK: usize = 128;

/// Silent until an event reaches its event input, then the constant `level` on
/// both outputs: the level is structure, so another one swaps the node.
#[derive(Clone)]
pub struct Hold {
    level: u32,
    heard: bool,
    cells: HoldCells,
}

declare_params!([] Hold, HoldConfig, HoldCells {
    options { level: u32 = 1 }
});

impl From<HoldConfig> for Hold {
    fn from(config: HoldConfig) -> Self {
        Self {
            level: config.level,
            heard: false,
            cells: HoldCells::from_config(&config),
        }
    }
}

impl Node for Hold {
    fn shape(&self) -> Shape {
        Shape::source(ChannelTopology::STEREO).events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let port = Shape::source(ChannelTopology::STEREO)
            .events(1, 0)
            .event_input(0)
            .expect("one event input");
        self.heard |= !io.events(port).as_slice().is_empty();
        let level = if self.heard { self.level as f32 } else { 0.0 };
        io.output(0).fill(level);
        io.output(1).fill(level);
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl InstrumentNode<f32> for Hold {}

fn session() -> Session {
    Session::builder().build().expect("builds")
}

fn hold() -> HoldConfig {
    HoldConfig::new()
}

/// Renders `blocks` blocks and returns the last frame of the last one.
fn render(s: &mut Session, blocks: usize) -> [f32; 2] {
    let mut block = vec![f32::NAN; BLOCK * 2];
    for _ in 0..blocks {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        let _ = s.update();
    }
    [block[BLOCK * 2 - 2], block[BLOCK * 2 - 1]]
}

fn play(keys: &mut MidiKeys) -> Result<(), PushError> {
    keys.note_on(
        Stamp::NextBlock,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        NoteVelocity::new(0xC000),
    )
}

/// The instrument `"i"`, its strip wired to the main output.
fn instrument(
    gain: f64,
    config: HoldConfig,
) -> impl tutti_session::IntoPatch<&'static str, tutti_session::Graph> {
    let i = InstrumentDecl::new("i", config).initial_gain(Amplitude::new(gain));
    let wire = (&i).connect(MainOutput).unfaded();
    (i, wire)
}

/// **A note sent through the lent keys plays the declared instrument**: its
/// output is the node's constant at the strip's unity gain, and silence before
/// the note.
#[test]
fn a_declared_instrument_plays_a_note_sent_through_its_keys() {
    let mut s = session();
    s.patch(instrument(1.0, hold())).unwrap();
    let mut keys = s.take_keys(&"i").unwrap();
    assert_eq!(render(&mut s, 3), [0.0, 0.0], "nothing is played yet");

    play(&mut keys).unwrap();

    assert_eq!(render(&mut s, 3), [1.0, 1.0]);
}

/// **The strip's initial gain scales the node's output, and a patch that
/// declares another writes it** to the same strip: the key and the node stay.
#[test]
fn the_initial_gain_is_the_strips_volume_and_a_new_one_is_written() {
    let mut s = session();
    s.patch(instrument(0.5, hold())).unwrap();
    let node = s.key::<HoldConfig>(&"i").unwrap();
    let mut keys = s.take_keys(&"i").unwrap();
    play(&mut keys).unwrap();
    assert_eq!(render(&mut s, 3), [0.5, 0.5]);

    let patched = s.patch(instrument(0.25, hold())).unwrap();

    assert!(patched.rebuilt.is_empty() && patched.readded.is_empty());
    assert_eq!(s.key::<HoldConfig>(&"i").unwrap(), node);
    assert_eq!(render(&mut s, 3), [0.25, 0.25]);
}

/// **The keys are lent once**: a second take while the app holds them is
/// `Taken`; a key no patch declares is `Undeclared`; a key declared as a node is
/// `NotInstrument`.
#[test]
fn the_keys_are_lent_once_and_only_for_an_instrument() {
    let mut s = session();
    s.patch((instrument(1.0, hold()), NodeDecl::new("n", hold())))
        .unwrap();
    let _keys = s.take_keys(&"i").unwrap();

    assert_eq!(s.take_keys(&"i").err(), Some(LeaseError::Taken("i")));
    assert_eq!(
        s.take_keys(&"nobody").err(),
        Some(LeaseError::Undeclared("nobody"))
    );
    assert_eq!(
        s.take_keys(&"n").err(),
        Some(LeaseError::NotInstrument("n"))
    );
}

/// **Keys the app dropped are replaced by a lease that plays**: the queue is a
/// new one, wired as the old was.
#[test]
fn taking_again_after_the_keys_are_dropped_lends_keys_that_play() {
    let mut s = session();
    s.patch(instrument(1.0, hold())).unwrap();
    drop(s.take_keys(&"i").unwrap());
    let mut keys = s.take_keys(&"i").expect("the first keys are gone");
    assert_eq!(s.take_keys(&"i").err(), Some(LeaseError::Taken("i")));

    play(&mut keys).unwrap();

    assert_eq!(render(&mut s, 3), [1.0, 1.0]);
}

/// **A swap of the instrument's node keeps its parts and its lease**: keys
/// taken before it play the new node (its level is 2), and the lease is still
/// out.
#[test]
fn a_swap_keeps_the_lease_and_the_keys_play_the_new_node() {
    let mut s = session();
    s.patch(instrument(1.0, hold())).unwrap();
    let mut keys = s.take_keys(&"i").unwrap();

    let patched = s.patch(instrument(1.0, hold().level(2))).unwrap();
    assert_eq!(patched.rebuilt, ["i"]);
    render(&mut s, 4);
    play(&mut keys).unwrap();

    assert_eq!(render(&mut s, 4), [2.0, 2.0]);
    assert_eq!(s.take_keys(&"i").err(), Some(LeaseError::Taken("i")));
}

/// **A patch without the key takes the node, the queue and the strip away**:
/// the graph holds what it did before, the key is undeclared, and a push
/// through the keys the app held has nowhere to go.
#[test]
fn a_removed_instrument_takes_its_parts_and_disconnects_its_keys() {
    let mut s = session();
    let before = s.graph_ref().node_count();
    s.patch(instrument(1.0, hold())).unwrap();
    let mut keys = s.take_keys(&"i").unwrap();
    assert_eq!(s.graph_ref().node_count(), before + 3);
    play(&mut keys).unwrap();

    let patched = s.patch(Vec::<NodeDecl<&str, HoldConfig>>::new()).unwrap();
    render(&mut s, 8);

    assert_eq!(patched.removed, ["i"]);
    assert_eq!(s.graph_ref().node_count(), before);
    assert_eq!(s.key::<HoldConfig>(&"i"), Err(KeyError::Undeclared("i")));
    assert_eq!(s.take_keys(&"i").err(), Some(LeaseError::Undeclared("i")));
    assert_eq!(play(&mut keys), Err(PushError::Disconnected));
}

/// **A key that changes between a node and an instrument is re-added each
/// way**, and no part is left behind: three nodes as an instrument, one as a
/// node.
#[test]
fn a_key_changing_kind_is_readded_and_leaves_no_orphan_parts() {
    let mut s = session();
    let before = s.graph_ref().node_count();
    s.patch(NodeDecl::new("i", hold())).unwrap();
    assert_eq!(
        s.take_keys(&"i").err(),
        Some(LeaseError::NotInstrument("i"))
    );

    let patched = s.patch(InstrumentDecl::new("i", hold())).unwrap();
    render(&mut s, 8);
    assert_eq!(patched.readded, ["i"]);
    assert_eq!(s.graph_ref().node_count(), before + 3);
    assert!(s.take_keys(&"i").is_ok());

    let patched = s.patch(NodeDecl::new("i", hold())).unwrap();
    render(&mut s, 8);
    assert_eq!(patched.readded, ["i"]);
    assert_eq!(s.graph_ref().node_count(), before + 1);
    assert_eq!(
        s.take_keys(&"i").err(),
        Some(LeaseError::NotInstrument("i"))
    );
}

/// **An instrument's audio is not an end of a wire**: a main bus wired into its
/// key is refused with the named cause, and nothing changes.
#[test]
fn a_wire_into_an_instruments_audio_is_refused_at_the_check() {
    let mut s = session();
    let before = s.graph_ref().node_count();
    let src = NodeDecl::new("src", hold());
    let wire = (&src).connect(Mention::new("i"));

    let refused = s.patch((InstrumentDecl::new("i", hold()), src, wire));

    match refused {
        Err(PatchError::Wire {
            ends: WireEnds::Main("src", Some("i")),
            cause: ConnectError::NotAnInput(_),
        }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(s.graph_ref().node_count(), before, "nothing was added");
}
