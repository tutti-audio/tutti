//! The instrument node in a graph: with no event it renders silence, in stereo.

use tutti_buffers::MaxBlock;
use tutti_graph::{Prepare, Renderer};
use tutti_polysynth::{InstrumentConfig, InstrumentVoice, NoteInstrument};
use tutti_units::{SampleRate, Samples};

use crate::support::instruments::{each_voice, poly};

#[test]
fn an_instrument_with_no_events_renders_stereo_silence() {
    fn check<V: InstrumentVoice<Float = f32>>(name: &str, config: V::Config) {
        let instrument = NoteInstrument::<V>::from(InstrumentConfig::new(config, poly(4)));
        let prepare = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(256)));
        let (mut renderer, ..) = Renderer::alone(instrument, prepare);
        let out = renderer.render(1024);
        assert_eq!(out.len(), 2, "{name}: stereo");
        assert!(out.iter().flatten().all(|&s| s == 0.0), "{name}: silence");
    }
    each_voice!(check);
}
