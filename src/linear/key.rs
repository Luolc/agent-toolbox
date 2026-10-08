//! The Linear API key: from `LINEAR_API_KEY`, or from the output of
//! `LINEAR_API_KEY_CMD` with a 24-hour cache file.
//!
//! The value is only ever placed in the `Authorization` header. Every value
//! this run has held is kept so error messages can be masked against all of
//! them.

use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::common::{Context, Error, Secret, usage};

pub const KEY_VAR: &str = "LINEAR_API_KEY";
pub const CMD_VAR: &str = "LINEAR_API_KEY_CMD";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

enum Source {
    Env,
    Command { command: String, cache: PathBuf },
}

pub struct Key {
    source: Source,
    current: Secret,
    /// Every value held during this run, the current one included.
    seen: Vec<Secret>,
    refreshed: bool,
}

impl Key {
    pub fn resolve(ctx: &Context) -> Result<Self, Error> {
        if let Some(key) = ctx.secret(KEY_VAR)? {
            return Ok(Self::new(Source::Env, key));
        }
        let Some(command) = ctx.var(CMD_VAR) else {
            return Err(usage(format!(
                "no Linear API key: set {KEY_VAR}, or {CMD_VAR} to a command that prints the key"
            )));
        };
        let command = command
            .into_string()
            .map_err(|_| usage(format!("{CMD_VAR} is not valid UTF-8")))?;
        let cache = cache_path(ctx)?;
        let key = match read_fresh_cache(&cache) {
            Some(key) => key,
            None => {
                let key = run_command(&command)?;
                write_cache(&cache, &key)?;
                key
            }
        };
        Ok(Self::new(Source::Command { command, cache }, key))
    }

    fn new(source: Source, key: Secret) -> Self {
        Self {
            source,
            seen: vec![Secret::new(key.expose().to_owned())],
            current: key,
            refreshed: false,
        }
    }

    pub fn current(&self) -> &Secret {
        &self.current
    }

    /// After a 401: rerun the command once, whatever the cache's age, and
    /// rewrite the cache. `Ok(false)` when there is nothing to refetch: the key
    /// came from `LINEAR_API_KEY`, or this run has refreshed once already.
    pub fn refresh(&mut self) -> Result<bool, Error> {
        let Source::Command { command, cache } = &self.source else {
            return Ok(false);
        };
        if self.refreshed {
            return Ok(false);
        }
        self.refreshed = true;
        let key = run_command(command)?;
        write_cache(cache, &key)?;
        self.seen.push(Secret::new(key.expose().to_owned()));
        self.current = key;
        Ok(true)
    }

    /// Where the key came from, for error messages.
    pub fn source_name(&self) -> &'static str {
        match self.source {
            Source::Env => KEY_VAR,
            Source::Command { .. } => CMD_VAR,
        }
    }

    pub fn mask(&self, text: &str) -> String {
        let candidates: Vec<&str> = self.seen.iter().map(Secret::expose).collect();
        mask(&candidates, text)
    }

    /// Every string in `value`, object keys included, masked.
    pub fn mask_json(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.mask(text)),
            Value::Array(items) => items.iter().map(|v| self.mask_json(v)).collect(),
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| (self.mask(k), self.mask_json(v)))
                .collect(),
            other => other.clone(),
        }
    }
}

/// Replace every candidate in `text` with `***`, longest first: when one value
/// contains another, replacing the shorter one first would leave a fragment of
/// the longer one behind.
fn mask(candidates: &[&str], text: &str) -> String {
    let mut sorted: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|c| !c.is_empty())
        .collect();
    sorted.sort_by_key(|c| std::cmp::Reverse(c.len()));
    sorted
        .into_iter()
        .fold(text.to_owned(), |text, c| text.replace(c, "***"))
}

/// `api-key` in the directory the config file shares.
fn cache_path(ctx: &Context) -> Result<PathBuf, Error> {
    Ok(super::config_dir(ctx)?.join("api-key"))
}

/// The cached key if the file is younger than 24 hours and not empty. Any
/// problem reading it counts as a miss: the command is the source of truth.
fn read_fresh_cache(path: &Path) -> Option<Secret> {
    let modified = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age >= CACHE_TTL {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let key = text.trim();
    (!key.is_empty()).then(|| Secret::new(key.to_owned()))
}

/// Run the command through `sh -c` and take its trimmed stdout. Its stderr is
/// discarded, never shown: a secret manager's message can carry paths or
/// fragments of what it guards.
fn run_command(command: &str) -> Result<Secret, Error> {
    let failed = |why: &dyn std::fmt::Display| {
        usage(format!(
            "fetching the Linear API key with {CMD_VAR} failed: {why}"
        ))
    };
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|err| failed(&format_args!("cannot start /bin/sh: {}", err.kind())))?;
    if !output.status.success() {
        return Err(failed(&output.status));
    }
    let key = String::from_utf8(output.stdout).map_err(|_| failed(&"output is not UTF-8"))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(failed(&"empty output"));
    }
    Ok(Secret::new(key.to_owned()))
}

/// Write the cache file with mode 0600 inside a directory with mode 0700. An
/// existing directory is tightened to 0700: it is this tool's own directory,
/// and it holds a credential.
fn write_cache(path: &Path, key: &Secret) -> Result<(), Error> {
    let cannot = |err: std::io::Error| {
        usage(format!(
            "cannot write the key cache at {}: {err}",
            path.display()
        ))
    };
    let dir = path.parent().expect("the cache path has a parent");
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(cannot)?;
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::set_permissions(dir, Permissions::from_mode(0o700)).map_err(cannot)?;
        }
        Err(err) => return Err(cannot(err)),
    }
    // A fresh file renamed into place: the mode is 0600 from the first byte,
    // and a reader never sees a half-written key.
    let tmp = dir.join(format!(".api-key.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(cannot)?;
    file.write_all(key.expose().as_bytes()).map_err(cannot)?;
    drop(file);
    fs::rename(&tmp, path).map_err(cannot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_the_longer_value_whole_when_one_contains_the_other() {
        // Shorter first, as the values arrive after a refresh whose new key
        // extends the old one.
        let candidates = ["synthetic-key", "synthetic-key-rotated"];
        let text = "rejected synthetic-key-rotated and synthetic-key";
        assert_eq!(mask(&candidates, text), "rejected *** and ***");
    }
}
