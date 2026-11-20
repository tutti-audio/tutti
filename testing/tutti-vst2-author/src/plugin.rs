//! The plugin side of the VST2 API: the [`Plugin`] and [`PluginParameters`] traits a test
//! plugin implements and the [`HostCallback`] it calls the host through.

use std::os::raw::c_void;
use std::ptr;
use std::sync::Arc;

use tutti_plugin_types::read_c_str;
use tutti_vst2_ffi::{AudioMasterOpCode, Host, SpeakerArrangement};

pub use tutti_vst2_ffi::{CanDo, Category, EffectOpCode};

use crate::api::{
    self, AEffect, GetChunkError, HostCallbackProc, Supported, TimeInfo, consts::VST_MAGIC,
};

use crate::{buffer::AudioBuffer, channels::ChannelInfo, editor::Editor};

/// The static information a plugin declares: what `main` builds its `AEffect` from.
#[derive(Clone, Debug)]
pub struct Info {
    /// Plugin Name.
    pub name: String,

    /// Plugin Vendor.
    pub vendor: String,

    /// Number of different presets.
    pub presets: i32,

    /// Number of parameters.
    pub parameters: i32,

    /// Number of inputs.
    pub inputs: i32,

    /// Number of outputs.
    pub outputs: i32,

    /// Number of MIDI input channels (1-16), or 0 for the default of 16 channels.
    pub midi_inputs: i32,

    /// Number of MIDI output channels (1-16), or 0 for the default of 16 channels.
    pub midi_outputs: i32,

    /// Unique plugin ID. Can be registered with Steinberg to prevent conflicts with other plugins.
    ///
    /// This ID is used to identify a plugin during save and load of a preset and project.
    pub unique_id: i32,

    /// Plugin version (e.g. 0001 = `v0.0.0.1`, 1283 = `v1.2.8.3`).
    pub version: i32,

    /// Plugin category. Possible values are found in `enums::PluginCategory`.
    pub category: Category,

    /// Latency of the plugin in samples.
    ///
    /// This reports how many samples it takes for the plugin to create an output (group delay).
    pub initial_delay: i32,

    /// Indicates that preset data is handled in formatless chunks.
    ///
    /// If false, host saves and restores plugin by reading/writing parameter data. If true, it is
    /// up to the plugin to manage saving preset data by implementing the
    /// `{get, load}_{preset, bank}_chunks()` methods. Default is `false`.
    pub preset_chunks: bool,

    /// Indicates whether this plugin can process f64 based `AudioBuffer` buffers.
    ///
    /// Default is `false`.
    pub f64_precision: bool,

    /// If this is true, the plugin will not produce sound when the input is silence.
    ///
    /// Default is `false`.
    pub silent_when_stopped: bool,
}

impl Default for Info {
    fn default() -> Info {
        Info {
            name: "VST".to_string(),
            vendor: String::new(),

            presets: 1, // default preset
            parameters: 0,
            inputs: 2, // Stereo in,out
            outputs: 2,

            midi_inputs: 0,
            midi_outputs: 0,

            unique_id: 0, // This must be changed.
            version: 1,   // v0.0.0.1

            category: Category::Effect,

            initial_delay: 0,

            preset_chunks: false,
            f64_precision: false,
            silent_when_stopped: false,
        }
    }
}

/// Must be implemented by all VST plugins.
///
/// All methods except `new` and `get_info` provide a default implementation
/// which does nothing and can be safely overridden.
///
/// At any time, a plugin is in one of two states: *suspended* or *resumed*.
/// While a plugin is in the *suspended* state, various processing parameters,
/// such as the sample rate and block size, can be changed by the host, but no
/// audio processing takes place. While a plugin is in the *resumed* state,
/// audio processing methods and parameter access methods can be called by
/// the host. A plugin starts in the *suspended* state and is switched between
/// the states by the host using the `resume` and `suspend` methods.
///
/// Hosts call methods of the plugin on two threads: the UI thread and the
/// processing thread. For this reason, the plugin API is separated into two
/// traits: The `Plugin` trait containing setup and processing methods, and
/// the `PluginParameters` trait containing methods for parameter access.
#[allow(unused_variables)]
pub trait Plugin: Send {
    /// This method must return an `Info` struct.
    fn get_info(&self) -> Info;

