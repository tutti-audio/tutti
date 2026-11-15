# Changelog

## Unreleased

tutti has no release yet. Each change lands on `main` as one squash-merged
pull request, and that commit's message and PR description are its record,
including migration notes for a breaking change:

```bash
git log --first-parent main
```

or <https://github.com/tutti-audio/tutti/commits/main>. Release notes will be
written here when a version is tagged.

The entries written before this file was cut down, through #221, are in
[`CHANGELOG.md` at 2137ff54](https://github.com/tutti-audio/tutti/blob/2137ff54407a15217f04a7fa695ccdfda915bafd/CHANGELOG.md).

## Breaking changes in `tutti-session`: the pool's loads

- `Load` is built with `Load::on_worker`, `Load::on_worker_then_main`,
  `Load::on_session` or `Load::ready`. A worker stage that continues on the
  main thread hands its output by value to `main(W, MainThreadMarker)`; that
  output is dropped only on the main thread, and one dropped elsewhere is
  handed to the session's main-work collector or leaked and counted
  (`stranded_loads`).
- `PoolState` gains `WaitingForMainThread` and `Opening`; `PoolError` gains
  `NeedsMainThread(K)`; `Session::retry` returns `RetryError::{NotFailed,
  NotPooled, NeedsMainThread}`; `BounceError` gains `PoolPending(Vec<PoolKey>)`.
- The pool's key bound adds `Sync`.
- `PoolEntry::is_lost` is the one reading of lost a handle's `presence()` follows.
- `Declared::shared` makes keys of one kind and identity share one load;
  `audio_file` and `soundfont` do. `Declared::new` is one instance per key.
- `PoolConfig` (`SessionBuilder::pool_config`) caps the worker stages at once;
  it is `#[non_exhaustive]`.
- `Session::pool_loaded` returns a `PoolLoaded`: call `update` until
  `has_landed`, then `take` the `PoolReport`.
- `tutti-plugin-types`: work queued to `MainWork` is forgotten and counted, never
  dropped, when it is dropped off the main thread.
