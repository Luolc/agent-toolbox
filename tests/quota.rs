//! End-to-end runs of `atb quota` against synthetic directories.
//!
//! Every run here stops before the network: either the credential file is
//! unusable, or (Codex) it is absent and the local fallback answers. No test
//! reads a real credential file, and the only token is synthetic.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

const FAKE_TOKEN: &str = "synthetic-token-not-a-credential";

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `atb` with nothing inherited that could point it at a real account.
fn atb(args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atb"));
    command.args(args).env_clear();
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn token_count(ts: &str, primary_minutes: u32, used: f64, secondary: Value) -> String {
    json!({
        "timestamp": ts,
        "type": "event_msg",
        "payload": {
            "type": "token_count",
            "info": {"total_token_usage": {"input_tokens": 1}},
            "rate_limits": {
                "limit_id": "codex",
                "plan_type": "prolite",
                "primary": {
                    "used_percent": used,
                    "window_minutes": primary_minutes,
                    "resets_at": 1_788_747_923,
                },
                "secondary": secondary,
                "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
            },
        },
    })
    .to_string()
}

fn write_rollout(codex_dir: &Path, day: &str, name: &str, lines: &[String]) -> PathBuf {
    let dir = codex_dir.join("sessions").join(day);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-{name}.jsonl"));
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn set_mtime(path: &Path, unix_secs: u64) {
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(unix_secs);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

#[test]
fn codex_falls_back_to_the_newest_rollout_snapshot() {
    let home = scratch("codex-fallback");
    let codex = home.join(".codex");
    let old = write_rollout(
        &codex,
        "2026/08/30",
        "old",
        &[token_count(
            "2026-08-30T10:00:00.000Z",
            300,
            5.0,
            Value::Null,
        )],
    );
    set_mtime(&old, 1_900_000_000);
    let secondary =
        json!({"used_percent": 30.0, "window_minutes": 10080, "resets_at": 1_788_747_923});
    let newest = write_rollout(
        &codex,
        "2026/08/31",
        "new",
        &[
            token_count("2026-08-31T10:00:00.000Z", 300, 40.0, Value::Null),
            json!({"type": "response_item", "payload": {"type": "message"}}).to_string(),
            token_count("2026-08-31T11:00:00.000Z", 300, 42.5, secondary),
        ],
    );
    set_mtime(&newest, 2_000_000_000);

    let output = atb(&["quota", "codex", "--json"], &[("ATB_HOME", &home)]);
    assert!(output.status.success(), "{}", stderr(&output));
    let mut record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(record["host"].is_string());
    record["host"] = Value::Null;
    assert_eq!(
        record,
        json!({
            "ts": "2026-08-31T11:00:00Z",
            "host": null,
            "agent": "codex",
            "provenance": "local_rollout",
            "plan_type": "prolite",
            "limit_id": "codex",
            "windows": {
                "five_hour": {
                    "slot": "primary",
                    "used_percent": 42.5,
                    "window_minutes": 300,
                    "resets_at": "2026-09-07T02:25:23Z",
                },
                "seven_day": {
                    "slot": "secondary",
                    "used_percent": 30.0,
                    "window_minutes": 10080,
                    "resets_at": "2026-09-07T02:25:23Z",
                },
            },
            "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
        })
    );
    // The note says which arm answered and why.
    let note = stderr(&output);
    assert!(note.contains("using the local rollout snapshot"), "{note}");
    assert!(note.contains("auth.json"), "{note}");
}

#[test]
fn codex_window_of_unknown_length_keeps_its_minutes() {
    let home = scratch("codex-odd-window");
    write_rollout(
        &home.join(".codex"),
        "2026/08/31",
        "odd",
        &[token_count(
            "2026-08-31T10:00:00.000Z",
            1440,
            1.0,
            Value::Null,
        )],
    );
    let output = atb(&["quota", "codex", "--json"], &[("ATB_HOME", &home)]);
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(record["windows"]["window_1440m"].is_object(), "{record}");
}

#[test]
fn codex_without_any_snapshot_is_a_clear_error() {
    let home = scratch("codex-no-snapshot");
    write_rollout(
        &home.join(".codex"),
        "2026/08/31",
        "empty",
        &[json!({"type": "event_msg", "payload": {"type": "agent_message"}}).to_string()],
    );
    let output = atb(&["quota", "codex"], &[("ATB_HOME", &home)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no rate-limit snapshot found"));
}

#[test]
fn codex_home_relocates_credentials_and_sessions() {
    let root = scratch("codex-home-var");
    let account = root.join("account-b");
    // An unusable credential file: the run must name it, keep its content to
    // itself, and answer from the sessions next to it.
    fs::create_dir_all(&account).unwrap();
    fs::write(account.join("auth.json"), format!("not json {FAKE_TOKEN}")).unwrap();
    write_rollout(
        &account,
        "2026/08/31",
        "b",
        &[token_count(
            "2026-08-31T10:00:00.000Z",
            300,
            7.0,
            Value::Null,
        )],
    );
    let env = [
        ("CODEX_HOME", account.as_path()),
        ("ATB_HOME", root.as_path()),
    ];
    let output = atb(&["quota", "codex", "--json"], &env);
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["windows"]["five_hour"]["used_percent"], 7.0);
    let note = stderr(&output);
    assert!(
        note.contains(account.join("auth.json").to_str().unwrap()),
        "{note}"
    );
    assert!(!note.contains(FAKE_TOKEN), "{note}");
}

/// A Claude config directory whose credential file cannot be used, so a run
/// fails before any request and names the file it tried.
fn broken_claude_dir(dir: &Path) -> String {
    fs::create_dir_all(dir).unwrap();
    let file = dir.join(".credentials.json");
    fs::write(&file, format!("not json {FAKE_TOKEN}")).unwrap();
    file.to_str().unwrap().to_owned()
}

#[test]
fn claude_credential_directory_follows_the_documented_precedence() {
    let root = scratch("claude-precedence");
    let (flag, var, atb_home, home) = (
        root.join("flag"),
        root.join("var"),
        root.join("atb-home"),
        root.join("home"),
    );
    let flag_file = broken_claude_dir(&flag);
    let var_file = broken_claude_dir(&var);
    let atb_home_file = broken_claude_dir(&atb_home.join(".claude"));
    let home_file = broken_claude_dir(&home.join(".claude"));

    let all = [
        ("CLAUDE_CONFIG_DIR", var.as_path()),
        ("ATB_HOME", atb_home.as_path()),
        ("HOME", home.as_path()),
    ];
    let named = |args: &[&str], env: &[(&str, &Path)]| {
        let output = atb(args, env);
        assert_eq!(output.status.code(), Some(1));
        let message = stderr(&output);
        assert!(!message.contains(FAKE_TOKEN), "{message}");
        message
    };
    let with_flag = ["quota", "claude", "--config-dir", flag.to_str().unwrap()];
    assert!(named(&with_flag, &all).contains(&flag_file));
    assert!(named(&["quota", "claude"], &all).contains(&var_file));
    assert!(named(&["quota", "claude"], &all[1..]).contains(&atb_home_file));
    assert!(named(&["quota", "claude"], &all[2..]).contains(&home_file));
}

#[test]
fn grok_home_relocates_the_credential_file() {
    let root = scratch("grok-home-var");
    fs::write(root.join("auth.json"), format!("not json {FAKE_TOKEN}")).unwrap();
    let output = atb(&["quota", "grok"], &[("GROK_HOME", &root)]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains(root.join("auth.json").to_str().unwrap()),
        "{message}"
    );
    assert!(!message.contains(FAKE_TOKEN), "{message}");
}

#[test]
fn token_variable_without_its_id_fails_without_reading_any_file() {
    let root = scratch("token-without-id");
    // A credential file is present and unusable: reading it would produce a
    // different error (Grok) or the rollout fallback (Codex).
    fs::write(root.join("auth.json"), "not json").unwrap();
    write_rollout(
        &root,
        "2026/08/31",
        "x",
        &[token_count(
            "2026-08-31T10:00:00.000Z",
            300,
            7.0,
            Value::Null,
        )],
    );
    let token = Path::new(FAKE_TOKEN);
    let dir = root.to_str().unwrap();
    for (harness, token_var, id_var) in [
        ("codex", "ATB_CODEX_TOKEN", "ATB_CODEX_ACCOUNT_ID"),
        ("grok", "ATB_GROK_TOKEN", "ATB_GROK_USER_ID"),
    ] {
        let output = atb(
            &["quota", harness, "--config-dir", dir],
            &[(token_var, token)],
        );
        assert_eq!(output.status.code(), Some(1), "{harness}");
        assert!(output.stdout.is_empty(), "{harness}");
        let message = stderr(&output);
        assert!(message.contains(id_var), "{message}");
        assert!(!message.contains("auth.json"), "{message}");
        assert!(!message.contains(FAKE_TOKEN), "{message}");
    }
}

#[cfg(unix)]
#[test]
fn version_probe_child_does_not_inherit_credential_variables() {
    use std::os::unix::fs::PermissionsExt;

    const VARS: [&str; 5] = [
        "ATB_CLAUDE_TOKEN",
        "ATB_CODEX_TOKEN",
        "ATB_CODEX_ACCOUNT_ID",
        "ATB_GROK_TOKEN",
        "ATB_GROK_USER_ID",
    ];
    let bin = scratch("version-probe-env");
    // A stand-in `claude` that records which variables it can see (presence
    // only) and prints no version, so the run stops before any request.
    let script = bin.join("claude");
    let checks: String = VARS
        .iter()
        .map(|var| format!("[ -n \"${{{var}+x}}\" ] && echo {var} >> \"$0.seen\"\n"))
        .collect();
    fs::write(&script, format!("#!/bin/sh\n: > \"$0.seen\"\n{checks}")).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let token = Path::new(FAKE_TOKEN);
    let mut env = vec![("PATH", bin.as_path())];
    env.extend(VARS.iter().map(|var| (*var, token)));
    let output = atb(&["quota", "claude"], &env);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("cannot parse a version"),
        "{}",
        stderr(&output)
    );
    // The file exists, so the stand-in ran; it is empty, so it saw none.
    assert_eq!(fs::read_to_string(bin.join("claude.seen")).unwrap(), "");
}