    /// Called before the host changes preset, answering `effBeginSetProgram`
    /// (67).
    ///
    /// A preset switch moves many parameters at once. Without this bracket each
    /// one looks to the host like an individual edit, so a plugin that reports
    /// changes through `audioMasterAutomate` floods the host with what reads as
    /// forty separate automation writes rather than one atomic event.
    ///
    /// Defaults to a no-op, which is what an unimplemented opcode already does.
    fn begin_set_preset(&mut self) {}

    /// Called after the host has changed preset, answering `effEndSetProgram`
    /// (68). See [`begin_set_preset`](Self::begin_set_preset).
    fn end_set_preset(&mut self) {}

    /// Enter or leave soft bypass, answering `effSetBypass` (44).
    ///
    /// Return `true` to accept. The default is `false` — declining, which is
    /// what an unimplemented opcode reports anyway, so a plugin that does not
    /// override this behaves exactly as it did before the hook existed.
    ///
    /// A plugin that accepts must also answer `"bypass"` to
    /// [`can_do`](Self::can_do): the advertisement is what a host checks before
    /// relying on soft bypass rather than muting the plugin itself.
    fn set_bypass(&mut self, bypass: bool) -> bool {
        false
    }

    /// Takes the host's proposed speaker arrangements, answering `effSetSpeakerArrangement`
    /// (42). Return `true` to accept both.
    ///
    /// The default is `false`: declining, as an unimplemented opcode does.
    ///
    /// This method is only called while the plugin is in the *suspended* state.
    fn set_speaker_arrangement(
        &mut self,
        input: &SpeakerArrangement,
        output: &SpeakerArrangement,
    ) -> bool {
        false
    }

    /// The input and output arrangements the plugin holds, answering
    /// `effGetSpeakerArrangement` (69). `None` answers 0, "no arrangement to report".
    fn speaker_arrangement(&self) -> Option<(SpeakerArrangement, SpeakerArrangement)> {
        None
    }

    /// The plugin's own name, answering `effGetEffectName` (45).
    ///
    /// Distinct from [`Info::name`], which answers `effGetProductString` (48):
    /// the product string names the *product* a plugin ships in, so every
    /// plugin in a bundled suite shares one. A host that asks only for the
    /// product string sees the suite, not the plugin.
    ///
    /// Defaults to `None`, which leaves the opcode unanswered — the host then
    /// falls back to the product string, which is what it did before this
    /// existed. Override only when the two names genuinely differ.
    fn get_effect_name(&self) -> Option<String> {
        None
    }

    /// Called during initialization to pass a `HostCallback` to the plugin.
    ///
    /// This method can be overridden to set `host` as a field in the plugin struct.
    ///
    /// # Example
    ///
    /// ```
    /// // ...
    /// # use tutti_vst2_author::plugin::{Plugin, Info};
    /// use tutti_vst2_author::plugin::HostCallback;
    ///
    /// struct ExamplePlugin {
    ///     host: HostCallback
    /// }
    ///
    /// impl Plugin for ExamplePlugin {
    ///     fn new(host: HostCallback) -> ExamplePlugin {
    ///         ExamplePlugin {
    ///             host
    ///         }
    ///     }
    ///
    ///     fn init(&mut self) {
    ///         tracing::info!("loaded with host vst version: {}", self.host.vst_version());
    ///     }
    ///
    ///     // ...
    /// #     fn get_info(&self) -> Info {
    /// #         Info {
    /// #             name: "Example Plugin".to_string(),
    /// #             ..Default::default()
    /// #         }
    /// #     }
    /// }
    ///
    /// # fn main() {}
    /// ```
    fn new(host: HostCallback) -> Self
    where
        Self: Sized;

    /// Called when plugin is fully initialized.
    ///
    /// This method is only called while the plugin is in the *suspended* state.
    fn init(&mut self) {
        trace!("Initialized vst plugin.");
    }

    /// Called when sample rate is changed by host.
    ///
    /// This method is only called while the plugin is in the *suspended* state.
    fn set_sample_rate(&mut self, rate: f32) {}

    /// Called when block size is changed by host.
    ///
    /// This method is only called while the plugin is in the *suspended* state.
    fn set_block_size(&mut self, size: i64) {}

