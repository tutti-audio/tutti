//! Tests of the host half: the loader, the instance and the render handle.

use super::*;
use crate::api::Supported;

/// Pointer tables for building an `AudioBuffer` over test sample arrays.
struct HostBuffer<T: Float> {
    inputs: Vec<*const T>,
    outputs: Vec<*mut T>,
}

impl<T: Float> HostBuffer<T> {
    fn new(input_count: usize, output_count: usize) -> HostBuffer<T> {
        HostBuffer {
            inputs: vec![ptr::null(); input_count],
            outputs: vec![ptr::null_mut(); output_count],
        }
    }

    /// An `AudioBuffer` over the arrays, which are borrowed as long as it lives.
    fn bind<'a, I, O>(
        &'a mut self,
        input_arrays: &'a [I],
        output_arrays: &'a mut [O],
    ) -> AudioBuffer<'a, T>
    where
        I: AsRef<[T]>,
        O: AsMut<[T]>,
    {
        assert!(input_arrays.len() <= self.inputs.len());
        assert!(output_arrays.len() <= self.outputs.len());
        let mut length = None;
        for (i, input) in input_arrays.iter().map(|r| r.as_ref()).enumerate() {
            self.inputs[i] = input.as_ptr();
            assert_eq!(*length.get_or_insert(input.len()), input.len());
        }
        for (i, output) in output_arrays.iter_mut().map(|r| r.as_mut()).enumerate() {
            self.outputs[i] = output.as_mut_ptr();
            assert_eq!(*length.get_or_insert(output.len()), output.len());
        }
        // SAFETY: the first `input_arrays.len()` / `output_arrays.len()` slots were just
        // set from slices of `length` samples, the output slices are distinct `&mut`,
        // and the arrays and `self` are borrowed for `'a`, the buffer's lifetime.
        unsafe {
            AudioBuffer::from_raw(
                input_arrays.len(),
                output_arrays.len(),
                self.inputs.as_ptr(),
                self.outputs.as_mut_ptr(),
                length.unwrap_or(0),
            )
        }
    }
}

/// `PluginInstance::start_process` / `stop_process` must dispatch the
/// opcodes the plugin-side dispatcher in `interfaces.rs` actually matches
/// on, and must not be left at the `Plugin` trait's empty default bodies.
///
/// Asserting the numeric values is the point: `AudioMasterOpCode` is `#[repr(i32)]` and
/// positional, so inserting a variant above these shifts every opcode below
/// it and silently re-points the dispatch. The SDK fixes
/// `effStartProcess = 71` and `effStopProcess = 72`.
/// Spec: vst2.eff.StartProcess, vst2.eff.StopProcess
#[test]
fn start_and_stop_process_opcodes_match_the_sdk() {
    assert_eq!(plugin::EffectOpCode::StartProcess as i32, 71);
    assert_eq!(plugin::EffectOpCode::StopProcess as i32, 72);

    // And the plugin-side dispatcher decodes those same numbers back to
    // the variants it handles — the other half of the round trip.
    assert!(matches!(
        plugin::EffectOpCode::try_from(71),
        Ok(plugin::EffectOpCode::StartProcess)
    ));
    assert!(matches!(
        plugin::EffectOpCode::try_from(72),
        Ok(plugin::EffectOpCode::StopProcess)
    ));
}

/// `effSetProcessPrecision = 77`, pinned for the same reason as the pair
/// above. It sits four variants below `effStopProcess`, so it is the one
/// most exposed to a variant inserted anywhere in that range.
/// Spec: vst2.eff.SetProcessPrecision
#[test]
fn set_precision_opcode_matches_the_sdk() {
    assert_eq!(plugin::EffectOpCode::SetPrecision as i32, 77);
    assert!(matches!(
        plugin::EffectOpCode::try_from(77),
        Ok(plugin::EffectOpCode::SetPrecision)
    ));
}

/// A real plugin's `effCanDo` returns whatever its dispatcher left in the
/// return slot — commonly the length of the queried string, or an
/// uninitialised stack value.
#[test]
fn can_do_answers_outside_the_documented_set_do_not_panic() {
    assert_eq!(Supported::from(1), Supported::Yes);
    assert_eq!(Supported::from(0), Supported::Maybe);
    assert_eq!(Supported::from(-1), Supported::No);

    // The realistic garbage values.
    assert_eq!(Supported::from(14), Supported::Custom(14)); // strlen("receiveVstMidiEvent")-ish
    assert_eq!(Supported::from(-2), Supported::Custom(-2));
    assert_eq!(Supported::from(isize::MAX), Supported::Custom(isize::MAX));

    // And critically, none of them read as an affirmative "yes".
    for v in [2, 14, -2, 9999, isize::MIN, isize::MAX] {
        assert_ne!(Supported::from(v), Supported::Yes, "value {v}");
    }
}

/// `effGetChunk` is not trusted: a null pointer, a negative length or one past the state
/// cap is refused before any slice is formed. This is the project-*save* path, so each of these would be a real
/// crash on a real user's save.
/// Spec: vst2.eff.GetChunk
#[test]
fn get_chunk_rejects_unreadable_plugin_answers() {
    // `effFlagsProgramChunks` set but nothing to save yet: the plugin
    // returns 0 and never writes the out-pointer. `from_raw_parts(null, 0)`
    // is UB even at length zero — the pointer must always be non-null.
    //
    // `Ok(empty)`, not an error: the plugin answered, and the answer is
    // "nothing saved".
    // SAFETY: `copy_chunk` reads through `ptr` only when `len > 0` and `ptr` is non-null; here `len <= 0` so it returns before any read.
    assert_eq!(unsafe { copy_chunk(ptr::null_mut(), 0) }, Ok(Vec::new()));

    // A length no state holds is refused with a live buffer: nothing is read.
    let mut data = [1u8, 2, 3, 4];
    let past_cap = MAX_STATE_BYTES as isize + 1;
    assert_eq!(
        // SAFETY: `len` is past the cap, so `copy_chunk` returns before any read.
        unsafe { copy_chunk(data.as_mut_ptr(), past_cap) },
        Err(GetChunkError::TooLarge(past_cap))
    );

    // An error return.
    assert_eq!(
        // SAFETY: `copy_chunk` reads through `ptr` only when `len > 0` and `ptr` is non-null; here `len <= 0` so it returns before any read.
        unsafe { copy_chunk(ptr::null_mut(), -1) },
        Err(GetChunkError::Failed(-1))
    );

    // A plugin that sized the chunk but failed to hand back the buffer.
    assert_eq!(
        // SAFETY: `copy_chunk` returns `NullBuffer` on a null `ptr` with `len > 0` before any read.
        unsafe { copy_chunk(ptr::null_mut(), 4096) },
        Err(GetChunkError::NullBuffer(4096))
    );

    // A non-null pointer with a negative length is still rejected — the
    // length is what would be cast, and it must never reach `as usize`.
    assert_eq!(
        // SAFETY: `len` is negative, so `copy_chunk` returns before any read; `data` is a live 4-byte array regardless.
        unsafe { copy_chunk(data.as_mut_ptr(), -1) },
        Err(GetChunkError::Failed(-1))
    );

    // The good case still copies.
    assert_eq!(
        // SAFETY: `data` is a live local 4-byte array, so `ptr` is valid for reads of `len` = 4 bytes.
        unsafe { copy_chunk(data.as_mut_ptr(), 4) },
        Ok(vec![1, 2, 3, 4])
    );
}

/// `effEditOpen` / `effEditClose` must each be dispatched at most once per
/// open editor, and a truthy-but-not-1 open must count as success.
///
/// Three defects, all observable by counting dispatches: `open` demanded
/// `result == 1` (rejecting truthy non-1 answers some plugins give);
/// neither `open` nor `close` checked `is_open`, so a repeat open leaked the
/// first window and a host's two teardown paths double-closed.
/// Spec: vst2.eff.EditOpen, vst2.eff.EditClose
#[test]
fn the_editor_opens_once_closes_once_and_accepts_a_truthy_open() {
    use crate::api::DispatcherProc;
    use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

    static OPEN_ANSWER: AtomicIsize = AtomicIsize::new(1);
    static OPENS: AtomicUsize = AtomicUsize::new(0);
    static CLOSES: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn dispatch(
        _effect: *mut AEffect,
        opcode: i32,
        _index: i32,
        _value: isize,
        _ptr: *mut c_void,
        _opt: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::EditorOpen as i32 {
            OPENS.fetch_add(1, Ordering::SeqCst);
            return OPEN_ANSWER.load(Ordering::SeqCst);
        }
        if opcode == plugin::EffectOpCode::EditorClose as i32 {
            CLOSES.fetch_add(1, Ordering::SeqCst);
        }
        0
    }

    // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
    let mut effect: AEffect = unsafe { std::mem::zeroed() };
    effect.magic = VST_MAGIC;
    effect.dispatcher = Some(dispatch as DispatcherProc);
    let (mut ed, main) = instance_over(&mut effect as *mut AEffect);

    let reset = || {
        OPENS.store(0, Ordering::SeqCst);
        CLOSES.store(0, Ordering::SeqCst);
    };

    // Closing an editor that was never opened must not reach the plugin.
    reset();
    ed.close_editor(main);
    assert_eq!(
        CLOSES.load(Ordering::SeqCst),
        0,
        "effEditClose dispatched for an editor that was never open"
    );

    // A plugin answering 2 has opened successfully.
    reset();
    OPEN_ANSWER.store(2, Ordering::SeqCst);
    assert!(
        ed.open_editor(main, no_window()),
        "a truthy non-1 effEditOpen return must read as success; `== 1` \
         reported an embedded editor as a refusal"
    );

    // A second open must not reach the plugin.
    ed.open_editor(main, no_window());
    assert_eq!(
        OPENS.load(Ordering::SeqCst),
        1,
        "effEditOpen dispatched twice without an intervening close — \
         undefined per VST 2.4"
    );

    // Close twice: only the first reaches the plugin.
    ed.close_editor(main);
    ed.close_editor(main);
    assert_eq!(
        CLOSES.load(Ordering::SeqCst),
        1,
        "effEditClose dispatched twice — close_editor() plus Drop is \
         exactly the sequence a host runs"
    );

    // A refusal is still a refusal, so none of the above is satisfied by
    // always reporting success.
    reset();
    OPEN_ANSWER.store(0, Ordering::SeqCst);
    assert!(
        !ed.open_editor(main, no_window()),
        "effEditOpen returned 0 (refused)"
    );
    ed.close_editor(main);
    assert_eq!(
        CLOSES.load(Ordering::SeqCst),
        0,
        "an editor whose open was refused is not open, so closing it must \
         not reach the plugin"
    );
}

