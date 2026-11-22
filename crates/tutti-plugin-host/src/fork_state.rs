//! What a fork carries of the live plugin, and the order it restores in. The
//! in-process fork (`PluginNode::fork_instance`) and the out-of-process one
//! (`tutti-plugin`'s `PluginClient::fork_instance`) read and restore through
//! this one type.

use tutti_plugin_types::{PresetId, RenderMode, StateError};

/// The live plugin's current preset (when it lists any), its saved state and
/// the render mode it runs in, read on the control side when a node forks.
#[derive(Debug, Clone)]
pub struct ForkState {
    preset: Option<PresetId>,
    state: Vec<u8>,
    mode: RenderMode,
}

impl ForkState {
    /// The live plugin's `preset`, its saved `state` and its render `mode`.
    #[must_use]
    pub fn new(preset: Option<PresetId>, state: Vec<u8>, mode: RenderMode) -> Self {
        Self {
            preset,
            state,
            mode,
        }
    }

    /// The live plugin's saved state.
    #[must_use]
    pub fn state(&self) -> &[u8] {
        &self.state
    }

    /// Whether the live plugin had a current preset to carry.
    #[must_use]
    pub fn has_preset(&self) -> bool {
        self.preset.is_some()
    }

    /// Whether the live plugin ran offline.
    #[must_use]
    pub fn ran_offline(&self) -> bool {
        self.mode.is_offline()
    }

    /// Whether the fork renders offline: a [`RenderMode::Offline`] fork does,
    /// and a [`RenderMode::Realtime`] one keeps the mode the live plugin ran in.
    #[must_use]
    pub fn offline(&self, mode: RenderMode) -> bool {
        match mode {
            RenderMode::Offline => true,
            RenderMode::Realtime => self.mode.is_offline(),
        }
    }

    /// Restores the `fresh` plugin: its preset first, then its state. In that
    /// order because a preset change may move every parameter, and the state
    /// is what the live plugin holds now. The in-process fork calls it before
    /// activation; an out-of-process fork calls it after the launch, which loaded the
    /// state and activated the plugin (a plugin may take its mode from the
    /// state at activation), so the state goes in again over the preset.
    ///
    /// # Errors
    ///
    /// [`StateError::Rejected`] when `load_preset` refuses the live plugin's
    /// preset, else what `load_state` answers.
    pub fn restore<T>(
        &self,
        fresh: &mut T,
        load_preset: impl FnOnce(&mut T, &PresetId) -> bool,
        load_state: impl FnOnce(&mut T, &[u8]) -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        if let Some(preset) = &self.preset
            && !load_preset(fresh, preset)
        {
            return Err(StateError::Rejected(format!(
                "the live instance's preset {preset:?} is not in the fresh instance"
            )));
        }
        load_state(fresh, &self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn preset() -> PresetId {
        PresetId::Number(3)
    }

    /// A live fork of an offline plugin is offline; an offline fork is offline
    /// whatever the live mode; a live fork of a realtime plugin is realtime.
    #[test]
    fn a_live_fork_keeps_the_live_mode_and_an_offline_fork_is_offline() {
        let state = |mode| ForkState::new(None, Vec::new(), mode);
        assert!(state(RenderMode::Offline).offline(RenderMode::Realtime));
        assert!(state(RenderMode::Offline).offline(RenderMode::Offline));
        assert!(state(RenderMode::Realtime).offline(RenderMode::Offline));
        assert!(!state(RenderMode::Realtime).offline(RenderMode::Realtime));
    }

    /// The preset goes in before the state, and the state is the live one's.
    #[test]
    fn the_preset_is_restored_before_the_state() {
        let log = RefCell::new(Vec::new());
        let fork = ForkState::new(Some(preset()), vec![7, 8], RenderMode::Realtime);
        fork.restore(
            &mut (),
            |(), id| {
                log.borrow_mut().push(format!("preset {id:?}"));
                true
            },
            |(), bytes| {
                log.borrow_mut().push(format!("state {bytes:?}"));
                Ok(())
            },
        )
        .expect("both load");
        assert_eq!(
            *log.borrow(),
            [format!("preset {:?}", preset()), "state [7, 8]".to_string()]
        );
    }

    /// A refused preset is a refused restore, and the state is not loaded
    /// over the wrong preset.
    #[test]
    fn a_refused_preset_stops_the_restore() {
        let fork = ForkState::new(Some(preset()), vec![1], RenderMode::Realtime);
        let err = fork
            .restore(
                &mut (),
                |(), _| false,
                |(), _| panic!("the state follows an accepted preset"),
            )
            .expect_err("the preset is refused");
        assert!(matches!(err, StateError::Rejected(_)), "{err:?}");
    }

    /// A plugin with no current preset loads the state alone.
    #[test]
    fn no_preset_loads_the_state_alone() {
        let fork = ForkState::new(None, vec![1], RenderMode::Realtime);
        fork.restore(&mut (), |(), _| panic!("no preset to load"), |(), _| Ok(()))
            .expect("the state loads");
    }
}