    /// Called when the host chooses which render entry point it will use —
    /// `effSetProcessPrecision`. `double` is true for `processReplacingF64`,
    /// false for `processReplacing`.
    ///
    /// Distinct from `effFlagsCanDoubleReplacing`, which says only what the
    /// plugin *can* do: a plugin supporting both widths configures its
    /// internal precision on this call, and one never told stays at whatever
    /// it defaulted to.
    ///
    /// This method is only called while the plugin is in the *suspended* state.
    fn set_precision(&mut self, double: bool) {}

    /// Called to transition the plugin into the *resumed* state.
    fn resume(&mut self) {}

    /// Called to transition the plugin into the *suspended* state.
    fn suspend(&mut self) {}

    /// Vendor specific handling.
    fn vendor_specific(&mut self, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
        0
    }

    /// Return whether plugin supports specified action.
    ///
    /// This method is only called while the plugin is in the *suspended* state.
    fn can_do(&self, can_do: CanDo) -> Supported {
        info!("Host is asking if plugin can: {:?}.", can_do);
        Supported::Maybe
    }

    /// `effShellGetNextPlugin`, for a plugin whose category is `Category::Shell`: the next
    /// sub-plugin's `uniqueID` and name, then `None` (the host reads 0) when none is left. The
    /// host calls it until then. Not a shell: leave it, and the answer is 0.
    fn shell_next_plugin(&mut self) -> Option<(i32, String)> {
        None
    }

    /// Get the tail size of plugin when it is stopped. Used in offline processing as well.
    fn get_tail_size(&self) -> isize {
        0
    }

    /// Process an audio buffer containing `f32` values.
    ///
    /// # Example
    /// ```no_run
    /// # use tutti_vst2_author::plugin::{HostCallback, Info, Plugin};
    /// # use tutti_vst2_author::buffer::AudioBuffer;
    /// #
    /// # struct ExamplePlugin;
    /// # impl Plugin for ExamplePlugin {
    /// #     fn new(_host: HostCallback) -> Self { Self }
    /// #
    /// #     fn get_info(&self) -> Info { Default::default() }
    /// #
    /// // Processor that clips samples above 0.4 or below -0.4:
    /// fn process(&mut self, buffer: &mut AudioBuffer<f32>){
    ///     // For each input and output
    ///     for (input, output) in buffer.zip() {
    ///         // For each input sample and output sample in buffer
    ///         for (in_sample, out_sample) in input.into_iter().zip(output.into_iter()) {
    ///             *out_sample = if *in_sample > 0.4 {
    ///                 0.4
    ///             } else if *in_sample < -0.4 {
    ///                 -0.4
    ///             } else {
    ///                 *in_sample
    ///             };
    ///         }
    ///     }
    /// }
    /// # }
    /// ```
    ///
    /// This method is only called while the plugin is in the *resumed* state.
    fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        // For each input and output
        for (input, output) in buffer.zip() {
            // For each input sample and output sample in buffer
            for (in_frame, out_frame) in input.iter().zip(output.iter_mut()) {
                *out_frame = *in_frame;
            }
        }
    }

    /// Process an audio buffer containing `f64` values.
    ///
    /// # Example
    /// ```no_run
    /// # use tutti_vst2_author::plugin::{HostCallback, Info, Plugin};
    /// # use tutti_vst2_author::buffer::AudioBuffer;
    /// #
    /// # struct ExamplePlugin;
    /// # impl Plugin for ExamplePlugin {
    /// #     fn new(_host: HostCallback) -> Self { Self }
    /// #
    /// #     fn get_info(&self) -> Info { Default::default() }
    /// #
    /// // Processor that clips samples above 0.4 or below -0.4:
    /// fn process_f64(&mut self, buffer: &mut AudioBuffer<f64>){
    ///     // For each input and output
    ///     for (input, output) in buffer.zip() {
    ///         // For each input sample and output sample in buffer
    ///         for (in_sample, out_sample) in input.into_iter().zip(output.into_iter()) {
    ///             *out_sample = if *in_sample > 0.4 {
    ///                 0.4
    ///             } else if *in_sample < -0.4 {
    ///                 -0.4
    ///             } else {
    ///                 *in_sample
    ///             };
    ///         }
    ///     }
    /// }
    /// # }
    /// ```
    ///
    /// This method is only called while the plugin is in the *resumed* state.
    fn process_f64(&mut self, buffer: &mut AudioBuffer<f64>) {
        // For each input and output
        for (input, output) in buffer.zip() {
            // For each input sample and output sample in buffer
            for (in_frame, out_frame) in input.iter().zip(output.iter_mut()) {
                *out_frame = *in_frame;
            }
        }
    }

    /// Handle incoming events sent from the host.
    ///
    /// This is always called before the start of `process` or `process_f64`.
    ///
    /// This method is only called while the plugin is in the *resumed* state.
    fn process_events(&mut self, events: &api::Events) {}

    /// Get a reference to the shared parameter object.
    fn get_parameter_object(&mut self) -> Arc<dyn PluginParameters> {
        Arc::new(DummyPluginParameters)
    }

    /// Get information about an input channel. Only used by some hosts.
    fn get_input_info(&self, input: i32) -> ChannelInfo {
        ChannelInfo::new(
            format!("Input channel {}", input),
            Some(format!("In {}", input)),
            true,
            None,
        )
    }

    /// Get information about an output channel. Only used by some hosts.
    fn get_output_info(&self, output: i32) -> ChannelInfo {
        ChannelInfo::new(
            format!("Output channel {}", output),
            Some(format!("Out {}", output)),
            true,
            None,
        )
    }

    /// Called one time before the start of process call.
    ///
    /// This indicates that the process call will be interrupted (due to Host reconfiguration
    /// or bypass state when the plug-in doesn't support softBypass).
    ///
    /// This method is only called while the plugin is in the *resumed* state.
    fn start_process(&mut self) {}

    /// Called after the stop of process call.
    ///
    /// This method is only called while the plugin is in the *resumed* state.
    fn stop_process(&mut self) {}

    /// Return handle to plugin editor if supported.
    /// The method need only return the object on the first call.
    /// Subsequent calls can just return `None`.
    ///
    /// The editor object will typically contain an `Arc` reference to the parameter
    /// object through which it can communicate with the audio processing.
    fn get_editor(&mut self) -> Option<Box<dyn Editor>> {
        None
    }
}