/// `PluginInstance::idle_editor` dispatches `effEditIdle` while the editor is open, and
/// only then.
///
/// An idle that dispatched nothing would freeze editors that animate in `effEditIdle`.
/// Spec: vst2.eff.EditIdle
#[test]
fn idle_dispatches_effeditidle_only_while_the_editor_is_open() {
    use crate::api::DispatcherProc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static IDLES: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn dispatch(
        _effect: *mut AEffect,
        opcode: i32,
        _index: i32,
        _value: isize,
        _ptr: *mut c_void,
        _opt: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::EditorOpen as i32 {
            return 1;
        }
        if opcode == plugin::EffectOpCode::EditorIdle as i32 {
            IDLES.fetch_add(1, Ordering::SeqCst);
        }
        0
    }

    // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
    let mut effect: AEffect = unsafe { std::mem::zeroed() };
    effect.dispatcher = Some(dispatch as DispatcherProc);
    effect.magic = VST_MAGIC;
    let (mut ed, main) = instance_over(&mut effect as *mut AEffect);

    IDLES.store(0, Ordering::SeqCst);
    ed.idle_editor(main);
    assert_eq!(IDLES.load(Ordering::SeqCst), 0, "idle before open");

    assert!(ed.open_editor(main, no_window()));
    ed.idle_editor(main);
    ed.idle_editor(main);
    assert_eq!(IDLES.load(Ordering::SeqCst), 2, "one effEditIdle per idle");

    ed.close_editor(main);
    ed.idle_editor(main);
    assert_eq!(IDLES.load(Ordering::SeqCst), 2, "idle after close");
}

/// `audioMasterCanDo` reaches `Host::can_do` with the plugin's string, and
/// the three VST 2.4 answers come back as `1`, `0` and `-1`.
///
/// A plugin gates a feature on `canDo("sendVstEvents")`, so each query is answered
/// from the host, never with the fall-through `0` ("don't know").
/// Spec: vst2.audioMaster.CanDo
#[test]
fn host_can_do_reaches_the_host_and_maps_to_the_three_answers() {
    struct Answers;
    impl Host for Answers {
        fn can_do(&self, can_do: &str) -> Supported {
            match can_do {
                "sendVstEvents" => Supported::Yes,
                "openFileSelector" => Supported::No,
                _ => Supported::Maybe,
            }
        }
    }

    let ask = |name: &str| {
        let name = CString::new(name).unwrap();
        // SAFETY: `name` is a NUL-terminated string that outlives the call.
        unsafe {
            interfaces::host_dispatch(
                &Answers,
                ptr::null_mut(),
                AudioMasterOpCode::CanDo as i32,
                0,
                0,
                name.as_ptr() as *mut c_void,
                0.0,
            )
        }
    };
    assert_eq!(ask("sendVstEvents"), 1);
    assert_eq!(ask("openFileSelector"), -1);
    assert_eq!(ask("somethingElse"), 0);
}

/// **The host has no file dialog and no localization**: `audioMasterOpenFileSelector` and
/// `audioMasterCloseFileSelector` answer 0 (declined) without reading the `VstFileSelect`
/// the plugin passes, and `audioMasterGetLanguage` answers `kVstLangEnglish` (1), never
/// the 0 that names no language.
/// Spec: vst2.audioMaster.GetLanguage, vst2.language.kVstLangEnglish
#[test]
fn file_selector_is_declined_and_language_is_english() {
    struct Plain;
    impl Host for Plain {}
    let ask = |opcode: AudioMasterOpCode| {
        // A non-null garbage pointer: a declining arm must not dereference it.
        // SAFETY: the declining arms do not read `ptr`, which is what the test checks.
        unsafe {
            interfaces::host_dispatch(
                &Plain,
                ptr::null_mut(),
                opcode as i32,
                0,
                0,
                0x10 as *mut c_void,
                0.0,
            )
        }
    };
    assert_eq!(ask(AudioMasterOpCode::OpenFileSelector), 0);
    assert_eq!(ask(AudioMasterOpCode::CloseFileSelector), 0);
    assert_eq!(ask(AudioMasterOpCode::GetLanguage), 1);
}

/// **A chunk reaches the plugin as the header lays `effSetChunk` out**: `[index]` is 0 for a
/// bank and 1 for a program, `[value]` the byte size, `[ptr]` the data. The plugin's
/// return value is not part of that layout (`aeffect.h` documents none), and a plugin
/// that returns 0 after a successful load, as a JUCE plugin does, must not be told it
/// failed: the call has no way to report a refusal, so none is invented.
/// Spec: vst2.eff.SetChunk
#[test]
fn a_chunk_reaches_the_plugin_with_its_index_and_size() {
    use crate::api::DispatcherProc;
    static SEEN: Mutex<Vec<(i32, isize, Vec<u8>)>> = Mutex::new(Vec::new());

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        index: i32,
        value: isize,
        ptr: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::SetData as i32 {
            // SAFETY: the host gives `value` readable bytes at `ptr`.
            let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, value as usize) };
            SEEN.lock().push((index, value, bytes.to_vec()));
        }
        0
    }

    // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
    let mut effect: AEffect = unsafe { std::mem::zeroed() };
    effect.magic = VST_MAGIC;
    effect.dispatcher = Some(dispatch as DispatcherProc);
    let (params, main) = instance_over(&mut effect as *mut AEffect);
    params.load_preset_data(main, b"program");
    assert_eq!(*SEEN.lock(), [(1, 7, b"program".to_vec())]);
}

/// A *failed* save and an *empty* save must not produce the same value.
#[test]
fn failed_chunk_save_is_distinguishable_from_an_empty_one() {
    // SAFETY: `copy_chunk` reads through `ptr` only when `len > 0` and `ptr` is non-null; here `len <= 0` so it returns before any read.
    let empty = unsafe { copy_chunk(ptr::null_mut(), 0) };
    // SAFETY: `copy_chunk` reads through `ptr` only when `len > 0` and `ptr` is non-null; here `len <= 0` so it returns before any read.
    let failed = unsafe { copy_chunk(ptr::null_mut(), -1) };
    assert_ne!(
        empty, failed,
        "a refused effGetChunk must not look like an empty one"
    );
    assert!(empty.is_ok(), "nothing-saved is a normal answer");
    assert!(failed.is_err(), "an error return must surface as an error");
}

/// The fallback path clears the outputs before the accumulating
/// `process` adds into them, and is also what a plugin with neither entry
/// point gets. Whatever the caller's scratch held must not leak through.
#[test]
fn zero_outputs_silences_every_channel() {
    let mut host_buffer: HostBuffer<f32> = HostBuffer::new(2, 2);
    let inputs = vec![vec![1.0f32; 8]; 2];
    let mut outputs = vec![vec![0.5f32; 8]; 2];
    {
        let mut buffer = host_buffer.bind(&inputs, &mut outputs);
        zero_outputs(&mut buffer);
    }
    assert_eq!(outputs, vec![vec![0.0f32; 8]; 2]);
}

