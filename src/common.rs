//! Helpers every tool shares: home directory, credentials, local CLI
//! versions, single-shot HTTP, records and table rendering.
//!
//! Invariants every caller relies on:
//!
//! - A token value is read from disk only to build an `Authorization` header.
//!   It is never printed and never part of an error or of `Debug` output.
//! - Every credential location is resolved by [`Context`], so a test can
//!   point it at a synthetic directory and never touches the real home.
//! - [`http_get_json`] issues exactly one request: no retry, no polling.

use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::timefmt::now_iso;

pub type Record = Map<String, Value>;

#[derive(Debug)]
pub enum Error {
    /// Fatal, user-facing error. The message never contains a token value.
    Usage(String),
    /// The upstream endpoint answered 429. Callers exit without retrying.
    RateLimited { retry_after: Option<String> },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(message) => f.write_str(message),
            Error::RateLimited { retry_after } => write!(
                f,
                "rate limited by the upstream endpoint (retry-after: {})",
                retry_after.as_deref().unwrap_or("unknown")
            ),
        }
    }
}

pub fn usage(message: impl Into<String>) -> Error {
    Error::Usage(message.into())
}

/// A credential. It has no `Display`, and `Debug` does not show the value.
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Request headers. `Debug` lists the names only, because a value may be a
/// credential.
pub struct Headers(pub Vec<(&'static str, String)>);

impl fmt::Debug for Headers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Harness {
    Claude,
    Codex,
    Grok,
}

impl Harness {
    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Grok => "grok",
        }
    }

    /// The variable the harness itself reads to relocate its config directory.
    fn dir_var(self) -> &'static str {
        match self {
            Harness::Claude => "CLAUDE_CONFIG_DIR",
            Harness::Codex => "CODEX_HOME",
            Harness::Grok => "GROK_HOME",
        }
    }

    fn dir_name(self) -> &'static str {
        match self {
            Harness::Claude => ".claude",
            Harness::Codex => ".codex",
            Harness::Grok => ".grok",
        }
    }
}

type EnvLookup = Box<dyn Fn(&str) -> Option<OsString>>;

/// Where a run looks for credentials: the `--config-dir` flag and the
/// environment. The environment is a closure so tests never touch the real one.
pub struct Context {
    pub config_dir: Option<PathBuf>,
    pub env: EnvLookup,
}

impl Context {
    pub fn from_process(config_dir: Option<PathBuf>) -> Self {
        Self {
            config_dir,
            env: Box::new(|name| std::env::var_os(name)),
        }
    }

    /// An environment variable; an empty value counts as unset.
    fn var(&self, name: &str) -> Option<OsString> {
        (self.env)(name).filter(|value| !value.is_empty())
    }

    /// The harness config directory (the one holding its credential file):
    /// `--config-dir`, then the harness's own variable, then `ATB_HOME` as a
    /// whole-home override, then the default home.
    pub fn harness_dir(&self, harness: Harness) -> Result<PathBuf, Error> {
        if let Some(dir) = &self.config_dir {
            return Ok(dir.clone());
        }
        if let Some(dir) = self.var(harness.dir_var()) {
            return Ok(PathBuf::from(dir));
        }
        let home = match self.var("ATB_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => std::env::home_dir().ok_or_else(|| {
                usage("cannot determine the home directory; pass --config-dir or set ATB_HOME")
            })?,
        };
        Ok(home.join(harness.dir_name()))
    }

    /// A credential passed directly in the environment.
    pub fn secret(&self, name: &str) -> Result<Option<Secret>, Error> {
        match self.var(name) {
            None => Ok(None),
            Some(value) => value
                .into_string()
                .map(|value| Some(Secret::new(value)))
                .map_err(|_| usage(format!("{name} is not valid UTF-8"))),
        }
    }

    /// The identifier that must accompany `token_var` when that one is set.
    pub fn companion(&self, token_var: &str, name: &str) -> Result<String, Error> {
        let missing = || usage(format!("{token_var} is set but {name} is not; set both"));
        self.var(name)
            .ok_or_else(missing)?
            .into_string()
            .map_err(|_| usage(format!("{name} is not valid UTF-8")))
    }
}