/// Parameter object shared between the UI and processing threads.
/// Since access is shared, all methods take `self` by immutable reference.
/// All mutation must thus be performed using thread-safe interior mutability.
#[allow(unused_variables)]
pub trait PluginParameters: Send + Sync {
    /// Set the current preset to the index specified by `preset`.
    ///
    /// This method can be called on the processing thread for automation.
    fn change_preset(&self, preset: i32) {}

    /// Get the current preset index.
    fn get_preset_num(&self) -> i32 {
        0
    }

    /// Set the current preset name.
    fn set_preset_name(&self, name: String) {}

    /// Get the name of the preset at the index specified by `preset`.
    fn get_preset_name(&self, preset: i32) -> String {
        "".to_string()
    }

    /// Get parameter label for parameter at `index` (e.g. "db", "sec", "ms", "%").
    fn get_parameter_label(&self, index: i32) -> String {
        "".to_string()
    }

    /// Get the parameter value for parameter at `index` (e.g. "1.0", "150", "Plate", "Off").
    fn get_parameter_text(&self, index: i32) -> String {
        format!("{:.3}", self.get_parameter(index).unwrap_or(0.0))
    }

    /// Get the name of parameter at `index`.
    fn get_parameter_name(&self, index: i32) -> String {
        format!("Param {}", index)
    }

    /// Get the value of parameter at `index`, or `None` if this implementation
    /// exposes no parameter access at all.
    ///
    /// **Fork divergence from upstream vst-rs**, which returns a bare `f32`.
    /// That mirrors the C API, where the call cannot fail — but on the host side
    /// it can: VST 2.4 lets a plugin declaring `numParams == 0` leave
    /// `AEffect::getParameter` null, and a bare `f32` has to answer `0.0` there.
    /// `0.0` is also a perfectly ordinary parameter value, so the two collapse
    /// into each other and later surface as a parameter that silently reads
    /// zero.
    ///
    /// There is deliberately no infallible variant alongside this one. A lossy
    /// method and an honest one means callers reach for the lossy one by
    /// default and the distinction rots; `unwrap_or(0.0)` at a call site that
    /// genuinely does not care is one visible line.
    ///
    /// Plugin-side implementations always have a value, so returning `Some` is
    /// the whole of the work — see the `Default` note below.
    ///
    /// # Implementing
    ///
    /// A plugin overriding this should return `Some(value)` unconditionally.
    /// `None` is reserved for a *host* wrapping a loaded plugin that offers no
    /// parameter access; it means "there is nothing to ask", not "the value is
    /// unknown right now".
    fn get_parameter(&self, index: i32) -> Option<f32> {
        None
    }