/// The audio entry points must be *detectably* absent.
///
/// They were typed as bare `extern "C" fn`, which the compiler knows to be
/// non-null, so the `(p as *const u8).is_null()` guard folded to `false` under
/// `-O`: the null check shipped in debug builds and a jump to address 0 shipped
/// in release. `Option` makes the check survive optimization.
///
/// This asserts the *type-level* property, which is what actually failed —
/// a plugin's real null slot cannot be exercised without a hostile plugin
/// binary, but a guard the optimizer can delete is the whole bug.
#[test]
fn a_null_entry_point_is_detectable_at_every_opt_level() {
    use crate::api::{DispatcherProc, ProcessProc, ProcessProcF64};

    // The bytes a plugin leaves in a slot it never fills in. Transmuted
    // per-field: every function slot is `Option`, so an all-zero
    // `AEffect` would in fact be valid, but building one would assert far
    // less precisely than naming each slot that must read as absent.
    // SAFETY: `Option<fn>` has the size of a pointer and null is its `None` niche, so the zero word is a valid `None`.
    let absent: Option<ProcessProc> = unsafe { std::mem::transmute(0usize) };
    assert!(
        absent.is_none(),
        "a null processReplacing must read as absent; if this fails, the \
         guard in `process` is decorative and release builds jump to 0"
    );

    // SAFETY: `Option<fn>` has the size of a pointer and null is its `None` niche, so the zero word is a valid `None`.
    let absent_f64: Option<ProcessProcF64> = unsafe { std::mem::transmute(0usize) };
    assert!(absent_f64.is_none());

    // SAFETY: `Option<fn>` has the size of a pointer and null is its `None` niche, so the zero word is a valid `None`.
    let absent_dispatch: Option<DispatcherProc> = unsafe { std::mem::transmute(0usize) };
    assert!(
        absent_dispatch.is_none(),
        "dispatcher carries every host->plugin call, so a null one is hit on \
         the first opcode rather than only during audio"
    );

    // A real pointer still reads as present — the guard rejects null, not
    // everything. (Any `extern "C"` fn of the right shape will do; it is never called.)
    extern "C" fn present_process(_: *mut AEffect, _: *const *const f32, _: *mut *mut f32, _: i32) {
    }
    // SAFETY: `process_replacing` is an `extern "C"` fn of `ProcessProc`'s shape, so its address as a pointer-sized word is a valid `Some(ProcessProc)`; it is only compared, never called.
    let present: Option<ProcessProc> =
        unsafe { std::mem::transmute(present_process as *const () as usize) };
    assert!(present.is_some());

    // ABI unchanged: null pointer optimization keeps `Option<fn>` one pointer
    // wide, so the C struct the plugin writes into still has its original
    // layout. A regression here is an ABI break, not a lint.
    assert_eq!(
        std::mem::size_of::<Option<ProcessProc>>(),
        std::mem::size_of::<ProcessProc>(),
    );
    assert_eq!(
        std::mem::size_of::<Option<DispatcherProc>>(),
        std::mem::size_of::<*const u8>(),
    );

    // The parameter accessors, converted later than the four above. VST 2.4
    // lets a plugin with `numParams == 0` leave these null, so an ordinary
    // parameterless effect reaches the same jump-to-zero.
    use crate::api::{GetParameterProc, SetParameterProc};
    // SAFETY: `Option<fn>` has the size of a pointer and null is its `None` niche, so the zero word is a valid `None`.
    let absent_get: Option<GetParameterProc> = unsafe { std::mem::transmute(0usize) };
    assert!(absent_get.is_none());
    // SAFETY: `Option<fn>` has the size of a pointer and null is its `None` niche, so the zero word is a valid `None`.
    let absent_set: Option<SetParameterProc> = unsafe { std::mem::transmute(0usize) };
    assert!(absent_set.is_none());
    assert_eq!(
        std::mem::size_of::<Option<GetParameterProc>>(),
        std::mem::size_of::<GetParameterProc>(),
    );
}

/// The absent case must stay distinguishable from a real `0.0`.
///
/// `PluginParameters::get_parameter` returns `Option<f32>` for exactly this
/// reason. A bare `f32`, mirroring the C API, would force `0.0` to mean both "the plugin
/// has no parameter access" and "the parameter is at zero", and there is deliberately no
/// infallible overload alongside it that would let a caller re-merge them by accident.
///
/// Driven through the real accessor over a zeroed `AEffect` — exactly the
/// struct a parameterless plugin presents, and a valid instance only
/// *because* every function slot is `Option`: an absent accessor is refused, never
/// called at address zero.
#[test]
fn an_absent_parameter_accessor_is_not_a_real_zero() {
    // Every slot null: the plugin filled in nothing.
    // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
    let mut effect: AEffect = unsafe { std::mem::zeroed() };
    let (params, _main) = instance_over(&mut effect as *mut AEffect);

    assert_eq!(
        params.get_parameter(0),
        None,
        "a null getParameter must be reported as absent, not as a value"
    );
    assert!(
        !params.set_parameter(0, 0.75),
        "a null setParameter must report that the value went nowhere"
    );

    // `None` is not `Some(0.0)`: a plugin genuinely sitting at zero stays
    // distinguishable from one that cannot be asked. Collapsing the two is
    // what the signature exists to prevent, and there is no lossy overload
    // that would quietly do it for a caller.
    assert_ne!(
        params.get_parameter(0),
        Some(0.0),
        "absent and a real zero must stay distinguishable — if they merge, \
         callers have no way to recover which happened"
    );
}

/// `Host` that answers `audioMasterCurrentId` with its own number.
struct IdHost(i32);

impl Host for IdHost {
    fn get_plugin_id(&self) -> i32 {
        self.0
    }
}

/// Rendezvous inside `main`, so both loads are mid-initialization at once.
#[cfg(unix)]
static IN_MAIN: std::sync::Barrier = std::sync::Barrier::new(2);

#[cfg(unix)]
thread_local! {
    /// What the host callback answered inside this thread's last `main`.
    static ANSWERED: Cell<isize> = const { Cell::new(-1) };
}

/// A `VSTPluginMain` that calls back during initialization, as plugins do, after both
/// loading threads are inside it.
#[cfg(unix)]
extern "C" fn probing_main(callback: crate::api::HostCallbackProc) -> *mut AEffect {
    IN_MAIN.wait();
    // SAFETY: `callback` is `callback_wrapper`, which accepts a null effect while loading;
    // opcode 2 is `audioMasterCurrentId`.
    let answer = unsafe { callback(ptr::null_mut(), 2, 0, 0, ptr::null_mut(), 0.0) };
    ANSWERED.set(answer);
    ptr::null_mut()
}

#[cfg(unix)]
fn loader(id: i32) -> (PluginLoader<IdHost>, Arc<IdHost>) {
    let host = Arc::new(IdHost(id));
    // The loader only runs `main`, so any library handle will do.
    let lib: Library = libloading::os::unix::Library::this().into();
    let loader = PluginLoader {
        main: probing_main,
        lib: Arc::new(lib),
        host: Arc::clone(&host),
    };
    (loader, host)
}

