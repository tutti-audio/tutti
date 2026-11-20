//! What a plugin crate's tests share (feature `plugin`).

use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::{cdylib, main_thread, profile_dir};
use tutti_platform::MainThreadMarker;
use tutti_plugin_types::{Loaded, MainWork, MainWorkHandle, PluginError, Prepare, WindowHandle};

/// The constant 120 BPM a plugin node's tests render under.
///
/// # Panics
///
/// Panics when the tempo reader cannot be built, which a constant 120 BPM never is.
pub fn tempo() -> tutti_timeline::TempoReader {
    tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo")
}

/// A [`MainWorkHandle`] whose collector is gone: for a test that loads a plugin
/// and does not look at what an off-thread drop hands over. A close sent to it
/// runs at once on the main thread and is otherwise counted in
/// [`MainWork::stranded`]; a test that checks the hand-over makes its own
/// [`MainWork::new`].
pub fn detached_work() -> MainWorkHandle {
    MainWork::new().1
}

/// A thread that runs a [`MainWork`] as the main thread's tick does, until it is
/// dropped: for a test that calls a plugin handle from its own thread, which is
/// off the main thread, and needs something to answer the hop.
#[derive(Debug)]
pub struct Pump {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Runs `work` on a new thread, every millisecond, with a token of its own. It
/// has run once before this returns, so the collector already counts as pumped.
pub fn pump(work: MainWork) -> Pump {
    work.run(main_thread());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = std::sync::Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        let main = main_thread();
        while !seen.load(std::sync::atomic::Ordering::Acquire) {
            work.run(main);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        work.run(main);
    });
    Pump {
        stop,
        thread: Some(thread),
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A [`MainWorkHandle`] whose collector a thread of this process runs for the
/// rest of its life: for a test that calls a plugin handle from its own thread
/// (off the main thread, so each call hops to the collector) and ends with its
/// process. A test that stops the pump, or looks at what was handed over, makes
/// its own with [`MainWork::new`] and [`pump`].
pub fn pumped_work() -> MainWorkHandle {
    let (work, handle) = MainWork::new();
    std::mem::forget(pump(work));
    handle
}

/// A window that names no window, for a test whose plugin, or whose host path,
/// refuses or ignores its parent before embedding: a probe that never touches
/// it, a host that must reject an editorless plugin first, a plugin that cannot
/// be loaded. Pass it where an API takes `impl HasWindowHandle`, or its
/// [`handle`](Self::handle) where one takes a [`WindowHandle`].
///
/// Handing it to a plugin that does embed is a crash in that plugin, which is
/// why the suites reach a stand-in only here.
#[derive(Debug, Clone, Copy)]
pub struct StandInWindow;

impl StandInWindow {
    /// The stand-in as a plugin's parent [`WindowHandle`].
    ///
    /// # Panics
    ///
    /// Panics when the platform names no handle for the stand-in, which every
    /// platform does.
    pub fn handle(self) -> WindowHandle {
        WindowHandle::from_window(&self).expect("an AppKit handle is one every platform names")
    }
}

impl raw_window_handle::HasWindowHandle for StandInWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let view = std::ptr::NonNull::<std::ffi::c_void>::dangling();
        let raw = raw_window_handle::RawWindowHandle::AppKit(
            raw_window_handle::AppKitWindowHandle::new(view),
        );
        #[allow(unsafe_code, reason = "a test's stand-in window; see the type")]
        // SAFETY: `borrow_raw` wants a window valid for the borrow. This one is
        // not, which the type's callers allow: their path fails or ignores
        // the parent before it embeds.
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
}

/// How one format makes and unmakes an activation's two halves: the part of
/// [`Active`] that differs between the formats' hosts. [`Whole`] is the
/// adapter for any [`Loaded`]; a host whose activation is typed (VST3's
/// `activate_typed`) writes its own.
pub trait Activation {
    /// The plugin, loaded and not rendering.
    type Loaded;
    /// The realtime half.
    type Processor;
    /// The main-thread half.
    type Controller;

    /// Splits `loaded`; a refusal hands the plugin back when the format can.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    #[allow(clippy::type_complexity, clippy::result_large_err)]
    fn activate(
        main: MainThreadMarker,
        loaded: Self::Loaded,
        prepare: Prepare,
    ) -> Result<(Self::Processor, Self::Controller), (Option<Self::Loaded>, PluginError)>;

    /// Rejoins the halves.
    ///
    /// # Errors
    ///
    /// Why the halves would not rejoin.
    fn deactivate(
        main: MainThreadMarker,
        processor: Self::Processor,
        controller: Self::Controller,
    ) -> Result<Self::Loaded, PluginError>;
}

/// The [`Activation`] of a [`Loaded`] plugin, through its own `activate` and
/// `deactivate`.
#[derive(Debug)]
pub struct Whole<L>(PhantomData<fn() -> L>);

impl<L: Loaded> Activation for Whole<L> {
    type Loaded = L;
    type Processor = L::Processor;
    type Controller = L::Controller;

