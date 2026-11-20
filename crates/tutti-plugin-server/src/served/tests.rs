//! `Server<L>` against each format's reference plugin.

use super::*;
use std::path::Path;
use tutti_buffers::{Float, FloatWidth};
use tutti_plugin_types::{PluginFormat, PluginIo, PluginRef, ProcessContext, Processor};

#[cfg(served_au)]
mod au;
#[cfg(feature = "clap")]
mod clap;
#[cfg(feature = "vst3")]
mod vst3;

impl<L: ServerSide> Server<L> {
    /// The plugin a file's first, at `rate`, for blocks of up to `block`, with no
    /// state to restore: the shortest load a test needs.
    fn load_first(
        main: MainThreadMarker,
        path: &Path,
        rate: f64,
        block: usize,
        requested: FloatWidth,
    ) -> PluginResult<Self> {
        Self::load(
            main,
            &tutti_fixtures::detached_work(),
            &PluginRef::file(path),
            SampleRate::of(rate),
            block,
            requested,
            None,
            &mut || {},
            Hand::local(),
            crate::ties::ties().expect("bells").0,
        )
        .map(|(served, _)| served)
    }
}

/// A plugin loaded on a [`Hand::local`] renders in place: the Processor is
/// this value's, as a test that calls the plugin needs it (production renders
/// through [`crate::render::End`], which holds the Processor on its thread).
impl<L: ServerSide> Render for Server<L> {
    fn features(&self) -> Features {
        self.wiring.features
    }

    fn render_f32(&mut self, io: &mut PluginIo<'_, f32>) -> PluginResult<()> {
        self.render_here(io)
    }

    fn render_f64(&mut self, io: &mut PluginIo<'_, f64>) -> PluginResult<()> {
        self.render_here(io)
    }
}

impl<L: ServerSide> Server<L> {
    /// One block at either width, in place: silence when the plugin is not
    /// running.
    fn render_here<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
        // A rate change the plugin refused left it deactivated: silence.
        let Some(processor) = self.lifecycle.processor() else {
            io.out.clear();
            io.buffer.output().silence();
            return Ok(());
        };
        processor.process(io)
    }
}

impl<L: ServerSide> Server<L> {
    /// A parameter's value, which the plugin must have.
    fn normalized(&self, main: MainThreadMarker, id: ParamId) -> f64 {
        self.parameter(main, id)
            .expect("the plugin has the parameter")
            .get()
    }
}

use tutti_fixtures::main_thread;

/// Loads `path` through the table as `format`, with everything else defaulted.
fn load_any(path: &Path, format: PluginFormat) -> PluginResult<AnyServer> {
    AnyServer::load(
        main_thread(),
        &tutti_fixtures::detached_work(),
        &PluginRef::file(path),
        format,
        SampleRate::SR_44K1,
        512,
        FloatWidth::F32,
        None,
        &mut || {},
        crate::ties::ties().expect("bells").0,
    )
    .map(|(plugin, _, _)| plugin)
}

fn assert_refused(result: PluginResult<AnyServer>, at: LoadStage, reason: &str) {
    match result {
        Err(PluginError::Load { stage, reason: r }) => {
            assert_eq!(stage, at, "{r}");
            assert!(r.contains(reason), "{r:?} should contain {reason:?}");
        }
        Err(e) => panic!("expected a load refused at {at}, got {e:?}"),
        Ok(_) => panic!("expected a load refused at {at}, got a plugin"),
    }
}

/// **A path that is not there is refused at the scan**, before any format is
/// asked.
#[test]
fn load_missing_path_errors() {
    assert_refused(
        load_any(Path::new("/nonexistent/plugin.vst3"), PluginFormat::Vst3),
        LoadStage::Scanning,
        "does not exist",
    );
}

/// **VST2 is not served**: its `AEffect` fuses editor and audio, so a load as
/// VST2 reaches no format of this server, whatever the file's extension
/// (`.vst`, `.dll`, `.so`). Adding a `Vst2` row to the table is a compile error
/// (`Vst2Loaded` is not `OutOfProcess`); this pins that the dispatch does not
/// quietly take another route.
#[test]
fn a_vst2_load_is_refused() {
    for extension in ["vst", "dll", "so"] {
        let tmp =
            std::env::temp_dir().join(format!("fake_vst2_{}.{extension}", std::process::id()));
        std::fs::write(&tmp, b"fake").unwrap();
        let result = load_any(&tmp, PluginFormat::Vst2);
        let _ = std::fs::remove_file(&tmp);
        assert_refused(result, LoadStage::Opening, "unsupported plugin format");
    }
}

/// **The format a load names opens the file, not the file's extension**: the
/// reference plugin's library, whose `.so` names no row of the table, opens as
/// CLAP when the load says CLAP.
#[cfg(feature = "clap")]
#[test]
fn a_load_opens_the_file_as_the_format_it_names() {
    let path = Path::new(crate::test_utils::clap_probe_path());
    assert_ne!(
        path.extension().and_then(|e| e.to_str()),
        Some("clap"),
        "the premise: the library does not carry the format's extension"
    );
    let plugin = load_any(path, PluginFormat::Clap).expect("a CLAP plugin under another name");
    assert!(matches!(plugin, AnyServer::Clap(_)));
}

/// A plugin reads as lost once a call into it panicked, and not before: what
/// the server loop ends on.
fn assert_poison_reads_as_lost(mut plugin: AnyServer) {
    assert!(!plugin.plugin().is_lost());
    plugin.poison();
    assert!(plugin.plugin().is_lost());
}

/// **A poisoned CLAP plugin reads as lost, and a fresh one does not.**
#[cfg(feature = "clap")]
#[test]
fn a_poisoned_clap_plugin_reads_as_lost_and_a_fresh_one_does_not() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = Path::new(crate::test_utils::clap_probe_path());
    assert_poison_reads_as_lost(load_any(path, PluginFormat::Clap).expect("load"));
}

/// **A poisoned VST3 plugin reads as lost, and a fresh one does not.**
#[cfg(feature = "vst3")]
#[test]
fn a_poisoned_vst3_plugin_reads_as_lost_and_a_fresh_one_does_not() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = Path::new(crate::test_utils::vst3_probe_path());
    assert_poison_reads_as_lost(load_any(path, PluginFormat::Vst3).expect("load"));
}
