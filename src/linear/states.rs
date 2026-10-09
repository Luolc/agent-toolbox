//! Workflow states, chosen by type rather than by name.

use std::collections::BTreeMap;

use crate::common::{Error, usage};

pub const STARTED: &str = "started";
pub const COMPLETED: &str = "completed";
pub const UNSTARTED: &str = "unstarted";
pub const BACKLOG: &str = "backlog";
pub const CANCELED: &str = "canceled";
pub const TYPES: [&str; 5] = [BACKLOG, UNSTARTED, STARTED, COMPLETED, CANCELED];
/// The config key under `states` that names the state `release --abandon`
/// sets; not a type, and never chosen by position.
pub const ABANDONED: &str = "abandoned";

pub struct State {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub position: f64,
}

/// The state to use for `kind`: the override's state if one is configured
/// (an error if the team has no state of that name and type), otherwise the
/// one with the lowest position, ties broken by name, then id, so that every
/// agent picks the same one. `None` when the team has no state of that type.
pub fn pick<'a>(
    states: &'a [State],
    kind: &str,
    overrides: &BTreeMap<String, String>,
) -> Result<Option<&'a State>, Error> {
    if let Some(name) = overrides.get(kind) {
        return match states.iter().find(|s| s.kind == kind && &s.name == name) {
            Some(state) => Ok(Some(state)),
            None => Err(usage(format!(
                "the config overrides the {kind} state with {name:?}, \
                 but the team has no {kind} state of that name"
            ))),
        };
    }
    Ok(states.iter().filter(|s| s.kind == kind).min_by(|a, b| {
        a.position
            .total_cmp(&b.position)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    }))
}
