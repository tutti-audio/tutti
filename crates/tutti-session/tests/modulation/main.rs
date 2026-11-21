//! Modulation through the session: lanes, routes onto params, edge gain and mix.

mod common;

/// Inert outside `assert_no_alloc`, so every test here may also gate a closure on
/// allocating nothing.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

// The session's modulation loop (`Session::mods`, `ModInput`).
mod modulation;
// A route onto a param on an `f64` session.
mod f64_modulation;
// A route onto an edge's gain (doc 013, "Edge gain", G2).
mod edge_modulation;
// A route onto the gain of an edge a narrower re-insert dropped.
mod unwired_route;
// A route onto a node's mix (doc 013, "Bypass and mute are graph state", M4b).
mod mix_modulation;
// Modulation on control lanes through the session: renders, takes, refusals
// and the node-side declarations (`export` and `synth` gate two cases inside
// the file).
mod mod_lanes;
// The insertion paths address a node's params, so a lane's knob is its cell.
mod params_at_insert;
// Indexed param addresses through the session: a graph write and a modulation
// route to a node's second param of one kind.
mod indexed_params;
// A route's range is a range of the target's unit; a backwards one is refused.
mod ranges;
// A route is found again and edited through a view; its keys are minted
// (doc 030, T4).
mod routes;
// One place to ask why nothing modulates (doc 030, T5).
mod problems;
// A route's polarity and curve, an LFO's rate and a macro's value are values:
// staged at `set`, republished at the update.
mod program_targets;

// Starts a bounce, awaits its render and applies it.
#[cfg(feature = "export")]
#[path = "../support/bounced.rs"]
mod bounced;