/// Two threads inside `main` at once each get their own host's answer, and the host `Arc`
/// is released when the loader goes.
///
/// Answers are collected and asserted after both joins, so a failure cannot strand the
/// other thread at the barrier.
#[cfg(unix)]
#[test]
fn concurrent_loads_reach_their_own_host() {
    let threads: Vec<_> = [11, 22]
        .into_iter()
        .map(|id| {
            std::thread::spawn(move || {
                (0..200)
                    .map(|_| {
                        let (mut loader, host) = loader(id);
                        assert!(loader.call_main().is_null());
                        let answer = ANSWERED.get();
                        drop(loader);
                        (answer, Arc::strong_count(&host))
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for (t, id) in threads.into_iter().zip([11isize, 22]) {
        for (answer, count) in t.join().unwrap() {
            assert_eq!(answer, id, "callback reached another host");
            assert_eq!(count, 1, "loader leaked a host Arc");
        }
    }
}

/// A load started inside `main` on the same thread leaves the outer load's pointer in
/// place when it returns.
#[cfg(unix)]
#[test]
fn nested_load_restores_the_outer_pointer() {
    thread_local! {
        static OUTER: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
        static AFTER_INNER: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
    }
    extern "C" fn inner_main(_: crate::api::HostCallbackProc) -> *mut AEffect {
        ptr::null_mut()
    }
    extern "C" fn outer_main(_: crate::api::HostCallbackProc) -> *mut AEffect {
        OUTER.set(LOAD_HOST.get());
        let host = Arc::new(IdHost(2));
        let lib: Library = libloading::os::unix::Library::this().into();
        let mut inner = PluginLoader {
            main: inner_main,
            lib: Arc::new(lib),
            host,
        };
        inner.call_main();
        AFTER_INNER.set(LOAD_HOST.get());
        ptr::null_mut()
    }

    let lib: Library = libloading::os::unix::Library::this().into();
    let mut outer = PluginLoader {
        main: outer_main,
        lib: Arc::new(lib),
        host: Arc::new(IdHost(1)),
    };
    outer.call_main();
    assert!(!OUTER.get().is_null());
    assert_eq!(AFTER_INNER.get(), OUTER.get());
    assert!(LOAD_HOST.get().is_null());
}

/// `main` leaves nothing behind on the loading thread: a callback after it returns, with
/// no `reserved1` to go on, is answered `0`.
#[cfg(unix)]
#[test]
fn load_host_is_null_after_main() {
    // A one-party barrier would hang on the shared one, so run `main` on a pair.
    let other = std::thread::spawn(|| {
        let (mut loader, _host) = loader(2);
        loader.call_main();
    });
    let (mut loader, _host) = loader(1);
    loader.call_main();
    other.join().unwrap();
    assert!(LOAD_HOST.get().is_null());
    let answer = callback_wrapper::<IdHost>(ptr::null_mut(), 2, 0, 0, ptr::null_mut(), 0.0);
    assert_eq!(answer, 0);
}

/// The box `reserved1` points at is freed with its owner.
#[test]
fn host_ref_releases_its_arc() {
    let host = Arc::new(IdHost(3));
    let held = HostRef::new(Arc::clone(&host));
    assert_eq!(Arc::strong_count(&host), 2);
    drop(held);
    assert_eq!(Arc::strong_count(&host), 1);
}

/// A zeroed `AEffect` the test owns and leaks, as a plugin's would be: a valid
/// magic and the given dispatcher.
fn fake_effect(dispatcher: crate::api::DispatcherProc) -> *mut AEffect {
    // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
    let mut effect: Box<AEffect> = Box::new(unsafe { std::mem::zeroed() });
    effect.magic = VST_MAGIC;
    effect.dispatcher = Some(dispatcher);
    // A render entry, which the loader requires; a test that renders installs its own.
    extern "C" fn no_process(_: *mut AEffect, _: *const *const f32, _: *mut *mut f32, _: i32) {}
    effect._process = Some(no_process);
    Box::into_raw(effect)
}

/// The library handle an instance over a fake `AEffect` keeps: this process's own.
fn this_library() -> Arc<Library> {
    #[cfg(unix)]
    let lib: Library = libloading::os::unix::Library::this().into();
    #[cfg(windows)]
    let lib: Library = libloading::os::windows::Library::this()
        .expect("this process's own module")
        .into();
    Arc::new(lib)
}

/// An instance over a fake `AEffect`, and the proof of the thread it lives on.
fn instance_over(effect: *mut AEffect) -> (PluginInstance, MainThreadMarker) {
    let (instance, main, _work) = collected_instance_over(effect);
    (instance, main)
}

/// A handle whose collector is gone: a close handed to it runs at once on the main
/// thread and is otherwise stranded.
#[cfg(unix)]
fn detached() -> MainWorkHandle {
    tutti_plugin_types::MainWork::new().1
}

/// [`instance_over`], and the collector the instance hands an off-thread close to.
fn collected_instance_over(
    effect: *mut AEffect,
) -> (
    PluginInstance,
    MainThreadMarker,
    tutti_plugin_types::MainWork,
) {
    let main = tutti_fixtures::main_thread();
    let (work, handle) = tutti_plugin_types::MainWork::new();
    let instance = PluginInstance::new(effect, this_library(), None, main, handle);
    (instance, main, work)
}

/// A window handle for an editor test: the fake plugin never reads it.
fn no_window() -> WindowHandle {
    // SAFETY: the fake plugins of these tests never dereference the handle.
    unsafe { WindowHandle::from_u64(0) }
}

/// **`effClose` is the instance's drop, on its thread, and no handle outlives it.**
/// The plugin frees its `AEffect` in response to `effClose`, so a handle that reads it
/// afterwards reads freed memory. The render handle shares the core but does not
/// own the plugin: it can neither close it (dropped last, it does nothing) nor reach it
/// once closed (every entry takes the gate and finds it closed). The fake plugin poisons
/// its slots on close, as freeing the `AEffect` does, so a call after the close is
/// visible.
#[cfg(unix)]
#[test]
fn effclose_is_the_instances_and_no_handle_outlives_it() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CLOSES: AtomicUsize = AtomicUsize::new(0);
    static RENDERS: AtomicUsize = AtomicUsize::new(0);
    static OPCODES_AFTER_CLOSE: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn dispatch(
        effect: *mut AEffect,
        opcode: i32,
        _index: i32,
        _value: isize,
        _ptr: *mut c_void,
        _opt: f32,
    ) -> isize {
        // SAFETY: the test's leaked `AEffect`, valid for the program.
        if unsafe { (*effect).getParameter.is_none() } {
            OPCODES_AFTER_CLOSE.fetch_add(1, Ordering::SeqCst);
        }
        if opcode == plugin::EffectOpCode::Shutdown as i32 {
            CLOSES.fetch_add(1, Ordering::SeqCst);
            // The plugin frees its `AEffect` here; poison what a late call sees.
            // SAFETY: as above.
            unsafe {
                (*effect).getParameter = None;
                (*effect).setParameter = None;
                (*effect).processReplacing = None;
            }
        }
        0
    }
    extern "C" fn get_parameter(_: *mut AEffect, _: i32) -> f32 {
        0.5
    }
    extern "C" fn set_parameter(_: *mut AEffect, _: i32, _: f32) {}
    extern "C" fn render(_: *mut AEffect, _: *const *const f32, outputs: *mut *mut f32, n: i32) {
        RENDERS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the test's buffer has one output channel of `n` samples.
        unsafe { std::slice::from_raw_parts_mut(*outputs, n as usize) }.fill(1.0);
    }

    let effect = fake_effect(dispatch);
    // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
    unsafe {
        (*effect).getParameter = Some(get_parameter);
        (*effect).setParameter = Some(set_parameter);
        (*effect).processReplacing = Some(render);
        (*effect).numOutputs = 1;
        (*effect).flags = PluginFlags::CAN_REPLACING.bits();
    }
    let (instance, main) = instance_over(effect);
    let mut render = instance.render_handle(main);
    let mut host: HostBuffer<f32> = HostBuffer::new(0, 1);
    let mut out = vec![vec![9.0f32; 4]];
    let silent: Vec<Vec<f32>> = Vec::new();

    // Open: the handle renders and sets, and the instance is the one that reads.
    render.process(&mut host.bind(&silent, &mut out));
    assert_eq!(RENDERS.load(Ordering::SeqCst), 1);
    assert_eq!(out[0][0], 1.0);
    assert!(render.set_parameter(0, 0.25));
    assert_eq!(instance.get_parameter(0), Some(0.5));
    assert_eq!(CLOSES.load(Ordering::SeqCst), 0);

    // The instance drops, the handle lives: that closes it, once.
    drop(instance);
    assert_eq!(
        CLOSES.load(Ordering::SeqCst),
        1,
        "effClose is the instance's drop, not the last handle's"
    );
    let after_close = OPCODES_AFTER_CLOSE.load(Ordering::SeqCst);

    // Closed: every entry is refused without calling the plugin.
    out[0].fill(9.0);
    render.process(&mut host.bind(&silent, &mut out));
    assert_eq!(
        RENDERS.load(Ordering::SeqCst),
        1,
        "a render reached a closed plugin"
    );
    assert!(
        out[0].iter().all(|s| *s == 0.0),
        "a closed plugin is silent"
    );
    assert!(!render.set_parameter(0, 0.75));
    render.start_process();
    render.stop_process();
    assert_eq!(
        OPCODES_AFTER_CLOSE.load(Ordering::SeqCst),
        after_close,
        "an entry reached the dispatcher of a closed plugin"
    );

    // The handle, last, closes nothing.
    drop(render);
    assert_eq!(
        CLOSES.load(Ordering::SeqCst),
        1,
        "effClose runs exactly once"
    );
}

/// **A drop off the instance's thread does not close there, and does not skip the close**:
/// the sequence is handed to the owner's collector and runs at its next tick
/// (`MainWork::run`), once, and a live handle keeps working until then. A close on the
/// wrong thread is what a plugin may not survive; a skipped one leaks the plugin and its
/// licence seat. A drop has no token, so the thread is recorded and compared.
#[cfg(unix)]
#[test]
fn a_drop_off_the_main_thread_is_closed_on_the_main_threads_next_tick() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CLOSES: AtomicUsize = AtomicUsize::new(0);
    static RENDERS: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::Shutdown as i32 {
            CLOSES.fetch_add(1, Ordering::SeqCst);
        }
        0
    }
    extern "C" fn render(_: *mut AEffect, _: *const *const f32, _: *mut *mut f32, _: i32) {
        RENDERS.fetch_add(1, Ordering::SeqCst);
    }

    let effect = fake_effect(dispatch);
    // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
    unsafe {
        (*effect).processReplacing = Some(render);
        (*effect).flags = PluginFlags::CAN_REPLACING.bits();
    }
    let (instance, main, work) = collected_instance_over(effect);
    let mut render = instance.render_handle(main);
    let deferred = tutti_plugin_types::MainWork::deferred_drops();

    std::thread::spawn(move || drop(instance))
        .join()
        .expect("the drop does not panic");
    assert_eq!(
        CLOSES.load(Ordering::SeqCst),
        0,
        "effClose ran on a thread that is not the instance's"
    );
    assert_eq!(
        tutti_plugin_types::MainWork::deferred_drops(),
        deferred + 1,
        "the off-thread drop is counted"
    );
    let mut host: HostBuffer<f32> = HostBuffer::new(0, 0);
    let none: Vec<Vec<f32>> = Vec::new();
    let mut none_out: Vec<Vec<f32>> = Vec::new();
    render.process(&mut host.bind(&none, &mut none_out));
    assert_eq!(
        RENDERS.load(Ordering::SeqCst),
        1,
        "the plugin stays valid for the render handle until the tick"
    );

    assert!(work.run(main) >= 1);
    assert_eq!(
        CLOSES.load(Ordering::SeqCst),
        1,
        "the main thread's tick sent the close"
    );
    render.process(&mut host.bind(&none, &mut none_out));
    assert_eq!(
        RENDERS.load(Ordering::SeqCst),
        1,
        "and the handle then finds the plugin closed"
    );
    assert_eq!(work.run(main), 0);
    assert_eq!(CLOSES.load(Ordering::SeqCst), 1, "once");
}

/// **A render handle that is the last holder frees the core on the main thread**, not on
/// the thread it is dropped on (the audio thread, for a Processor): the core keeps the
/// host box alive, and with it the host state. The instance closes first, on its thread;
/// the handle then drops elsewhere.
#[cfg(unix)]
#[test]
fn the_last_render_handle_dropped_off_the_main_thread_frees_nothing_there() {
    let host = Arc::new(IdHost(4));
    let effect = fake_effect(noop_dispatch);
    let main = tutti_fixtures::main_thread();
    let (work, handle) = tutti_plugin_types::MainWork::new();
    let instance = PluginInstance::new(
        effect,
        this_library(),
        Some(HostRef::new(Arc::clone(&host))),
        main,
        handle,
    );
    let render = instance.render_handle(main);
    assert_eq!(Arc::strong_count(&host), 2, "the core holds the host");
    drop(instance);
    assert_eq!(
        Arc::strong_count(&host),
        2,
        "the render handle still holds the core"
    );

    std::thread::spawn(move || drop(render)).join().unwrap();
    assert_eq!(
        Arc::strong_count(&host),
        2,
        "the core was not freed on the dropping thread"
    );
    work.run(main);
    assert_eq!(Arc::strong_count(&host), 1, "the main thread freed it");
}

#[cfg(unix)]
extern "C" fn noop_dispatch(
    _: *mut AEffect,
    _: i32,
    _: i32,
    _: isize,
    _: *mut c_void,
    _: f32,
) -> isize {
    0
}

/// **A panic under the gate kills the handle**: the plugin may be in any state, so every
/// later entry is refused (silence) and the plugin is never called again; the instance
/// still closes it.
#[cfg(unix)]
#[test]
fn a_panic_under_the_gate_kills_the_core() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RENDERS: AtomicUsize = AtomicUsize::new(0);
    static CLOSES: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::Shutdown as i32 {
            CLOSES.fetch_add(1, Ordering::SeqCst);
        }
        0
    }
    extern "C" fn render(_: *mut AEffect, _: *const *const f32, outputs: *mut *mut f32, n: i32) {
        RENDERS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the test's buffer has two output channels of `n` samples.
        unsafe { std::slice::from_raw_parts_mut(*outputs, n as usize) }.fill(1.0);
    }

    let effect = fake_effect(dispatch);
    // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
    unsafe {
        (*effect).numOutputs = 2;
        (*effect).processReplacing = Some(render);
        (*effect).flags = PluginFlags::CAN_REPLACING.bits();
    }
    let (instance, main) = instance_over(effect);
    let mut handle = instance.render_handle(main);
    let none: Vec<Vec<f32>> = Vec::new();

    // A buffer with one output channel for a plugin with two: the entry panics under the gate.
    let mut host1: HostBuffer<f32> = HostBuffer::new(0, 1);
    let mut one = vec![vec![9.0f32; 4]];
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle.process(&mut host1.bind(&none, &mut one));
    }));
    assert!(panicked.is_err(), "the short buffer is refused by a panic");
    assert_eq!(RENDERS.load(Ordering::SeqCst), 0);

    // A good buffer afterwards: the core is dead, so it is silent and the plugin is not called.
    let mut host2: HostBuffer<f32> = HostBuffer::new(0, 2);
    let mut two = vec![vec![9.0f32; 4]; 2];
    handle.process(&mut host2.bind(&none, &mut two));
    assert_eq!(
        RENDERS.load(Ordering::SeqCst),
        0,
        "a plugin that a panic left in an unknown state was called again"
    );
    assert!(
        two.iter().flatten().all(|s| *s == 0.0),
        "and renders silence"
    );

    drop(instance);
    assert_eq!(
        CLOSES.load(Ordering::SeqCst),
        1,
        "the instance still closes it"
    );
}

