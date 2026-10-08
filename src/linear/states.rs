//! Workflow states, chosen by type rather than by name, and the optional name
//! overrides in `<config home>/linear/config.json`.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::common::{Error, usage};

pub const STARTED: &str = "started";
pub const COMPLETED: &str = "completed";
pub const UNSTARTED: &str = "unstarted";
pub const BACKLOG: &str = "backlog";
const TYPES: [&str; 5] = [BACKLOG, UNSTARTED, STARTED, COMPLETED, "canceled"];

pub struct State {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub position: f64,
}

/// For each state type, the name of the state to use instead of the first.
#[derive(Default)]
pub struct Overrides(BTreeMap<String, String>);

impl Overrides {
    /// No file means no overrides. Errors name the path but never echo the
    /// file's content.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(usage(format!(
                    "cannot read {}: {}",
                    path.display(),
                    err.kind()
                )));
            }
        };
        let bad = |why: &str| usage(format!("{}: {why}", path.display()));
        let value: Value = serde_json::from_str(&text).map_err(|err| {
            bad(&format!(
                "not valid JSON (line {}, column {})",
                err.line(),
                err.column()
            ))
        })?;
        let shape = "expected {\"states\": {\"<type>\": \"<state name>\", ...}}";
        let states = match value {
            Value::Object(mut top) if top.len() == 1 => top.remove("states"),
            _ => None,
        };
        let Some(Value::Object(states)) = states else {
            return Err(bad(shape));
        };
        let mut overrides = BTreeMap::new();
        for (kind, name) in states {
            if !TYPES.contains(&kind.as_str()) {
                return Err(bad(&format!(
                    "a key under \"states\" is not a state type ({})",
                    TYPES.join(", ")
                )));
            }
            let Value::String(name) = name else {
                return Err(bad(shape));
            };
            overrides.insert(kind, name);
        }
        Ok(Self(overrides))
    }
}

/// The state to use for `kind`: the override's state if one is configured
/// (an error if the team has no state of that name and type), otherwise the
/// one with the lowest position, ties broken by name, then id, so that every
/// agent picks the same one. `None` when the team has no state of that type.
pub fn pick<'a>(
    states: &'a [State],
    kind: &str,
    overrides: &Overrides,
) -> Result<Option<&'a State>, Error> {
    if let Some(name) = overrides.0.get(kind) {
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