/// Build a record with the four fields every record of this binary carries.
pub fn record(
    agent: &str,
    provenance: &str,
    ts: Option<String>,
    fields: Vec<(&'static str, Value)>,
) -> Record {
    let mut record = Record::new();
    record.insert("ts".into(), ts.unwrap_or_else(now_iso).into());
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    record.insert("host".into(), host.into());
    record.insert("agent".into(), agent.into());
    record.insert("provenance".into(), provenance.into());
    for (name, value) in fields {
        record.insert(name.into(), value);
    }
    record
}

/// Read a JSON object, reporting only the path on failure, never the content.
pub fn read_json(path: &Path, what: &str) -> Result<Map<String, Value>, Error> {
    let raw = std::fs::read(path)
        .map_err(|err| usage(format!("cannot read {what} at {}: {err}", path.display())))?;
    match serde_json::from_slice(&raw) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(usage(format!(
            "{what} at {} is not a JSON object",
            path.display()
        ))),
        Err(_) => Err(usage(format!(
            "{what} at {} is not valid JSON",
            path.display()
        ))),
    }
}

const VERSION_TIMEOUT: Duration = Duration::from_secs(15);

/// Read the version of a locally installed CLI by running `<binary> --version`.
pub fn cli_version(binary: &str) -> Result<String, Error> {
    let cannot_run =
        |why: &dyn fmt::Display| usage(format!("cannot run `{binary} --version`: {why}"));
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => usage(format!(
                "{binary} is not on PATH; its version is needed for the User-Agent"
            )),
            _ => cannot_run(&err),
        })?;
    let deadline = Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(cannot_run(&"timed out"));
            }
            Err(err) => return Err(cannot_run(&err)),
        }
    }
    let mut stdout = String::new();
    let mut stderr = String::new();
    // A version line fits the pipe buffer, so reading after exit cannot block.
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let text = if stdout.is_empty() { &stderr } else { &stdout };
    find_version(text).map(str::to_owned).ok_or_else(|| {
        usage(format!(
            "cannot parse a version out of `{binary} --version`"
        ))
    })
}

/// The first `digits.digits.digits` in `text`.
fn find_version(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let digits = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    (0..bytes.len()).find_map(|start| {
        let mut end = start;
        for part in 0..3 {
            if part > 0 {
                if bytes.get(end) != Some(&b'.') {
                    return None;
                }
                end += 1;
            }
            let run = digits(end);
            if run == 0 {
                return None;
            }
            end += run;
        }
        Some(&text[start..end])
    })
}

/// Issue exactly one GET and decode the JSON body. No retry loop, ever.
///
/// Redirects are not followed, so the headers never travel to another host.
/// Error messages carry the URL and the status, never a header or the body.
pub fn http_get_json(url: &str, headers: &Headers, timeout: Duration) -> Result<Value, Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .accept(ureq::config::AutoHeaderValue::None)
        .build()
        .into();
    let mut request = agent.get(url);
    for (name, value) in &headers.0 {
        request = request.header(*name, value);
    }
    let mut response = request
        .call()
        .map_err(|err| usage(format!("cannot reach {url}: {err}")))?;
    let status = response.status();
    if status.as_u16() == 429 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        return Err(Error::RateLimited { retry_after });
    }
    if !status.is_success() {
        let reason = status.canonical_reason().unwrap_or("");
        return Err(usage(format!(
            "HTTP {} {reason} from {url}",
            status.as_u16()
        )));
    }
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|err| usage(format!("cannot read the response from {url}: {err}")))?;
    serde_json::from_str(&body).map_err(|_| usage(format!("{url} did not return JSON")))
}

pub fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return "(no data)".into();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: &mut dyn Iterator<Item = &str>| {
        let padded: Vec<String> = cells
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        padded.join("  ").trim_end().to_owned()
    };
    let mut lines = vec![line(&mut headers.iter().copied())];
    let rules: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
    lines.push(rules.join("  "));
    for row in rows {
        lines.push(line(&mut row.iter().map(String::as_str)));
    }
    lines.join("\n")
}

