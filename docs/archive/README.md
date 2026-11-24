# Archive

Dated records of finished work. Each file is kept as written: it describes the
code and the questions as they stood when it was made, so a statement in it may
no longer hold. The current design is the rustdoc of the item, with
`CLAUDE.md` and `docs/engineering-notes.md` for the rules and their reasons;
come here for the history behind a decision.

| File | What it was |
|---|---|
| `004-plugin-host-gap-audit.md` | The audit of what the plugin hosts lacked (the D-, C- and A- items); its gaps are closed. |
| `005-plugin-presets.md` | The design for plugin presets (D-9), shipped. |
| `007-channel-topology.md` | The design for speaker arrangement and channel topology (D-11), reporting built, the proposal deferred. |
| `008-plugin-lifecycle-strategies.md` | Why the four hosts model their lifecycles differently; its rule, the fifth-format checklist and the AU pinned pointer now live in `tutti-plugin`'s crate docs, under *The plugin state machine*. |
| `009-plugin-crash-notification.md` | The design for telling the app a plugin crashed, built. |
| `010-parameter-grouping.md` | The design for parameter groups across the four formats, built. |
| `011-outcome-types.md` | The design for `Result` and enum outcomes on the plugin surface, built. |
| `013-native-graph.md` | The design and build record of the native graph: the compiler, the executor, the cost model, events as ports, fan-in, bypass, and the per-node migration. Built; what holds today is in `tutti-graph`'s rustdoc. Its 64-bit path and per-block scheduling sections were replaced before they shipped. |
| `014-modulation.md` | The design for automation and modulation as one control program per target, built. The grid, the lane and the plugin delivery are in the rustdoc of `tutti-modulation`, `tutti-nodes` and `tutti-plugin-host`. |
| `015-headless-api.md` | The design for the session without a host framework: phases, liveness and the umbrella's features, built. |
| `020-tempo-map-transport.md` | The design for the tempo map and the transport walk, built; it lives in `tutti-tempo` and `tutti-timeline`. |
| `025-streaming.md` | The design for `Source`, `Sink` and the disk streamer. The edges are built as designed; the streamer was reshaped afterwards and its rustdoc is current. |
| `030-core-api.md` | The design and step list for the session's typed-key API, built through its last step. |
| `unit-adoption-backlog.md` | The backlog of moving bare floats to unit types, completed apart from app-blocked items. |
| `fundsp-comparison.md` | The comparison of hand-written DSP against fundsp opcodes, from before fundsp was deleted. |
| `fundsp-fork-audit.md` | The audit of which modules of the vendored fundsp fork were reachable, from before the fork was deleted. |
| `fundsp-vs-tutti-units-audit.md` | The audit of `tutti-nodes` against fundsp duplicates, from before fundsp was deleted. |
| `benchmarks.md` | The benchmark baseline recorded on one machine on 2026-09-22; figures do not travel between machines. |
| `plugin-hosting/` | The CLAP and VST3 completion plans, the VST3 multi-bus plan and the CLAP conformance harness note, all executed. |