    fn activate(
        main: MainThreadMarker,
        loaded: L,
        prepare: Prepare,
    ) -> Result<(L::Processor, L::Controller), (Option<L>, PluginError)> {
        loaded
            .activate(main, prepare)
            .map_err(|refused| (refused.plugin, refused.reason))
    }

    fn deactivate(
        main: MainThreadMarker,
        processor: L::Processor,
        controller: L::Controller,
    ) -> Result<L, PluginError> {
        L::deactivate(main, processor, controller).map_err(|stuck| stuck.reason)
    }
}

/// An activation's two halves held together, for the suites that drive one
/// plugin from a single thread.
///
/// The Processor and the Controller are separate values on purpose; most of
/// these suites want "the active plugin", so `Active` holds the pair and derefs
/// to the Controller. It does not deref to the loaded plugin, so a call meant
/// for the Controller cannot quietly skip what the Controller tells the
/// Processor. A format's own accessors are the host's tests' extension traits
/// over this type, not part of it.
pub struct Active<A: Activation> {
    pair: Option<(A::Processor, A::Controller)>,
    prepare: Prepare,
}

impl<A: Activation> fmt::Debug for Active<A>
where
    A::Processor: fmt::Debug,
    A::Controller: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Active")
            .field("pair", &self.pair)
            .field("prepare", &self.prepare)
            .finish()
    }
}

impl<A: Activation> Active<A> {
    /// Activates `loaded` at `prepare`; a refusal hands the plugin back when
    /// the format can.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    #[allow(clippy::result_large_err)]
    pub fn activate(
        loaded: A::Loaded,
        prepare: Prepare,
    ) -> Result<Self, (Option<A::Loaded>, PluginError)> {
        A::activate(main_thread(), loaded, prepare).map(|pair| Self {
            pair: Some(pair),
            prepare,
        })
    }

    /// The configuration the plugin was last activated at.
    pub fn prepare(&self) -> Prepare {
        self.prepare
    }

    fn halves(&self) -> &(A::Processor, A::Controller) {
        self.pair.as_ref().expect("an Active holds its pair")
    }

    fn halves_mut(&mut self) -> &mut (A::Processor, A::Controller) {
        self.pair.as_mut().expect("an Active holds its pair")
    }

    /// The realtime half.
    pub fn processor(&mut self) -> &mut A::Processor {
        &mut self.halves_mut().0
    }

    /// The realtime half, shared: for a counter or a flag read.
    pub fn processor_shared(&self) -> &A::Processor {
        &self.halves().0
    }

    /// The main-thread half.
    pub fn controller(&self) -> &A::Controller {
        &self.halves().1
    }

    /// The main-thread half, mutably.
    pub fn controller_mut(&mut self) -> &mut A::Controller {
        &mut self.halves_mut().1
    }

    /// Ends the activation: the plugin comes back deactivated.
    ///
    /// # Panics
    ///
    /// When the halves will not rejoin.
    #[must_use]
    pub fn deactivate(mut self) -> A::Loaded {
        let (processor, controller) = self.pair.take().expect("an Active holds its pair");
        A::deactivate(main_thread(), processor, controller)
            .unwrap_or_else(|reason| panic!("the halves would not rejoin: {reason}"))
    }

    /// Deactivates and activates again at `wanted`, as a rate or block change
    /// does. A refusal activates again at the configuration it had, and is
    /// returned.
    ///
    /// # Errors
    ///
    /// The plugin's refusal of `wanted`; the activation is as it was.
    ///
    /// # Panics
    ///
    /// When the halves will not rejoin, or the plugin neither takes `wanted`
    /// nor hands itself back, nor takes the configuration it had.
    pub fn reconfigure(&mut self, wanted: Prepare) -> Result<(), PluginError> {
        let main = main_thread();
        let (processor, controller) = self.pair.take().expect("an Active holds its pair");
        let loaded = A::deactivate(main, processor, controller)
            .unwrap_or_else(|reason| panic!("the halves would not rejoin: {reason}"));
        match A::activate(main, loaded, wanted) {
            Ok(pair) => {
                self.pair = Some(pair);
                self.prepare = wanted;
                Ok(())
            }
            Err((plugin, reason)) => {
                let plugin = plugin.expect("the format hands the plugin back");
                match A::activate(main, plugin, self.prepare) {
                    Ok(pair) => self.pair = Some(pair),
                    Err((_, again)) => {
                        panic!("the plugin refused the configuration it had: {again}")
                    }
                }
                Err(reason)
            }
        }
    }
}