/// One table cell: `-` for null, one decimal for a float, the bare value otherwise.
pub fn fmt_cell(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "-".into(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) if number.is_f64() => {
            format!("{:.1}", number.as_f64().unwrap_or_default())
        }
        Some(other) => other.to_string(),
    }
}

pub fn dump_json(record: &Record) -> String {
    serde_json::to_string_pretty(record).expect("a JSON map always serializes")
}

/// A [`Context`] over a fixed set of variables, for tests.
#[cfg(test)]
pub fn test_context(flag: Option<&str>, vars: &'static [(&'static str, &'static str)]) -> Context {
    Context {
        config_dir: flag.map(PathBuf::from),
        env: Box::new(move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn finds_the_version_each_cli_prints() {
        assert_eq!(find_version("2.1.285 (Claude Code)"), Some("2.1.285"));
        assert_eq!(find_version("codex-cli 0.153.0"), Some("0.153.0"));
        assert_eq!(
            find_version("grok 1.0.44 (5b807183dd79) [stable]"),
            Some("1.0.44")
        );
        assert_eq!(find_version("v2 build 7"), None);
    }

    #[test]
    fn debug_output_never_shows_a_credential() {
        let token = "synthetic-token-not-a-credential";
        let secret = format!("{:?}", Secret::new(token.into()));
        let headers = format!(
            "{:?}",
            Headers(vec![("Authorization", format!("Bearer {token}"))])
        );
        assert!(!secret.contains(token), "{secret}");
        assert_eq!(headers, r#"["Authorization"]"#);
    }

    #[test]
    fn table_pads_columns_and_formats_cells() {
        let rows = vec![
            vec![
                "five_hour".to_owned(),
                fmt_cell(Some(&json!(42.5))),
                fmt_cell(None),
            ],
            vec![
                "seven_day".to_owned(),
                fmt_cell(Some(&json!(7))),
                fmt_cell(Some(&json!("x"))),
            ],
        ];
        assert_eq!(
            render_table(&["WINDOW", "USED_PCT", "RESETS_AT"], &rows),
            "WINDOW     USED_PCT  RESETS_AT\n\
             ---------  --------  ---------\n\
             five_hour  42.5      -\n\
             seven_day  7         x"
        );
        assert_eq!(render_table(&["A"], &[]), "(no data)");
    }

    #[test]
    fn harness_dir_precedence_is_flag_then_harness_variable_then_atb_home() {
        let vars = &[("CLAUDE_CONFIG_DIR", "/acct/claude"), ("ATB_HOME", "/atb")];
        let dir = |ctx: Context, harness| ctx.harness_dir(harness).unwrap();
        assert_eq!(
            dir(test_context(Some("/flag"), vars), Harness::Claude),
            Path::new("/flag")
        );
        assert_eq!(
            dir(test_context(None, vars), Harness::Claude),
            Path::new("/acct/claude")
        );
        assert_eq!(
            dir(test_context(None, &[("ATB_HOME", "/atb")]), Harness::Claude),
            Path::new("/atb/.claude")
        );
        // The other two harnesses fall through to ATB_HOME here, because the
        // Claude variable is not theirs.
        assert_eq!(
            dir(test_context(None, vars), Harness::Codex),
            Path::new("/atb/.codex")
        );
        assert_eq!(
            dir(test_context(None, vars), Harness::Grok),
            Path::new("/atb/.grok")
        );
    }

    #[test]
    fn each_harness_reads_its_own_variable_and_empty_counts_as_unset() {
        let vars = &[
            ("CLAUDE_CONFIG_DIR", ""),
            ("CODEX_HOME", "/acct/codex"),
            ("GROK_HOME", "/acct/grok"),
            ("ATB_HOME", "/atb"),
        ];
        let dir = |harness| test_context(None, vars).harness_dir(harness).unwrap();
        assert_eq!(dir(Harness::Claude), Path::new("/atb/.claude"));
        assert_eq!(dir(Harness::Codex), Path::new("/acct/codex"));
        assert_eq!(dir(Harness::Grok), Path::new("/acct/grok"));
    }
}
