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

### Breaking: the root's id and the device's placement (J3b)

- `tutti_graph`: `RootId` (a root generation), `GraphSpec::root_id`,
  `Plan::root_id()`, `CommitError::Unplaceable`, and `fold_to_topology(src, dst)`
  (`fold_to_width` is `fold_to_topology` into `assumed(width)`).
- `tutti_engine::OutputControl`: `declare_root(root, device)` is
  `publish_root(id, root, device)`, keyed by `(RootId, device)`; new `state_device`,
  `device`, `has_fold`, `has_width`. Each root's folds are published before the
  commit that carries its id, and an older root's go at the first `reclaim` after a
  plan of a later root has rendered.
- `tutti_session`: `SessionBuilder::device_topology`, `DeviceRestart::topology`,
  `Session::output_device` and `LiveSession::output_device` (an `OutputPlacement`:
  `Stated` or `Assumed`), `Error::Unplaceable` for a restart whose root has no fold
  into the new device, and a commit with no fold refused as
  `CommitError::Unplaceable` (`SessionEvent::CommitRefused`).
  `Session::restart_on_placed` (testing).
