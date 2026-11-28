//! What the plugin declares of its parameters, read once and again on a rescan.

use super::config::RangeTable;
use super::params::project_param_info;
use crate::events::ParamBounds;
use crate::types::{ClapParamFlags, ClapParamInfo};
use tutti_clap_ffi::Plugin;
use tutti_plugin_types::{MainThreadMarker, ParameterInfo};
use tutti_units::ParamId;

/// The plugin's parameters as declared: the table every read of them is
/// answered from, so no read asks the plugin once per parameter.
///
/// The enumeration truncates at the first `get_info(i)` that fails for
/// `i < count`. A parameter skipped past the hole would be absent from the
/// bounds and its automation would reach the plugin un-denormalized (raw
/// `0..1` into a `100..1100` range, silently); a short list is a visible
/// symptom where a selectively wrong table is not.
///
/// Read at load and again by a rescan of the parameters' info
/// ([`refresh`](Self::refresh)); the editor and the Processor's
/// [`RangeTable`] share it.
#[derive(Debug, Default)]
pub(crate) struct ParamTable {
    /// Each parameter's bounds and steppedness, sorted by id, and whether the
    /// plugin reported a nonzero `params.count()`.
    ranges: RangeTable,
    /// Every parameter in the shared vocabulary, in the plugin's order.
    listed: Vec<ParameterInfo>,
}

impl ParamBounds {
    /// The bounds `info` declares: the one reading of a `clap_param_info` that
    /// the Processor's table and the shared [`ParameterInfo`] both come from.
    pub(super) fn declared(info: &ClapParamInfo) -> Self {
        Self {
            // `clap_param_info.id` is the plugin's raw `clap_id`.
            id: ParamId::new(info.id),
            min: info.min_value,
            max: info.max_value,
            stepped: info.flags.contains(ClapParamFlags::STEPPED),
            read_only: info.flags.contains(ClapParamFlags::READONLY),
            requires_process: info.flags.contains(ClapParamFlags::REQUIRES_PROCESS),
        }
    }
}

impl ParamTable {
    /// The table of `plugin`'s parameters. Queries the plugin: main thread.
    pub(crate) fn read(main: MainThreadMarker, plugin: &Plugin) -> Self {
        let count = plugin.param_count(main);
        let mut infos = Vec::with_capacity(count as usize);
        for index in 0..count {
            let Some(info) = plugin.param_info(main, index) else {
                break;
            };
            infos.push(ClapParamInfo::from(info));
        }
        let mut declared: Vec<ParamBounds> = infos.iter().map(ParamBounds::declared).collect();
        declared.sort_by_key(|bounds| bounds.id);
        Self {
            ranges: RangeTable {
                ranges: declared,
                // The plugin's own count, not the length of the table: they
                // differ exactly when the enumeration truncated at a hole,
                // which `add_param_changes` has to tell apart from a plugin
                // with no parameters.
                claims: count > 0,
            },
            listed: infos.into_iter().map(project_param_info).collect(),
        }
    }

    /// Reads the plugin's parameters again.
    pub(crate) fn refresh(&mut self, main: MainThreadMarker, plugin: &Plugin) {
        *self = Self::read(main, plugin);
    }

    /// The bounds sorted by id, as the Processor denormalizes against them.
    pub(crate) fn ranges(&self) -> &RangeTable {
        &self.ranges
    }

    /// The declared bounds of `id`, or `None` for an id the plugin does not
    /// declare (an id past an enumeration hole included).
    pub(crate) fn bounds(&self, id: ParamId) -> Option<ParamBounds> {
        ParamBounds::find(&self.ranges.ranges, id).copied()
    }

    /// Whether `id` carries `CLAP_PARAM_REQUIRES_PROCESS`; `false` for an
    /// unknown id.
    pub(crate) fn requires_process(&self, id: ParamId) -> bool {
        self.bounds(id).is_some_and(|b| b.requires_process)
    }

    /// Every parameter in the shared vocabulary, in the plugin's order.
    pub(crate) fn listed(&self) -> &[ParameterInfo] {
        &self.listed
    }
}