    /// Set the value of parameter at `index`, reporting whether the value was
    /// delivered. `value` is between 0.0 and 1.0.
    ///
    /// This method can be called on the processing thread for automation.
    ///
    /// `false` means the write went nowhere — the host-side counterpart of
    /// [`get_parameter`](Self::get_parameter)'s `None`, and the case that
    /// matters on a preset-restore path, where the entire point is that the
    /// values arrive. A dropped write and a successful one are otherwise
    /// identical from the caller's side.
    fn set_parameter(&self, index: i32, value: f32) -> bool {
        false
    }

    /// Return whether parameter at `index` can be automated.
    fn can_be_automated(&self, index: i32) -> bool {
        true
    }

    /// Use String as input for parameter value. Used by host to provide an editable field to
    /// adjust a parameter value. E.g. "100" may be interpreted as 100hz for parameter. Returns if
    /// the input string was used.
    fn string_to_parameter(&self, index: i32, text: String) -> bool {
        false
    }

    /// If `preset_chunks` is set to true in plugin info, this should return the raw chunk data for
    /// the current preset.
    fn get_preset_data(&self) -> Vec<u8> {
        Vec::new()
    }

    /// If `preset_chunks` is set to true in plugin info, this should return the raw chunk data for
    /// the current plugin bank.
    fn get_bank_data(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Host-side chunk read that can report a *failed* save.
    ///
    /// [`get_preset_data`](Self::get_preset_data)'s empty `Vec` conflates
    /// "nothing saved" with "`effGetChunk` returned an error", and a host that
    /// cannot separate them downgrades the second to a parameter snapshot,
    /// losing the plugin's non-parameter state.
    ///
    /// The default never errs, which is right for a plugin implementation: it is
    /// the source of the bytes and has no dispatch result to relay.
    ///
    /// # Errors
    ///
    /// [`GetChunkError`] when the chunk read failed. The default never errs.
    fn try_get_preset_data(&self) -> Result<Vec<u8>, GetChunkError> {
        Ok(self.get_preset_data())
    }

    /// If `preset_chunks` is set to true in plugin info, this should load a preset from the given
    /// chunk data.
    ///
    /// There is nothing to return: `aeffect.h` documents no return value for `effSetChunk`,
    /// so a host cannot read a refusal from it, and the dispatcher answers 0, as a JUCE
    /// plugin does after a successful load. A plugin handed a blob it cannot read keeps its
    /// state and says nothing.
    fn load_preset_data(&self, data: &[u8]) {}

    /// If `preset_chunks` is set to true in plugin info, this should load a preset bank from the
    /// given chunk data. See [`load_preset_data`](Self::load_preset_data).
    fn load_bank_data(&self, data: &[u8]) {}
}

struct DummyPluginParameters;

impl PluginParameters for DummyPluginParameters {}

/// A reference to the host which allows the plugin to call back and access information.
///
/// # Panics
///
/// All methods in this struct will panic if the `HostCallback` was constructed using
/// `Default::default()` rather than being set to the value passed to `Plugin::new`.
#[derive(Copy, Clone, Debug)]
pub struct HostCallback {
    callback: Option<HostCallbackProc>,
    effect: *mut AEffect,
}

/// `HostCallback` implements `Default` so that the plugin can implement `Default` and have a
/// `HostCallback` field.
impl Default for HostCallback {
    fn default() -> HostCallback {
        HostCallback {
            callback: None,
            effect: ptr::null_mut(),
        }
    }
}

// SAFETY: the value is a `Copy` function pointer plus the plugin's own `*mut AEffect`;
// neither is mutated through `&HostCallback`, so moving it is sound. The VST2 ABI has the
// host's `audioMaster` callable from the plugin's audio and UI threads (the audio thread
// asks for the time inside `process`); which opcode suits which thread is the plugin
// author's, and a host that does not take a call on a thread breaks its own contract.
unsafe impl Send for HostCallback {}
// SAFETY: as for `Send`; shared access only reads the two fields.
unsafe impl Sync for HostCallback {}

impl HostCallback {
    /// The host's `(version, vendor, product)`, read through `audioMaster` callbacks. Owned
    /// strings: the plugin's calling thread is not the audio thread.
    pub fn host_info(&self) -> (isize, String, String) {
        use api::consts::*;
        let version = self.callback(
            self.effect,
            AudioMasterOpCode::CurrentId,
            0,
            0,
            ptr::null_mut(),
            0.0,
        ) as isize;
        let vendor_name = self.read_string(AudioMasterOpCode::GetVendorString, MAX_VENDOR_STR_LEN);
        let product_name =
            self.read_string(AudioMasterOpCode::GetProductString, MAX_PRODUCT_STR_LEN);
        (version, vendor_name, product_name)
    }

