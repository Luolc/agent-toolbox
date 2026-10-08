//! The optional `<config home>/linear/config.json`:
//! `{"states": {"<type>": "<state name>", ...}, "default_labels": ["<label>", ...]}`,
//! both keys optional.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use super::states::TYPES;
use crate::common::{Error, usage};

#[derive(Default)]
pub struct Config {
    /// For each state type, the name of the state to use instead of the first.
    pub states: BTreeMap<String, String>,
    /// Labels `create` adds besides those given with `--label`.
    pub default_labels: Vec<String>,
}

impl Config {
    /// No file means an empty config. Errors name the path but never echo the
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
        let shape = "expected {\"states\": {\"<type>\": \"<state name>\", ...}, \
                     \"default_labels\": [\"<label>\", ...]}";
        let Value::Object(top) = value else {
            return Err(bad(shape));
        };
        let mut config = Self::default();
        for (key, value) in top {
            match (key.as_str(), value) {
                ("states", Value::Object(states)) => {
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
                        config.states.insert(kind, name);
                    }
                }
                ("default_labels", Value::Array(labels)) => {
                    for label in labels {
                        let Value::String(label) = label else {
                            return Err(bad(shape));
                        };
                        config.default_labels.push(label);
                    }
                }
                _ => return Err(bad(shape)),
            }
        }
        Ok(config)
    }
}
