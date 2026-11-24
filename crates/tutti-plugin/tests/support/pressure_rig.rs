//! The rig the pressure test and the pressure bench share: real out-of-process
//! plugins as the only node of a graph each, driven with planar buffers.

#![allow(dead_code)]

use crate::clap_probe;
use std::time::Duration;

/// Planar scratch, `channels` × [`BLOCK`] frames: what this harness fills and
/// measures.
pub(crate) struct Planes(pub Vec<Vec<f32>>);

impl Planes {
    pub(crate) fn new(channels: usize) -> Self {
        Self(vec![vec![0.0; BLOCK]; channels])
    }

    pub(crate) fn clear(&mut self) {
        for c in &mut self.0 {
            c.fill(0.0);
        }
    }

    pub(crate) fn at(&self, channel: usize, i: usize) -> f32 {
        self.0[channel][i]
    }
}

/// A loaded plugin as the only node of a graph, driven with the
/// buffer types this suite fills and measures (`fill_sine`, `peak`).
///
/// A block reaches the plugin node through the executor, with the `Env` the
/// engine would give it.
/// The main inputs feed its inputs, its outputs feed the main outputs.
pub(crate) struct GraphUnit {
    renderer: tutti_graph::Renderer,
    /// The plugin's controls: whether the server has answered its chunk.
    pub controls: tutti_plugin_host::PluginControls,
    /// Whether this plugin is stalled on every block ([`load_stalled`]): it
    /// never answers in time, so a driver does not wait for it.
    pub stalled: bool,
    inputs: usize,
    outputs: usize,
    ins: Vec<Vec<f32>>,
    outs: Vec<Vec<f32>>,
}

impl GraphUnit {
    pub(crate) fn new<N: tutti_graph::IntoNode<Controls = tutti_plugin_host::PluginControls>>(
        node: N,
        inputs: usize,
        outputs: usize,
    ) -> Self {
        assert!(
            inputs <= 16 && outputs <= 16,
            "`run` holds 16 ports on the stack"
        );
        let layout =
            |n: usize| tutti_buffers::ChannelLayout::from_count(u16::try_from(n).expect("ports"));
        let mut g = tutti_graph::GraphBuilder::new(layout(inputs), layout(outputs));
        let (key, controls) = g.add_with_controls(node);
        g.pipe_input(key).pipe_output(key);
        let renderer = g
            .renderer(tutti_graph::Prepare::new(
                tutti_units::SampleRate::of(SAMPLE_RATE),
                tutti_buffers::MaxBlock::of(tutti_units::Samples::new(BLOCK)),
            ))
            .expect("a one-plugin graph compiles");
        Self {
            renderer,
            controls,
            stalled: false,
            inputs,
            outputs,
            ins: vec![vec![0.0; BLOCK]; inputs],
            outs: vec![vec![0.0; BLOCK]; outputs],
        }
    }

    pub(crate) fn inputs(&self) -> usize {
        self.inputs
    }

    pub(crate) fn outputs(&self) -> usize {
        self.outputs
    }

    /// One block: `input`'s first `inputs` channels in, the plugin's outputs
    /// into `output`'s first `outputs` channels. [`stage`](Self::stage),
    /// [`run`](Self::run) and [`read`](Self::read) in one, for the untimed
    /// callers.
    pub(crate) fn process(&mut self, size: usize, input: &Planes, output: &mut Planes) {
        self.stage(size, input);
        self.run(size);
        self.read(size, output);
    }

    /// Copy `input` into this unit's planar input buffers. Not the plugin
    /// path: outside a timed region.
    pub(crate) fn stage(&mut self, size: usize, input: &Planes) {
        for (c, ch) in self.ins.iter_mut().enumerate() {
            for (i, s) in ch[..size].iter_mut().enumerate() {
                *s = input.at(c, i);
            }
        }
    }

    /// The plugin path, and nothing else: one executor block over the staged
    /// buffers, the port tables on the stack. What a timed region measures.
    pub(crate) fn run(&mut self, size: usize) {
        const MAX: usize = 16;
        let mut ins: [&[f32]; MAX] = [&[]; MAX];
        for (slot, c) in ins.iter_mut().zip(&self.ins) {
            *slot = &c[..size];
        }
        let mut outs: [&mut [f32]; MAX] = std::array::from_fn(|_| Default::default());
        for (slot, c) in outs.iter_mut().zip(self.outs.iter_mut()) {
            *slot = &mut c[..size];
        }
        let transport = tutti_graph::TransportPosition::default();
        self.renderer.executor_mut().process(
            size,
            &transport,
            &ins[..self.inputs],
            &mut outs[..self.outputs],
        );
    }

    /// Copy the last block's output out, and drain what the executor sent
    /// back. Outside a timed region.
    pub(crate) fn read(&mut self, size: usize, output: &mut Planes) {
        self.renderer.editor_mut().collect();
        for (c, ch) in self.outs.iter().enumerate() {
            for (i, &s) in ch[..size].iter().enumerate() {
                output.0[c][i] = s;
            }
        }
    }
}