/// **Dropping the instance on its own thread sends the close sequence in order**: the
/// editor, `effStopProcess` if processing, `effMainsChanged(0)` if resumed, then
/// `effClose`; a plugin that was not resumed or not processing sees neither.
#[cfg(unix)]
#[test]
fn the_close_sequence_is_in_order_and_guarded() {
    use tutti_rt::Mutex;
    static SEEN: Mutex<Vec<i32>> = Mutex::new(Vec::new());

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        value: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        let mut seen = SEEN.lock();
        if opcode == plugin::EffectOpCode::StateChanged as i32 {
            seen.push(if value == 1 { 1000 } else { 1001 });
        } else {
            seen.push(opcode);
        }
        1
    }
    let take = || std::mem::take(&mut *SEEN.lock());
    let wanted = |ops: &[plugin::EffectOpCode]| ops.iter().map(|o| *o as i32).collect::<Vec<_>>();

    // Resumed and processing, editor open: all four, in order.
    let (mut instance, main) = instance_over(fake_effect(dispatch));
    let mut render = instance.render_handle(main);
    instance.resume(main);
    assert!(instance.open_editor(main, no_window()));
    render.start_process();
    take();
    drop(instance);
    let mut expected = wanted(&[
        plugin::EffectOpCode::EditorClose,
        plugin::EffectOpCode::StopProcess,
    ]);
    expected.push(1001);
    expected.push(plugin::EffectOpCode::Shutdown as i32);
    assert_eq!(take(), expected);
    drop(render);

    // Never resumed, never processing, no editor: just `effClose`.
    let (instance, _main) = instance_over(fake_effect(dispatch));
    take();
    drop(instance);
    assert_eq!(take(), wanted(&[plugin::EffectOpCode::Shutdown]));
}

/// **The exclusive calls are serialised with the audio entries, both ways**: an
/// `effSetSampleRate` waits for a render in flight, and a render that starts during one is
/// refused (silence) without reaching the plugin. A plugin that cannot take the opcode
/// beside `process` therefore never sees them together, whoever calls.
#[cfg(unix)]
#[test]
fn the_exclusive_calls_are_serialised_with_the_audio_entries() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    static IN_PROCESS: AtomicBool = AtomicBool::new(false);
    static RELEASE_PROCESS: AtomicBool = AtomicBool::new(false);
    static RATE_DISPATCHED: AtomicBool = AtomicBool::new(false);
    static IN_RATE: AtomicBool = AtomicBool::new(false);
    static RELEASE_RATE: AtomicBool = AtomicBool::new(false);
    static PROCESSES: AtomicUsize = AtomicUsize::new(0);
    static FINISHED_PROCESSES: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::SetSampleRate as i32 {
            // The rate must not be dispatched while a process is unfinished.
            assert_eq!(
                PROCESSES.load(Ordering::SeqCst),
                FINISHED_PROCESSES.load(Ordering::SeqCst),
                "an exclusive call began inside an audio entry"
            );
            RATE_DISPATCHED.store(true, Ordering::SeqCst);
            IN_RATE.store(true, Ordering::SeqCst);
            while !RELEASE_RATE.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            IN_RATE.store(false, Ordering::SeqCst);
        }
        0
    }
    extern "C" fn render(_: *mut AEffect, _: *const *const f32, outputs: *mut *mut f32, n: i32) {
        PROCESSES.fetch_add(1, Ordering::SeqCst);
        IN_PROCESS.store(true, Ordering::SeqCst);
        while !RELEASE_PROCESS.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        // SAFETY: the test's buffer has one output channel of `n` samples.
        unsafe { std::slice::from_raw_parts_mut(*outputs, n as usize) }.fill(1.0);
        IN_PROCESS.store(false, Ordering::SeqCst);
        FINISHED_PROCESSES.fetch_add(1, Ordering::SeqCst);
    }

    let effect = fake_effect(dispatch);
    // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
    unsafe {
        (*effect).processReplacing = Some(render);
        (*effect).numOutputs = 1;
        (*effect).flags = PluginFlags::CAN_REPLACING.bits();
    }
    let (mut instance, main) = instance_over(effect);
    let mut handle = instance.render_handle(main);
    let block = |handle: &mut RenderHandle| {
        let mut host: HostBuffer<f32> = HostBuffer::new(0, 1);
        let none: Vec<Vec<f32>> = Vec::new();
        let mut out = vec![vec![9.0f32; 4]];
        handle.process(&mut host.bind(&none, &mut out));
        out
    };

    // Part one: a render is in flight when the exclusive call is made. The call waits.
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    let renderer = std::thread::spawn(move || {
        let out = block(&mut handle);
        handle_tx.send(handle).unwrap();
        out
    });
    while !IN_PROCESS.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    let watcher = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(150));
        let early = RATE_DISPATCHED.load(Ordering::SeqCst);
        RELEASE_PROCESS.store(true, Ordering::SeqCst);
        early
    });
    RELEASE_RATE.store(true, Ordering::SeqCst);
    instance.set_sample_rate(main, 48_000.0);
    assert!(
        !watcher.join().unwrap(),
        "the exclusive call ran while a render was in flight"
    );
    assert!(RATE_DISPATCHED.load(Ordering::SeqCst), "and it ran after");
    assert_eq!(renderer.join().unwrap()[0][0], 1.0);
    let mut handle = handle_rx.recv().unwrap();

    // Part two: a render starts while the exclusive call runs. It is refused.
    RATE_DISPATCHED.store(false, Ordering::SeqCst);
    RELEASE_RATE.store(false, Ordering::SeqCst);
    RELEASE_PROCESS.store(true, Ordering::SeqCst);
    let before = PROCESSES.load(Ordering::SeqCst);
    let during = std::thread::spawn(move || {
        while !IN_RATE.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let out = block(&mut handle);
        RELEASE_RATE.store(true, Ordering::SeqCst);
        out
    });
    instance.set_sample_rate(main, 44_100.0);
    let out = during.join().unwrap();
    assert_eq!(
        PROCESSES.load(Ordering::SeqCst),
        before,
        "a render reached the plugin during an exclusive call"
    );
    assert!(
        out[0].iter().all(|s| *s == 0.0),
        "and was refused to silence"
    );
}

