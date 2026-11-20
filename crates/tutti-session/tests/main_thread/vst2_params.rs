//! `Param::try_unit` over a real format's own value map: the
//! reference VST2 plugin, in process. Its parameter 1 is labelled `Hz` and
//! declares the integer range `0..=127`, which VST2 maps linearly
//! (`tutti-vst2-host`'s `IntegerRange`), so `Hz::new(31.75)` is position `0.25`
//! (the figures are the range's, written here, not read from the host).

use crate::pooled::{LoadPlugin, Spec};
use tutti_buffers::ChannelLayout;
use tutti_graph::NodeKey;
use tutti_plugin::catalog::PluginRef;
use tutti_plugin_types::ParamUnit;
use tutti_session::{
    DetachedSession, Root, Session, SetRefusal, UnitError, ValueError, ValueErrorKind,
};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{Db, Hz, SampleRate};

/// The probe's parameter that is labelled `Hz` and has an integer range.
const HERTZ: ParamId = ParamId::new(1);

/// The probe's image, opened here and held, with its parameter-properties
/// switch on: the host's own load of the same file shares the image, so its
/// first read of the parameters sees the integer range (a default VST2 plugin,
/// like every real one measured, declines the opcode).
fn probe_answering_properties() -> (libloading::Library, std::path::PathBuf) {
    let path = tutti_fixtures::vst2_probe();
    // SAFETY: the probe is our own test plugin, whose load runs no constructor.
    let lib = unsafe { libloading::Library::new(path) }.expect("the probe image opens");
    // SAFETY: the probe exports this as `extern "C" fn(bool)`.
    unsafe {
        let switch: libloading::Symbol<extern "C" fn(bool)> = lib
            .get(b"tutti_vst2_probe_set_answer_param_properties\0")
            .expect("the probe exports its switch");
        switch(true);
    }
    (lib, path.to_path_buf())
}

fn session_with_probe() -> (DetachedSession, NodeKey, libloading::Library) {
    let (image, probe) = probe_answering_properties();
    let (mut s, mut background) = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    background
        .spawn_threads(std::num::NonZeroUsize::MIN)
        .expect("a background thread starts");
    let node = s
        .load_plugin_blocking(
            tutti_fixtures::main_thread(),
            Spec::new(PluginRef::file(probe)),
        )
        .expect("the in-process VST2 loads");
    s.graph().connect(node, Root).unwrap();
    let _ = s.update();
    (s, node.erase(), image)
}

/// **A unit target round-trips plain values through VST2's own map**, and
/// refuses the wrong unit on the same parameter: `Db` on a parameter that
/// measures hertz.
fn a_typed_hertz_parameter_round_trips_through_the_formats_own_range() {
    let (s, node, _image) = session_with_probe();
    let param = s.plugin_view(node).unwrap().params().get(HERTZ).unwrap();
    assert_eq!(
        param.info().measures(),
        Some(ParamUnit::Hertz),
        "the label and the declared range type it"
    );
    assert!(matches!(
        param.try_unit::<Db>(),
        Err(UnitError::UnitMismatch {
            declared: ParamUnit::Hertz,
            ..
        })
    ));

    let hertz = param.try_unit::<Hz>().unwrap();
    let position = param.target();
    for (plain, at) in [(0.0, 0.0), (31.75, 0.25), (63.5, 0.5), (127.0, 1.0)] {
        s.set(hertz.to(Hz::new(plain))).unwrap();
        assert_eq!(
            s.get(position).unwrap(),
            Normalized::of(at),
            "{plain} Hz is position {at}"
        );
        assert_eq!(
            s.get(hertz).unwrap(),
            Hz::new(plain),
            "and reads back as {plain} Hz"
        );
    }
    assert!(
        matches!(
            s.set(hertz.to(Hz::new(128.0))),
            Err(ValueError {
                kind: ValueErrorKind::Refused(SetRefusal::OutOfRange),
                ..
            })
        ),
        "past the declared range"
    );
}

/// The cases of this module, run by `main`.
pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["vst2_params";
a_typed_hertz_parameter_round_trips_through_the_formats_own_range,
];
