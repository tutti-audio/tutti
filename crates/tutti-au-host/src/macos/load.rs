//! Finding an Audio Unit by the bundle it was installed from, and loading it.
//!
//! AU plugins are registered system-wide: the instance comes from the component
//! registry (`AudioComponentFindNext`), but the bundle says which units it
//! installed. The lookup reads the bundle's declared components and resolves
//! the chosen one by its exact type, subtype and manufacturer.

use std::path::Path;

use tutti_au_ffi::declared_components;
use tutti_plugin_types::{
    EditorPresence, LoadStage, MainThreadMarker, MainWorkHandle, PluginClass, PluginDescriptor,
    PluginError, PluginFormat, PluginId, PluginRef, PluginResult, SampleRate,
};

use crate::component::component_info;
use crate::component::{AuType, ComponentKey, find_component};
use crate::control::{class_of, plugin_error, unit_id};
use crate::instance::AuLoaded;

/// The stream configuration a unit is created at: the activation applies its
/// own rate and block ceiling, so these only have to be a configuration the
/// unit takes.
const LOAD_RATE: SampleRate = SampleRate::SR_44K1;
const LOAD_BLOCK: u32 = 512;

/// The Audio Units the bundle at `path` declares, as the id a [`PluginRef`]
/// names them by and the key the registry resolves.
fn declared(path: &Path) -> PluginResult<Vec<(PluginId, ComponentKey)>> {
    let scanning = |reason: String| PluginError::Load {
        stage: LoadStage::Scanning,
        reason,
    };
    let entries = declared_components(path).map_err(|e| {
        scanning(format!(
            "'{}' is not an Audio Unit bundle: {e}",
            path.display()
        ))
    })?;
    // The host classifies the types it knows; a declared type it cannot is no
    // unit it can load.
    let units: Vec<_> = entries
        .into_iter()
        .filter(|c| !matches!(AuType::from_raw(c.key.component_type()), AuType::Unknown(_)))
        .map(|c| (unit_id(c.key), c.key))
        .collect();
    if units.is_empty() {
        return Err(scanning(format!(
            "'{}' declares no Audio Unit",
            path.display()
        )));
    }
    Ok(units)
}

/// The unit `load` selects among the bundle's `units`: the first with no
/// choice, otherwise the one whose id is the choice.
fn select(
    path: &Path,
    units: &[(PluginId, ComponentKey)],
    chosen: Option<&PluginId>,
) -> PluginResult<ComponentKey> {
    let Some(id) = chosen else {
        return Ok(units[0].1);
    };
    units
        .iter()
        .find(|(declared, _)| *declared == *id)
        .map(|(_, key)| *key)
        .ok_or_else(|| PluginError::Load {
            stage: LoadStage::Scanning,
            reason: format!(
                "'{}' declares no Audio Unit '{id}'; it declares: {}",
                path.display(),
                units
                    .iter()
                    .map(|(declared, _)| declared.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
}

impl AuLoaded {
    /// The descriptor of every Audio Unit the bundle at `path` declares
    /// (`AudioComponents` in its `Info.plist`), in the bundle's order, read from the
    /// registry's component and **without creating one**: a unit's editor is a property
    /// of an instance, so it is [`EditorPresence::Unknown`]. A unit the bundle declares
    /// and the system has not registered is left out, as a [`load`](Self::load) of it
    /// would refuse it.
    ///
    /// # Errors
    /// [`PluginError::Load`] at `Scanning`: `path` is not a bundle that declares an
    /// Audio Unit, or none of its units is registered.
    pub fn describe_all(path: &Path) -> PluginResult<Vec<(PluginId, PluginDescriptor)>> {
        let units = declared(path)?;
        let found: Vec<_> = units
            .iter()
            .filter_map(|(id, key)| {
                let info = find_component(*key).ok().and_then(component_info)?;
                Some((
                    id.clone(),
                    PluginDescriptor {
                        name: info.name,
                        vendor: info.manufacturer,
                        version: info.version,
                        class: PluginClass::Au {
                            component_type: class_of(info.component_type),
                        },
                        editor: EditorPresence::Unknown,
                        measured: None,
                        widths: None,
                    },
                ))
            })
            .collect();
        if found.is_empty() {
            return Err(PluginError::Load {
                stage: LoadStage::Scanning,
                reason: format!(
                    "'{}' declares Audio Units the system has not registered: {}",
                    path.display(),
                    units
                        .iter()
                        .map(|(id, _)| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        Ok(found)
    }

    /// Loads the Audio Unit installed from the bundle at `path`, not initialized.
    ///
    /// The host transport callbacks are offered once, here, off the audio
    /// thread: the AU pulls tempo, beat and transport state from them during its
    /// render. A unit that refuses the property simply runs on its own clock
    /// ([`transport`](Self::transport) is then `None`, and its features say so),
    /// not a failed load. The property listener is started
    /// ([`watch_properties`](Self::watch_properties)); its refusal fails the load.
    ///
    /// The unit is chosen from the bundle's own declaration (`AudioComponents`
    /// in its `Info.plist`) and resolved in the registry by its exact type,
    /// subtype and manufacturer. With no choice it is the bundle's first Audio
    /// Unit; a [`PluginRef`] that names a plugin picks the one whose
    /// [`PluginId::Au`] it is.
    ///
    /// # Errors
    /// [`PluginError::Load`] with the stage reached: `Scanning` when the path
    /// is not a bundle that declares an Audio Unit, the choice is not among its
    /// units, or the chosen unit is not registered, `Instantiation` when the component cannot be
    /// created, `Initialization` when AudioToolbox refuses the property listener.
    pub fn load(
        main: MainThreadMarker,
        work: &MainWorkHandle,
        plugin: &PluginRef,
    ) -> PluginResult<Self> {
        let _ = main;
        let path = plugin.path();
        let units = declared(path)?;
        let key = select(path, &units, plugin.id_for(PluginFormat::AudioUnit)?)?;
        let component = find_component(key).map_err(|_| PluginError::Load {
            stage: LoadStage::Scanning,
            reason: format!(
                "'{}' declares {}, which the system has not registered",
                path.display(),
                unit_id(key)
            ),
        })?;
        let mut loaded = AuLoaded::new(component, LOAD_RATE, LOAD_BLOCK, work).map_err(|e| {
            match plugin_error(e) {
                // The component's own failure, at the stage it reports.
                load @ PluginError::Load { .. } => load,
                other => PluginError::Load {
                    stage: LoadStage::Instantiation,
                    reason: other.to_string(),
                },
            }
        })?;
        // A unit that refuses the transport callbacks keeps its own clock: its
        // features lack `Features::TRANSPORT`, which is how the refusal reaches
        // the app.
        loaded.install_host_callbacks().ok();
        // Without the property listener no change of latency, tail or
        // parameters, and no edit the unit makes, is ever heard: a refusal fails
        // the load.
        if !loaded.watch_properties() {
            return Err(PluginError::Load {
                stage: LoadStage::Initialization,
                reason: "AudioToolbox refused the property listener".to_owned(),
            });
        }
        // The unit chosen by id even when the load named none, so a reopen
        // takes the same one.
        loaded.origin = PluginRef::new(path, unit_id(key));
        Ok(loaded)
    }
}