/// **A token-free dispatcher read runs beside `process`, both ways** (`get_parameter_name`
/// stands for the named exceptions: name, label, display, properties, `effCanBeAutomated`,
/// `effString2Parameter`). The 2.4 headers state no exclusion between these opcodes and a
/// render, so a UI read of a parameter's text must neither wait for a block nor make one
/// refuse: a block rendered as silence, with its MIDI and automation dropped, because the
/// UI asked for a string is the defect.
#[cfg(unix)]
#[test]
fn a_parameter_name_and_process_run_beside_each_other() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static IN_PROCESS: AtomicBool = AtomicBool::new(false);
    static RELEASE_PROCESS: AtomicBool = AtomicBool::new(false);
    static NAME_DISPATCHED: AtomicBool = AtomicBool::new(false);
    static IN_NAME: AtomicBool = AtomicBool::new(false);
    static RELEASE_NAME: AtomicBool = AtomicBool::new(false);

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::GetParameterName as i32 {
            NAME_DISPATCHED.store(true, Ordering::SeqCst);
            IN_NAME.store(true, Ordering::SeqCst);
            while !RELEASE_NAME.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            IN_NAME.store(false, Ordering::SeqCst);
        }
        0
    }
    extern "C" fn render(_: *mut AEffect, _: *const *const f32, outputs: *mut *mut f32, n: i32) {
        IN_PROCESS.store(true, Ordering::SeqCst);
        while !RELEASE_PROCESS.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        // SAFETY: the test's buffer has one output channel of `n` samples.
        unsafe { std::slice::from_raw_parts_mut(*outputs, n as usize) }.fill(1.0);
        IN_PROCESS.store(false, Ordering::SeqCst);
    }

    let effect = fake_effect(dispatch);
    // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
    unsafe {
        (*effect).processReplacing = Some(render);
        (*effect).numOutputs = 1;
        (*effect).flags = PluginFlags::CAN_REPLACING.bits();
    }
    let (instance, main) = instance_over(effect);
    let mut handle = instance.render_handle(main);
    let block = |handle: &mut RenderHandle| {
        let mut host: HostBuffer<f32> = HostBuffer::new(0, 1);
        let none: Vec<Vec<f32>> = Vec::new();
        let mut out = vec![vec![9.0f32; 4]];
        handle.process(&mut host.bind(&none, &mut out));
        out
    };

    // Part one: a name call is in flight (inside the plugin, on this thread); a block
    // starts on another and renders.
    RELEASE_PROCESS.store(true, Ordering::SeqCst);
    let (name, out) = std::thread::scope(|scope| {
        let renderer = scope.spawn(|| {
            while !IN_NAME.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            let out = block(&mut handle);
            RELEASE_NAME.store(true, Ordering::SeqCst);
            out
        });
        let name = instance.get_parameter_name(0);
        (name, renderer.join().unwrap())
    });
    assert_eq!(name, "");
    assert_eq!(
        out[0][0], 1.0,
        "a block refused (silence) because a parameter name was being read"
    );

    // Part two: a block is in flight (inside the plugin); a name call is dispatched and
    // returns without waiting for it.
    NAME_DISPATCHED.store(false, Ordering::SeqCst);
    RELEASE_PROCESS.store(false, Ordering::SeqCst);
    let out = std::thread::scope(|scope| {
        let renderer = scope.spawn(|| block(&mut handle));
        while !IN_PROCESS.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let _ = instance.get_parameter_name(0);
        assert!(
            NAME_DISPATCHED.load(Ordering::SeqCst) && IN_PROCESS.load(Ordering::SeqCst),
            "the name call waited for the block in flight"
        );
        RELEASE_PROCESS.store(true, Ordering::SeqCst);
        renderer.join().unwrap()
    });
    assert_eq!(out[0][0], 1.0);
}

/// **A plugin that raises its pin counts after activation is not rendered into
/// a buffer sized for the old ones.** Its `processReplacing` indexes the
/// channel tables by its live `numOutputs`; one more than the host sized is a
/// read past the table. The handle renders silence instead, and renders again
/// when the counts fit.
#[cfg(unix)]
#[test]
fn a_plugin_that_raises_its_pins_is_not_rendered() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RENDERS: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn dispatch(
        _: *mut AEffect,
        _: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        0
    }
    extern "C" fn render_f32(
        _: *mut AEffect,
        _: *const *const f32,
        outputs: *mut *mut f32,
        frames: i32,
    ) {
        RENDERS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the test's buffer has two output channels of `frames` samples.
        unsafe { std::slice::from_raw_parts_mut(*outputs, frames as usize) }.fill(1.0);
    }
    extern "C" fn render_f64(
        _: *mut AEffect,
        _: *const *const f64,
        outputs: *mut *mut f64,
        frames: i32,
    ) {
        RENDERS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: as for `render_f32`.
        unsafe { std::slice::from_raw_parts_mut(*outputs, frames as usize) }.fill(1.0);
    }

    let effect = fake_effect(dispatch);
    // SAFETY: the test's leaked `AEffect`; the instance reads it, nothing writes
    // concurrently.
    unsafe {
        (*effect).numInputs = 2;
        (*effect).numOutputs = 2;
        (*effect).processReplacing = Some(render_f32);
        (*effect).processReplacingF64 = Some(render_f64);
        (*effect).flags = (PluginFlags::CAN_REPLACING | PluginFlags::CAN_DOUBLE_REPLACING).bits();
    }
    let (instance, main) = instance_over(effect);
    let mut render = instance.render_handle(main);

    let inputs = vec![vec![0.0f32; 8]; 2];
    let mut outputs = vec![vec![9.0f32; 8]; 2];
    let inputs64 = vec![vec![0.0f64; 8]; 2];
    let mut outputs64 = vec![vec![9.0f64; 8]; 2];
    let mut host32: HostBuffer<f32> = HostBuffer::new(2, 2);
    let mut host64: HostBuffer<f64> = HostBuffer::new(2, 2);

    // The counts it was activated with: rendered.
    render.process(&mut host32.bind(&inputs, &mut outputs));
    render.process_f64(&mut host64.bind(&inputs64, &mut outputs64));
    assert_eq!(RENDERS.load(Ordering::SeqCst), 2);
    assert_eq!(outputs[0][0], 1.0);

    // It raises its output pins, as `audioMasterIOChanged` announces.
    // SAFETY: the test's leaked `AEffect`; no other thread.
    unsafe { (*effect).numOutputs = 3 };
    outputs.iter_mut().for_each(|c| c.fill(9.0));
    outputs64.iter_mut().for_each(|c| c.fill(9.0));
    // The refusal is allocation-free and logs nothing: it runs every block on the
    // audio thread, and is counted for the control side instead.
    assert_no_alloc::assert_no_alloc(|| {
        render.process(&mut host32.bind(&inputs, &mut outputs));
        render.process_f64(&mut host64.bind(&inputs64, &mut outputs64));
    });
    assert_eq!(RENDERS.load(Ordering::SeqCst), 2, "called past its buffer");
    assert_eq!(
        instance.pin_refusals().get(),
        2,
        "both refused blocks are counted"
    );
    instance.report_pin_refusals();
    assert!(outputs.iter().flatten().all(|s| *s == 0.0), "f32 is silent");
    assert!(
        outputs64.iter().flatten().all(|s| *s == 0.0),
        "f64 is silent"
    );

    // Fewer pins than the buffer holds is fine.
    // SAFETY: as above.
    unsafe { (*effect).numOutputs = 1 };
    render.process(&mut host32.bind(&inputs, &mut outputs));
    assert_eq!(RENDERS.load(Ordering::SeqCst), 3);
    assert_eq!(
        instance.pin_refusals().get(),
        2,
        "a fitting block is not counted"
    );
}

/// A loader over `main`, for the entry-point tests.
#[cfg(unix)]
fn loader_over(main: PluginMain, id: i32) -> PluginLoader<IdHost> {
    let lib: Library = libloading::os::unix::Library::this().into();
    PluginLoader {
        main,
        lib: Arc::new(lib),
        host: Arc::new(IdHost(id)),
    }
}

/// **A plugin with no render entry is refused at load**, not rendered silent per block:
/// neither `processReplacing` nor `process` is `NoProcessEntry`, and the refused plugin
/// is closed. One with only the deprecated `process` loads, and so does one that sets
/// `effFlagsCanReplacing` over a null `processReplacing` but has `process`.
#[cfg(unix)]
#[test]
fn a_plugin_with_no_render_entry_is_refused_at_load() {
    use std::sync::atomic::{AtomicPtr, AtomicUsize};
    static EFFECT: AtomicPtr<AEffect> = AtomicPtr::new(ptr::null_mut());
    static CLOSES: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::Shutdown as i32 {
            CLOSES.fetch_add(1, Ordering::SeqCst);
        }
        if opcode == plugin::EffectOpCode::GetApiVersion as i32 {
            return 2400;
        }
        0
    }
    extern "C" fn main_returning_effect(_: crate::api::HostCallbackProc) -> *mut AEffect {
        EFFECT.load(Ordering::SeqCst)
    }
    extern "C" fn render(_: *mut AEffect, _: *const *const f32, _: *mut *mut f32, _: i32) {}
    let load = |effect: *mut AEffect| {
        EFFECT.store(effect, Ordering::SeqCst);
        loader_over(main_returning_effect, 1).instance(tutti_fixtures::main_thread(), &detached())
    };

    // Neither entry: refused, and closed.
    let effect = fake_effect(dispatch);
    // SAFETY: the test's leaked `AEffect`.
    unsafe { (*effect)._process = None };
    let result = load(effect);
    assert!(matches!(result, Err(PluginLoadError::NoProcessEntry)));
    assert_eq!(
        CLOSES.load(Ordering::SeqCst),
        1,
        "the refused plugin is closed"
    );

    // Only the deprecated `process`: loads.
    let effect = fake_effect(dispatch);
    // SAFETY: as above.
    unsafe { (*effect)._process = Some(render) };
    drop(load(effect).expect("a plugin with only `process` loads"));
    assert_eq!(CLOSES.load(Ordering::SeqCst), 2);

    // `effFlagsCanReplacing` over a null `processReplacing`, with `process`: loads.
    let effect = fake_effect(dispatch);
    // SAFETY: as above.
    unsafe {
        (*effect).flags = PluginFlags::CAN_REPLACING.bits();
        (*effect).processReplacing = None;
        (*effect)._process = Some(render);
    }
    drop(load(effect).expect("a null processReplacing falls back to `process`"));

    // The flag over a null `processReplacing` and no `process`: refused.
    let effect = fake_effect(dispatch);
    // SAFETY: as above.
    unsafe {
        (*effect).flags = PluginFlags::CAN_REPLACING.bits();
        (*effect)._process = None;
    }
    assert!(matches!(load(effect), Err(PluginLoadError::NoProcessEntry)));
}