    /// Wrap callback in a function to avoid using fn pointer notation.
    #[doc(hidden)]
    fn callback(
        &self,
        effect: *mut AEffect,
        opcode: AudioMasterOpCode,
        index: i32,
        value: isize,
        ptr: *mut c_void,
        opt: f32,
    ) -> isize {
        let callback = self
            .callback
            .unwrap_or_else(|| panic!("Host not yet initialized."));
        // SAFETY: `callback` is the `audioMaster` the host handed this plugin
        // at load, and `effect` is the plugin's own `AEffect`. `ptr`/`value`
        // must suit `opcode` — the obligation `HostCallbackProc` being `unsafe`
        // now states rather than leaving to each caller to know.
        unsafe { callback(effect, opcode.into(), index, value, ptr, opt) }
    }

    /// Check whether the plugin has been initialized.
    #[doc(hidden)]
    fn is_effect_valid(&self) -> bool {
        // Check whether `effect` points to a valid AEffect struct
        //
        // SAFETY: `effect` is non-null here (a `HostCallback` made by `Default` has none),
        // and then it is the plugin's own `AEffect`, live while the plugin is.
        !self.effect.is_null() && unsafe { (*self.effect).magic == VST_MAGIC }
    }

    /// Create a new Host structure wrapping a host callback.
    #[doc(hidden)]
    pub fn wrap(callback: HostCallbackProc, effect: *mut AEffect) -> HostCallback {
        HostCallback {
            callback: Some(callback),
            effect,
        }
    }

    /// Get the VST API version supported by the host e.g. `2400 = VST 2.4`.
    pub fn vst_version(&self) -> i32 {
        self.callback(
            self.effect,
            AudioMasterOpCode::Version,
            0,
            0,
            ptr::null_mut(),
            0.0,
        ) as i32
    }

    /// Get the callback for calling host-specific extensions
    #[inline(always)]
    pub fn raw_callback(&self) -> Option<HostCallbackProc> {
        self.callback
    }

    /// Get the effect pointer for calling host-specific extensions
    #[inline(always)]
    pub fn raw_effect(&self) -> *mut AEffect {
        self.effect
    }

    fn read_string(&self, opcode: AudioMasterOpCode, max: usize) -> String {
        self.read_string_param(opcode, 0, 0, 0.0, max)
    }

    fn read_string_param(
        &self,
        opcode: AudioMasterOpCode,
        index: i32,
        value: isize,
        opt: f32,
        max: usize,
    ) -> String {
        let mut buf = vec![0; max];
        self.callback(
            self.effect,
            opcode,
            index,
            value,
            buf.as_mut_ptr() as *mut c_void,
            opt,
        );
        read_c_str(&buf)
    }
}

impl Host for HostCallback {
    /// Signal the host that the value for the parameter has changed.
    ///
    /// Make sure to also call `begin_edit` and `end_edit` when a parameter
    /// has been touched. This is important for the host to determine
    /// if a user interaction is happening and the automation should be recorded.
    fn automate(&self, index: i32, value: f32) {
        if self.is_effect_valid() {
            self.callback(
                self.effect,
                AudioMasterOpCode::Automate,
                index,
                0,
                ptr::null_mut(),
                value,
            );
        }
    }

