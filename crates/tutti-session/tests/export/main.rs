//! Offline render through the session: bounce, cancel and the forks of every node kind.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

// `Session::new` is a full session: it bounces.
#[cfg(feature = "export")]
mod new_session;
// Stems: a node's own output beside the master, aligned to it, in one pass.
#[cfg(feature = "export")]
mod bounce_stems;
// A bounce's `create_new`: a path that exists is refused at the commit and the file kept.
#[cfg(feature = "export")]
mod bounce_create_new;
// A bounce's dither: triangular unless it says otherwise.
#[cfg(feature = "export")]
mod bounce_dither;
// Stopping an export and reading its progress (`BounceHandle`).
#[cfg(feature = "export")]
mod export_cancel;
// What an export renders and forks: the graph's own latency and tail, a
// prepare hook's edit and timeline, a fork's isolation from live playback,
// and where a fork is built and dropped.
#[cfg(feature = "export")]
mod export_fork;
#[cfg(feature = "export")]
mod f64_export;
// What a bounce's nodes log on the audio thread, as `tracing` events.
#[cfg(feature = "export")]
mod rt_log;
// A parallel export is bit-identical to the serial one.
#[cfg(feature = "export")]
mod export_parallel;
// What a bounce keeps past its range for the sampler's nodes, and a voice pool's fork.
#[cfg(all(feature = "export", feature = "sampler"))]
mod bounce_sampler;
// A disk stream: opened, primed and bounced, and ended by the session's drop.
#[cfg(all(feature = "export", feature = "sampler"))]
mod streaming;
// A sampler voice (memory and disk) exported through a fork.
#[cfg(all(feature = "export", feature = "sampler"))]
mod export_fork_sampler;
// A SoundFont, a polysynth and their clip, exported through a fork.
#[cfg(all(
    feature = "export",
    feature = "midi",
    feature = "synth",
    feature = "soundfont"
))]
mod export_fork_synth;