/// **Each refusal of an audio-thread entry is counted in its own tally.** A closed gate,
/// a missing entry (`f64` render, `setParameter`) and a cleared dispatcher, whether an
/// audio-thread entry or a main-thread opcode reaches it.
#[test]
fn each_refused_entry_is_counted_apart() {
    extern "C" fn dispatch(
        _: *mut AEffect,
        _: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        0
    }
    extern "C" fn render(_: *mut AEffect, _: *const *const f32, _: *mut *mut f32, _: i32) {}
    let inputs = vec![vec![0.0f32; 8]; 2];
    let mut outputs = vec![vec![0.0f32; 8]; 2];
    let inputs64 = vec![vec![0.0f64; 8]; 2];
    let mut outputs64 = vec![vec![0.0f64; 8]; 2];
    let mut host32: HostBuffer<f32> = HostBuffer::new(2, 2);
    let mut host64: HostBuffer<f64> = HostBuffer::new(2, 2);
    let no_events = api::Events {
        num_events: 0,
        _reserved: 0,
        events: [ptr::null_mut(); 2],
    };

    // A plugin with `processReplacing` but no `f64` entry and no `setParameter`.
    let effect = fake_effect(dispatch);
    // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
    unsafe {
        (*effect).processReplacing = Some(render);
        (*effect).flags = PluginFlags::CAN_REPLACING.bits();
    }
    let (mut instance, main) = instance_over(effect);
    let mut handle = instance.render_handle(main);
    handle.process(&mut host32.bind(&inputs, &mut outputs));
    assert_eq!(instance.gate_refusals().get(), 0);
    assert_eq!(instance.entry_refusals().get(), 0);
    handle.process_f64(&mut host64.bind(&inputs64, &mut outputs64));
    assert_eq!(instance.entry_refusals().get(), 1, "no f64 entry");
    assert!(!handle.set_parameter(0, 0.5));
    assert_eq!(instance.entry_refusals().get(), 2, "a null setParameter");

    // A cleared dispatcher: the event list and the process start do not reach it.
    // SAFETY: as above; no other thread.
    unsafe { (*effect).dispatcher = None };
    handle.process_events(&no_events);
    assert_eq!(instance.dispatcher_cleared().get(), 1);
    handle.start_process();
    assert_eq!(instance.dispatcher_cleared().get(), 2);
    // A main-thread opcode reads 0 and is counted in the same tally: two here.
    instance.with_preset_bracket(main, |_| ());
    assert_eq!(instance.dispatcher_cleared().get(), 4);
    assert_eq!(instance.gate_refusals().get(), 0, "the gate was open");
    assert_eq!(instance.pin_refusals().get(), 0);
    // The dispatcher is restored for the close.
    // SAFETY: as above.
    unsafe { (*effect).dispatcher = Some(dispatch) };

    // A closed gate: the instance is gone, the handle's entries are refused.
    let gate = instance.gate_refusals().clone();
    drop(instance);
    handle.process(&mut host32.bind(&inputs, &mut outputs));
    assert_eq!(gate.get(), 1, "a block after the close");
    handle.process_events(&no_events);
    assert_eq!(gate.get(), 2, "an event list after the close");
}

/// **An `AEffect` that is not one is refused untouched.** A wrong `magic` or a
/// null `dispatcher` is `InvalidEffect`: nothing is called on it (not even
/// `effClose`) and `reserved1` is left alone. A valid one loads, and closes
/// once.
#[cfg(unix)]
#[test]
fn an_invalid_aeffect_is_refused_untouched() {
    use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static CLOSES: AtomicUsize = AtomicUsize::new(0);
    static EFFECT: AtomicPtr<AEffect> = AtomicPtr::new(ptr::null_mut());

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        CALLS.fetch_add(1, Ordering::SeqCst);
        if opcode == plugin::EffectOpCode::Shutdown as i32 {
            CLOSES.fetch_add(1, Ordering::SeqCst);
        }
        if opcode == plugin::EffectOpCode::GetApiVersion as i32 {
            return 2400;
        }
        0
    }
    extern "C" fn main_returning_effect(_: crate::api::HostCallbackProc) -> *mut AEffect {
        EFFECT.load(Ordering::SeqCst)
    }

    // A wrong magic.
    let effect = fake_effect(dispatch);
    // SAFETY: the test's leaked `AEffect`.
    unsafe { (*effect).magic = 0x1234 };
    EFFECT.store(effect, Ordering::SeqCst);
    let result =
        loader_over(main_returning_effect, 1).instance(tutti_fixtures::main_thread(), &detached());
    assert!(matches!(result, Err(PluginLoadError::InvalidEffect)));
    assert_eq!(CALLS.load(Ordering::SeqCst), 0, "a bad AEffect was called");
    // SAFETY: as above.
    assert_eq!(unsafe { (*effect).reserved1 }, 0, "reserved1 was written");

    // No dispatcher.
    let effect = fake_effect(dispatch);
    // SAFETY: as above.
    unsafe { (*effect).dispatcher = None };
    EFFECT.store(effect, Ordering::SeqCst);
    let result =
        loader_over(main_returning_effect, 1).instance(tutti_fixtures::main_thread(), &detached());
    assert!(matches!(result, Err(PluginLoadError::InvalidEffect)));
    assert_eq!(CALLS.load(Ordering::SeqCst), 0);

    // A valid one.
    let effect = fake_effect(dispatch);
    EFFECT.store(effect, Ordering::SeqCst);
    let instance = loader_over(main_returning_effect, 1)
        .instance(tutti_fixtures::main_thread(), &detached())
        .expect("a valid AEffect loads");
    assert_eq!(CLOSES.load(Ordering::SeqCst), 0);
    drop(instance);
    assert_eq!(CLOSES.load(Ordering::SeqCst), 1);
}

/// **A callback during `main` reaches the loader's host whatever the plugin
/// left in `AEffect::reserved1`.** The field is "reserved for host", and a
/// plugin that does not zero its `AEffect` before calling back from `main`
/// leaves garbage in it, which was dereferenced as the host box.
#[cfg(unix)]
#[test]
fn a_callback_in_main_ignores_a_garbage_reserved1() {
    extern "C" fn garbage_main(callback: crate::api::HostCallbackProc) -> *mut AEffect {
        // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
        let mut effect: AEffect = unsafe { std::mem::zeroed() };
        effect.reserved1 = 0xdead_beef;
        // SAFETY: `callback` is `callback_wrapper`; opcode 2 is `audioMasterCurrentId`.
        let answer = unsafe { callback(&mut effect, 2, 0, 0, ptr::null_mut(), 0.0) };
        ANSWERED.set(answer);
        ptr::null_mut()
    }
    let mut loader = loader_over(garbage_main, 7);
    assert!(loader.call_main().is_null());
    assert_eq!(
        ANSWERED.get(),
        7,
        "the call did not reach the loader's host"
    );
}