    /// Signal the host the start of a parameter change a gesture (mouse down on knob dragging).
    fn begin_edit(&self, index: i32) {
        self.callback(
            self.effect,
            AudioMasterOpCode::BeginEdit,
            index,
            0,
            ptr::null_mut(),
            0.0,
        );
    }

    /// Signal the host the end of a parameter change gesture (mouse up after knob dragging).
    fn end_edit(&self, index: i32) {
        self.callback(
            self.effect,
            AudioMasterOpCode::EndEdit,
            index,
            0,
            ptr::null_mut(),
            0.0,
        );
    }

    fn get_plugin_id(&self) -> i32 {
        self.callback(
            self.effect,
            AudioMasterOpCode::CurrentId,
            0,
            0,
            ptr::null_mut(),
            0.0,
        ) as i32
    }

    fn idle(&self) {
        self.callback(
            self.effect,
            AudioMasterOpCode::Idle,
            0,
            0,
            ptr::null_mut(),
            0.0,
        );
    }

    /// Request time information from Host.
    ///
    /// The mask parameter is composed of the same flags which will be found in the `flags` field of `TimeInfo` when returned.
    /// That is, if you want the host's tempo, the parameter passed to `get_time_info()` should have the `TEMPO_VALID` flag set.
    /// This request and delivery system is important, as a request like this may cause
    /// significant calculations at the application's end, which may take a lot of our precious time.
    /// This obviously means you should only set those flags that are required to get the information you need.
    ///
    /// Also please be aware that requesting information does not necessarily mean that that information is provided in return.
    /// Check the flags field in the `TimeInfo` structure to see if your request was actually met.
    fn get_time_info(&self, mask: i32) -> Option<TimeInfo> {
        let opcode = AudioMasterOpCode::GetTime;
        let mask = mask as isize;
        let null = ptr::null_mut();
        let ptr = self.callback(self.effect, opcode, 0, mask, null, 0.0);

        match ptr {
            0 => None,
            // SAFETY: for `audioMasterGetTime` a non-zero return is, per the VST2 contract, the
            // address of a host-owned `TimeInfo`; the host must keep it valid until the next
            // call. That is trusted, not checked (alignment included); `TimeInfo` is `Copy`.
            ptr => Some(unsafe { *(ptr as *const TimeInfo) }),
        }
    }

    /// Get the host's current process level (`audioMasterGetCurrentProcessLevel`).
    ///
    /// The level of the thread asking: a plugin asks this to decide how much time it may
    /// spend per block, since under `Offline` there is no deadline and a higher-quality
    /// path is affordable. A value outside `VstProcessLevels` reads as `Unknown`.
    fn get_process_level(&self) -> api::ProcessLevel {
        let raw = self.callback(
            self.effect,
            AudioMasterOpCode::GetCurrentProcessLevel,
            0,
            0,
            ptr::null_mut(),
            0.0,
        );
        i32::try_from(raw)
            .ok()
            .and_then(|v| api::ProcessLevel::try_from(v).ok())
            .unwrap_or(api::ProcessLevel::Unknown)
    }

    /// Get the host's automation state (`audioMasterGetAutomationState`). A value outside
    /// `VstAutomationStates` reads as `Unsupported`.
    fn get_automation_state(&self) -> api::AutomationState {
        let raw = self.callback(
            self.effect,
            AudioMasterOpCode::GetAutomationState,
            0,
            0,
            ptr::null_mut(),
            0.0,
        );
        i32::try_from(raw)
            .ok()
            .and_then(|v| api::AutomationState::try_from(v).ok())
            .unwrap_or(api::AutomationState::Unsupported)
    }

    /// Get block size.
    fn get_block_size(&self) -> isize {
        self.callback(
            self.effect,
            AudioMasterOpCode::GetBlockSize,
            0,
            0,
            ptr::null_mut(),
            0.0,
        )
    }

    /// Refresh UI after the plugin's parameters changed.
    fn update_display(&self) {
        self.callback(
            self.effect,
            AudioMasterOpCode::UpdateDisplay,
            0,
            0,
            ptr::null_mut(),
            0.0,
        );
    }
}
