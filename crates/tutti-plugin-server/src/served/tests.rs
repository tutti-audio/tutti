//! `Served<F>` against each format's reference plugin.

use super::*;
use tutti_plugin_types::PluginFormat;

#[cfg(served_au)]
mod au;
#[cfg(feature = "clap")]
mod clap;
#[cfg(feature = "vst3")]
mod vst3;

impl<F: ServerSide> Served<F> {
    /// The plugin a file's first, at `rate`, for blocks of up to `block`, with no
    /// state to restore: the shortest load a test needs.
    fn load_first(
        main: MainThreadMarker,
        path: &Path,
        rate: f64,
        block: usize,
        prefer_f64: bool,
    ) -> PluginResult<Self> {
        Self::load(
            main,
            &tutti_fixtures::detached_work(),
            path,
            &Load::new(),
            SampleRate::of(rate),
            block,
            prefer_f64,
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
impl<F: ServerSide> Render for Served<F> {
    fn features(&self) -> Features {
        self.wiring.features
    }

    fn render<'a>(
        &mut self,
        buffer: PluginAudio<'a>,
        ctx: &'a ProcessContext<'a>,
        out: &'a mut ProcessOutput,
    ) -> PluginResult<()> {
        // A rate change the plugin refused left it deactivated: silence.
        let Some(processor) = self.lifecycle.processor() else {
            out.clear();
            let mut buffer = buffer;
            buffer.clear_outputs();
            return Ok(());
        };
        buffer.process(processor, ctx, out)
    }
}

/// Leaves the loaded CLAP plugin as a panic in a call into it does
/// (`Lifecycle::guarded` replaces the plugin with `Poisoned`).
#[cfg(feature = "clap")]
pub(crate) fn lose(plugin: &mut AnyServed) {
    let AnyServed::Clap(served) = plugin else {
        panic!("a CLAP plugin");
    };
    served.lifecycle = Lifecycle::Poisoned;
}

impl<F: ServerSide> Served<F> {
    /// A parameter's value, which the plugin must have.
    fn normalized(&self, main: MainThreadMarker, id: ParamId) -> f64 {
        self.parameter(main, id)
            .expect("the plugin has the parameter")
            .get()
    }
}

use tutti_fixtures::main_thread;

/// Loads `path` through the table as `format`, with everything else defaulted.
fn load_any(path: &Path, format: PluginFormat) -> PluginResult<AnyServed> {
    AnyServed::load(
        main_thread(),
        &tutti_fixtures::detached_work(),
        path,
        format,
        &Load::new(),
        SampleRate::SR_44K1,
        512,
        false,
        None,
        &mut || {},
        crate::ties::ties().expect("bells").0,
    )
    .map(|(plugin, _, _)| plugin)
}

fn assert_refused(result: PluginResult<AnyServed>, at: LoadStage, reason: &str) {
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
    assert!(matches!(plugin, AnyServed::Clap(_)));
}