pub(crate) const SAMPLE_RATE: f64 = 48_000.0;
pub(crate) const BLOCK: usize = 64;

/// `count` units of the reference CLAP probe under `env`, through the real
/// `plugin-server` subprocess and shared-memory bridge.
///
/// Returns the units and their handles; the handles must outlive the units
/// (they share the subprocess guard), so the caller keeps both. The probe
/// reads `env` on load, in the subprocess, having inherited it at the spawn;
/// it is held across the whole loop, so every instance gets it.
fn load_units(
    count: usize,
    env: clap_probe::ProbeEnv,
    stalled: bool,
) -> (Vec<GraphUnit>, Vec<tutti_plugin_host::PluginHandle>) {
    let _env = env;
    let mut units: Vec<GraphUnit> = Vec::with_capacity(count);
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let probe = clap_probe::load_probe(SAMPLE_RATE);
        let (ins, outs) = (probe.client.inputs(), probe.client.outputs());
        let mut unit = GraphUnit::new(
            probe.client.bind(
                tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0))
                    .expect("a tempo"),
            ),
            ins,
            outs,
        );
        unit.stalled = stalled;
        units.push(unit);
        handles.push(probe.handle);
    }
    (units, handles)
}

/// `count` instances of the **reference CLAP probe**, answering in time.
///
/// **Read the resulting numbers as a bridge measurement, not a plugin
/// one.** The probe is a trivial fixture, so the per-block cost here is the
/// cost of the submit/collect pipeline, the socket and the shm slab, with
/// almost no DSP under it. That is the right quantity for "does hosting
/// out-of-process cost anything".
///
/// **The probe defaults to `RenderMode::Inert`, which writes nothing.**
/// Left at the default it reports `non-silent 0/N` on a perfectly healthy
/// bridge, and every throughput figure becomes a measurement of the fixture.
/// `TAG_PASSTHROUGH` writes `in + tag`, so the output is both non-silent and
/// *provably derived from the input* — a synth that ignored its input could
/// not show that audio traversed the graph.
pub(crate) fn load_n(count: usize) -> (Vec<GraphUnit>, Vec<tutti_plugin_host::PluginHandle>) {
    load_units(
        count,
        clap_probe::ProbeEnv::new().render_mode(clap_probe::render::TAG_PASSTHROUGH),
        false,
    )
}

/// A sine block at full-ish scale, so "audio arrived" is unambiguous.
pub(crate) fn fill_sine(input: &mut Planes, channels: usize, block_index: usize) {
    for plane in &mut input.0[..channels] {
        for (i, y) in plane[..BLOCK].iter_mut().enumerate() {
            let t = (block_index * BLOCK + i) as f32 / SAMPLE_RATE as f32;
            *y = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
        }
    }
}

pub(crate) fn peak(buf: &Planes, channels: usize) -> f32 {
    (0..channels)
        .map(|ch| {
            (0..BLOCK)
                .map(|i| buf.at(ch, i).abs())
                .fold(0.0f32, f32::max)
        })
        .fold(0.0f32, f32::max)
}

/// How late a stalled probe answers every block: far beyond any reply budget
/// (the bridge's is four block periods, 5.3 ms; the synchronous design's was
/// [`SYNC_WAIT_BUDGET`]), so the plugin never publishes in time and a design
/// that waits for it waits its whole budget. A missed budget costs the block,
/// not the connection (see `dispatch`'s reply timeout), so the probe stays
/// live throughout.
///
/// Far beyond the *block* too, and by more than a little: a stalled plugin
/// that publishes in time for the next block is not stalled for it. With
/// 10 ms, bursty load on the test's core stretched the paced blocks enough
/// that one stalled probe carried audio in 116 of 200, and a waiting design
/// pays nothing on those.
pub(crate) const STALL: Duration = Duration::from_millis(500);

/// `count` instances of the reference CLAP probe, each stalled by [`STALL`]
/// on every block: alive, answering, and always late.
///
/// Stalled by a sleep (`stall_every_block`), not parked with `block_from`:
/// a parked probe spins, `plugin-server` runs its audio thread at raised
/// priority (nice -20 here), and eight spinning servers would take the host's
/// cores, which is the time these tests measure.
pub(crate) fn load_stalled(count: usize) -> (Vec<GraphUnit>, Vec<tutti_plugin_host::PluginHandle>) {
    load_units(
        count,
        clap_probe::ProbeEnv::new()
            .render_mode(clap_probe::render::TAG_PASSTHROUGH)
            .stall_every_block(STALL),
        true,
    )
}

/// `pairs` stalled probes ([`load_stalled`]) and `pairs` answering plugins
/// ([`load_n`]), alternating: stalled at even positions, answering at odd.
pub(crate) fn load_half_stalled(
    pairs: usize,
) -> (Vec<GraphUnit>, Vec<tutti_plugin_host::PluginHandle>) {
    let (stalled, mut handles) = load_stalled(pairs);
    let (answering, answering_handles) = load_n(pairs);
    handles.extend(answering_handles);
    let units = stalled
        .into_iter()
        .zip(answering)
        .flat_map(|(s, a)| [s, a])
        .collect();
    (units, handles)
}