/// **A thread the plugin spawns during `main` cannot make the host dereference a
/// garbage `reserved1`.** `LOAD_HOST` is thread-local, so such a thread has none;
/// with a load in progress it gets the load-time default (`0`) unless the effect it
/// passes is one the host adopted, in which case it reaches that effect's own host
/// (an adopted plugin's audio thread keeps working through someone else's load).
#[cfg(unix)]
#[test]
fn a_spawned_thread_during_main_cannot_reach_a_garbage_reserved1() {
    use std::sync::atomic::{AtomicIsize, AtomicPtr, AtomicUsize, Ordering};
    static GARBAGE_ANSWER: AtomicIsize = AtomicIsize::new(-1);
    static ADOPTED_ANSWER: AtomicIsize = AtomicIsize::new(-1);
    static OTHER: AtomicPtr<AEffect> = AtomicPtr::new(ptr::null_mut());
    static CALLS: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::GetApiVersion as i32 {
            return 2400;
        }
        0
    }
    extern "C" fn first_main(_: crate::api::HostCallbackProc) -> *mut AEffect {
        CALLS.fetch_add(1, Ordering::SeqCst);
        fake_effect(dispatch)
    }
    // A plugin whose own thread calls the host while `main` runs: once with an
    // `AEffect` of garbage, once with the first plugin's adopted effect.
    extern "C" fn second_main(callback: crate::api::HostCallbackProc) -> *mut AEffect {
        let other = OTHER.load(Ordering::SeqCst) as usize;
        std::thread::spawn(move || {
            // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
            let mut garbage: AEffect = unsafe { std::mem::zeroed() };
            garbage.reserved1 = 0xdead_beef;
            // SAFETY: `callback` is `callback_wrapper`; opcode 2 is `audioMasterCurrentId`.
            let a = unsafe { callback(&mut garbage, 2, 0, 0, ptr::null_mut(), 0.0) };
            GARBAGE_ANSWER.store(a, Ordering::SeqCst);
            // SAFETY: as above, with an effect the first load adopted.
            let b = unsafe { callback(other as *mut AEffect, 2, 0, 0, ptr::null_mut(), 0.0) };
            ADOPTED_ANSWER.store(b, Ordering::SeqCst);
        })
        .join()
        .unwrap();
        ptr::null_mut()
    }

    let first = loader_over(first_main, 5)
        .instance(tutti_fixtures::main_thread(), &detached())
        .expect("the first plugin loads");
    OTHER.store(first.core.get_effect(), Ordering::SeqCst);
    let second = loader_over(second_main, 9).instance(tutti_fixtures::main_thread(), &detached());
    assert!(matches!(second, Err(PluginLoadError::InstanceFailed)));
    assert_eq!(
        GARBAGE_ANSWER.load(Ordering::SeqCst),
        0,
        "a garbage effect got a host's answer"
    );
    assert_eq!(
        ADOPTED_ANSWER.load(Ordering::SeqCst),
        5,
        "an adopted effect did not reach its own host"
    );
    drop(first);
}

/// **A callback from inside a render takes no lock and allocates nothing, even
/// while another plugin loads.** The render entry marks the effect it is calling
/// (`RENDERING`), so `callback_wrapper` does not consult the registry mutex for it.
#[cfg(unix)]
#[test]
fn a_callback_from_a_render_takes_no_lock_during_a_load() {
    use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
    static ANSWER: AtomicIsize = AtomicIsize::new(-1);
    static RENDERS: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::GetApiVersion as i32 {
            return 2400;
        }
        0
    }
    extern "C" fn render(effect: *mut AEffect, _: *const *const f32, _: *mut *mut f32, _: i32) {
        RENDERS.fetch_add(1, Ordering::SeqCst);
        // The plugin asks the host something from inside `processReplacing`:
        // opcode 2 is `audioMasterCurrentId`.
        ANSWER.store(
            callback_wrapper::<IdHost>(effect, 2, 0, 0, ptr::null_mut(), 0.0),
            Ordering::SeqCst,
        );
    }
    extern "C" fn main_fn(_: crate::api::HostCallbackProc) -> *mut AEffect {
        let effect = fake_effect(dispatch);
        // SAFETY: the test's leaked `AEffect`.
        unsafe {
            (*effect).processReplacing = Some(render);
            (*effect).flags = PluginFlags::CAN_REPLACING.bits();
        }
        effect
    }

    let instance = loader_over(main_fn, 5)
        .instance(tutti_fixtures::main_thread(), &detached())
        .expect("loads");
    let mut handle = instance.render_handle(tutti_fixtures::main_thread());
    let mut host32: HostBuffer<f32> = HostBuffer::new(0, 0);
    let none: Vec<Vec<f32>> = Vec::new();
    let mut none_out: Vec<Vec<f32>> = Vec::new();

    // Another plugin is loading.
    let _loading = Loading::begin();
    let before = REGISTRY_LOCKS.load(Ordering::SeqCst);
    assert_no_alloc::assert_no_alloc(|| {
        handle.process(&mut host32.bind(&none, &mut none_out));
    });
    assert_eq!(RENDERS.load(Ordering::SeqCst), 1);
    assert_eq!(
        ANSWER.load(Ordering::SeqCst),
        5,
        "the callback was not answered"
    );
    assert_eq!(
        REGISTRY_LOCKS.load(Ordering::SeqCst),
        before,
        "the render path took the registry lock"
    );
}

/// **A plugin that writes through `effSetChunk`'s pointer writes into a copy.**
/// The opcode's pointer is `void*`; the caller's slice must not be handed over.
#[test]
fn a_chunk_a_plugin_writes_into_is_a_copy() {
    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        value: isize,
        ptr: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::SetData as i32 {
            // SAFETY: the host gives `value` writable bytes (the property under
            // test: a copy it owns).
            unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, value as usize) }.fill(0xEE);
            return 1;
        }
        0
    }
    let (params, main) = instance_over(fake_effect(dispatch));
    let data = vec![1u8, 2, 3, 4];
    params.load_preset_data(main, &data);
    assert_eq!(
        data,
        vec![1, 2, 3, 4],
        "the plugin wrote into the caller's bytes"
    );
}

/// A name a plugin writes past the spec's length (real plugins do) lands in a
/// buffer that holds it, and comes back whole.
#[test]
fn a_name_longer_than_the_spec_fits_the_buffer() {
    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        _: isize,
        ptr: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode == plugin::EffectOpCode::GetParameterName as i32 {
            // SAFETY: the host offers at least 256 bytes (the property under test).
            let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, 200) };
            buf.fill(b'x');
        }
        0
    }
    let (params, _main) = instance_over(fake_effect(dispatch));
    assert_eq!(params.get_parameter_name(0).len(), 200);
}

/// `effGetSpeakerArrangement` is pointer to pointer: the plugin points the two slots the
/// host passes at arrangements it owns, and the host reads what they point at. A plugin
/// returning 0 has none to report, and one returning 1 with a null slot or an unnamed
/// speaker has written something that is not an arrangement.
#[test]
fn a_speaker_arrangement_is_read_through_the_slots_the_plugin_points() {
    use crate::speaker::{ArrangementRead, Speaker, SpeakerArrangement};
    use std::sync::atomic::{AtomicIsize, Ordering};
    static STEREO: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    static MODE: AtomicIsize = AtomicIsize::new(0);

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        value: isize,
        ptr: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode != plugin::EffectOpCode::GetSpeakerArrangement as i32 {
            return 0;
        }
        let stereo = STEREO.get().expect("set before the call");
        match MODE.load(Ordering::SeqCst) {
            0 => 0,
            // SAFETY: the host passes the addresses of two pointer slots (the property
            // under test).
            1 => unsafe {
                *(value as *mut *const u32) = stereo.as_ptr();
                *ptr.cast::<*const u32>() = stereo.as_ptr();
                1
            },
            _ => 1,
        }
    }
    STEREO
        .set(SpeakerArrangement::new(vec![Speaker::Left, Speaker::Right]).encode())
        .unwrap();
    let (instance, main) = instance_over(fake_effect(dispatch));

    MODE.store(0, Ordering::SeqCst);
    assert_eq!(
        instance.speaker_arrangement(main),
        Err(ArrangementRead::Unanswered)
    );

    MODE.store(1, Ordering::SeqCst);
    let stereo = SpeakerArrangement::new(vec![Speaker::Left, Speaker::Right]);
    assert_eq!(
        instance.speaker_arrangement(main),
        Ok((stereo.clone(), stereo))
    );

    MODE.store(2, Ordering::SeqCst);
    assert_eq!(
        instance.speaker_arrangement(main),
        Err(ArrangementRead::Malformed)
    );
}

/// A proposal reaches the plugin as the two `VstSpeakerArrangement`s in `value` and `ptr`
/// while it is suspended, and is not sent once it is resumed.
#[test]
fn a_speaker_arrangement_is_proposed_only_while_suspended() {
    use crate::speaker::{Speaker, SpeakerArrangement};
    use tutti_rt::Mutex;
    static SEEN: Mutex<Vec<(Vec<u32>, Vec<u32>)>> = Mutex::new(Vec::new());

    extern "C" fn dispatch(
        _: *mut AEffect,
        opcode: i32,
        _: i32,
        value: isize,
        ptr: *mut c_void,
        _: f32,
    ) -> isize {
        if opcode != plugin::EffectOpCode::SetSpeakerArrangement as i32 {
            return 0;
        }
        // SAFETY: the host passes two arrangements of the channels it proposes (the
        // property under test): mono and stereo here, 2 + 28 and 2 + 56 words.
        let (input, output) = unsafe {
            (
                std::slice::from_raw_parts(value as *const u32, 2 + 28),
                std::slice::from_raw_parts(ptr as *const u32, 2 + 56),
            )
        };
        SEEN.lock().push((input.to_vec(), output.to_vec()));
        1
    }
    let mono = SpeakerArrangement::new(vec![Speaker::Mono]);
    let stereo = SpeakerArrangement::new(vec![Speaker::Left, Speaker::Right]);
    let (mut instance, main) = instance_over(fake_effect(dispatch));

    assert!(instance.set_speaker_arrangement(main, &mono, &stereo));
    let seen = std::mem::take(&mut *SEEN.lock());
    assert_eq!(seen, vec![(mono.encode(), stereo.encode())]);
    assert_eq!(
        (seen[0].0[0], seen[0].0[1]),
        (0, 1),
        "kSpeakerArrMono, one channel"
    );
    assert_eq!(
        (seen[0].1[0], seen[0].1[1]),
        (1, 2),
        "kSpeakerArrStereo, two channels"
    );

    instance.resume(main);
    assert!(!instance.set_speaker_arrangement(main, &mono, &stereo));
    assert!(SEEN.lock().is_empty(), "a resumed plugin is sent nothing");
}
