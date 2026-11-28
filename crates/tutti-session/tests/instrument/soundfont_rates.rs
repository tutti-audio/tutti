//! The SoundFont unit declares the rates it runs at, and a session graph holds it to them.

use std::sync::Arc;
use tutti_buffers::ChannelLayout;
use tutti_session::{Session, SetRateError};
use tutti_sf2::SoundFont;
use tutti_soundfont::{SoundFontConfig, SoundFontUnit};
use tutti_units::SampleRate;

/// **A re-rate outside 16 to 192 kHz is refused by name and changes nothing; one
/// inside is taken.**
#[test]
fn a_session_refuses_a_rate_the_soundfont_unit_does_not_run_at() {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    let font = Arc::new(SoundFont::open(path.as_path()).expect("the test font parses"));
    let unit = SoundFontUnit::<f32>::from(
        SoundFontConfig::new(font).expect("a font with a playable preset"),
    );
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let key = s.graph().add(unit);
    let supported =
        tutti_graph::SampleRates::of(SampleRate::of(16_000.0), SampleRate::of(192_000.0));

    for rate in [SampleRate::of(8_000.0), SampleRate::of(384_000.0)] {
        assert_eq!(
            s.set_sample_rate(rate).unwrap_err(),
            SetRateError::RateUnsupported {
                node: key.erase(),
                supported,
                rate,
            }
        );
        assert_eq!(s.sample_rate(), SampleRate::SR_48K);
    }
    s.set_sample_rate(SampleRate::SR_44K1).unwrap();
    assert_eq!(s.sample_rate(), SampleRate::SR_44K1);
}