impl<A: Activation> Deref for Active<A> {
    type Target = A::Controller;

    fn deref(&self) -> &A::Controller {
        self.controller()
    }
}

impl<A: Activation> DerefMut for Active<A> {
    fn deref_mut(&mut self) -> &mut A::Controller {
        self.controller_mut()
    }
}

/// The shared plugin's library as the module of a `.vst3` bundle,
/// `<name>.vst3/Contents/<arch>/<module>`, the layout the VST3 host walks.
///
/// The bundle is `<profile>/vst3-bundles/<key>/`, keyed by the library's
/// modified time and length. It is built in a directory of this process's own
/// and published by renaming that directory to `<key>`, which fails where one
/// is already there: the first process to publish wins, and no process changes
/// a bundle once it is visible, so no process loads a path another is making.
/// The module is a hard link (a copy where linking fails), so a rebuild that
/// replaces the library leaves a published bundle as it was.
///
/// # Panics
///
/// If the bundle cannot be made, or the shared plugin has not been built
/// ([`cdylib`]).
#[must_use]
pub fn vst3_module() -> PathBuf {
    static MODULE: OnceLock<PathBuf> = OnceLock::new();
    MODULE
        .get_or_init(|| {
            let real = cdylib("tutti_shared_plugin", "tutti-shared-plugin");
            let meta = std::fs::metadata(&real).expect("the library's metadata");
            let nanos = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            let root = profile_dir().join("vst3-bundles");
            let name = real.file_name().expect("file name");
            let inside = Path::new(&format!("{nanos}-{}", meta.len()))
                .join(Path::new(name).with_extension("vst3"))
                .join("Contents")
                .join(tutti_plugin_types::bundle::native_arch_subdirs()[0])
                .join(name);
            let published = root.join(&inside);
            if published.exists() {
                return published;
            }
            let key = inside.components().next().expect("a key").as_os_str();
            let tmp = root.join(format!(".tmp-{}", std::process::id()));
            let module = tmp.join(inside.components().skip(1).collect::<PathBuf>());
            // A directory of an earlier process of this id is not in use.
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(module.parent().expect("module directory"))
                .expect("bundle directory");
            if std::fs::hard_link(&real, &module).is_err() {
                std::fs::copy(&real, &module).expect("copy the bundle module");
            }
            let renamed = std::fs::rename(&tmp, root.join(key));
            if renamed.is_err() {
                // Another process published first; its bundle is whole.
                let _ = std::fs::remove_dir_all(&tmp);
            }
            assert!(
                published.exists(),
                "no module at {} (publishing the bundle: {renamed:?})",
                published.display()
            );
            published
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::{MaxBlock, SampleRate, Samples};

    /// A plugin that takes rates up to 50 kHz and hands itself back on refusal.
    struct Fake;

    #[derive(Debug, PartialEq)]
    struct Plugin;
    #[derive(Debug)]
    struct Half(f64);
    #[derive(Debug)]
    struct Other(f64);

    impl Activation for Fake {
        type Loaded = Plugin;
        type Processor = Half;
        type Controller = Other;

        fn activate(
            _: MainThreadMarker,
            _: Plugin,
            prepare: Prepare,
        ) -> Result<(Half, Other), (Option<Plugin>, PluginError)> {
            let rate = prepare.rate().get();
            if rate > 50_000.0 {
                return Err((Some(Plugin), PluginError::Process(format!("no {rate} Hz"))));
            }
            Ok((Half(rate), Other(rate)))
        }

        fn deactivate(_: MainThreadMarker, _: Half, _: Other) -> Result<Plugin, PluginError> {
            Ok(Plugin)
        }
    }

    fn at(rate: f64) -> Prepare {
        let block = MaxBlock::new(Samples::new(64)).expect("a block");
        Prepare::new(SampleRate::of(rate), block)
    }

    /// A refused `reconfigure` reports the refusal and leaves the activation
    /// as it was: the previous configuration, both halves live.
    #[test]
    fn a_refused_reconfigure_keeps_the_previous_activation() {
        let mut active = Active::<Fake>::activate(Plugin, at(48_000.0)).expect("activates");
        assert!(active.reconfigure(at(96_000.0)).is_err());
        assert_eq!(active.prepare().rate().get(), 48_000.0);
        assert_eq!(active.processor().0, 48_000.0);
        assert_eq!(active.controller().0, 48_000.0);

        active.reconfigure(at(44_100.0)).expect("accepted");
        assert_eq!(active.prepare().rate().get(), 44_100.0);
        assert_eq!(active.processor().0, 44_100.0);
        assert_eq!(active.deactivate(), Plugin);
    }
}
