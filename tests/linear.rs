//! End-to-end runs of `atb linear` against a fake GraphQL server started in
//! the test. No request leaves the machine, and every key is synthetic.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

const KEY: &str = "synthetic-linear-key-not-a-credential";
const NEW_KEY: &str = "synthetic-linear-key-not-a-credential-rotated";
const ISSUE: &str = "ABC-123";

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("linear")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct Request {
    auth: String,
    query: String,
    variables: Value,
}

struct State {
    /// Keys answered normally; any other key gets a 401.
    accepted: Vec<String>,
    /// From this many requests on, every key gets a 401.
    revoke_after: usize,
    requests: Vec<Request>,
    /// The team's workflow states as (id, name, type, position).
    states: Vec<(String, String, String, f64)>,
    /// The name of the issue's state.
    issue_state: String,
    /// (id, createdAt, body)
    comments: Vec<(String, String, String)>,
    /// (id, name, team id or none for a workspace label)
    labels: Vec<(String, String, Option<String>)>,
    created: Vec<Value>,
    viewer_id: String,
}

impl State {
    fn add_comment(&mut self, body: &str) -> String {
        let n = self.comments.len();
        let id = format!("c{n:03}");
        let at = format!("2026-10-08T00:00:{n:02}.000Z");
        self.comments.push((id.clone(), at, body.to_owned()));
        id
    }

    /// Replace the team's states; ids are `s-<name>`.
    fn set_states(&mut self, states: &[(&str, &str, f64)]) {
        self.states = states
            .iter()
            .map(|(name, kind, position)| {
                (
                    format!("s-{name}"),
                    (*name).to_owned(),
                    (*kind).to_owned(),
                    *position,
                )
            })
            .collect();
    }

    fn answer(&mut self, query: &str, vars: &Value) -> Value {
        if query.contains("issueUpdate") {
            let state = self.states.iter().find(|s| vars["s"] == s.0).unwrap();
            self.issue_state = state.1.clone();
            json!({"issueUpdate": {"success": true}})
        } else if query.contains("commentCreate") {
            let id = self.add_comment(vars["b"].as_str().unwrap());
            json!({"commentCreate": {"success": true, "comment": {"id": id}}})
        } else if query.contains("comments(") {
            // Pages of two, so every read-back with more than two comments
            // walks the pagination.
            let start: usize = vars["after"].as_str().map_or(0, |c| c.parse().unwrap());
            let end = (start + 2).min(self.comments.len());
            let nodes: Vec<Value> = self.comments[start..end]
                .iter()
                .map(|(id, at, body)| json!({"id": id, "createdAt": at, "body": body}))
                .collect();
            json!({"issue": {"comments": {"nodes": nodes, "pageInfo": {
                "hasNextPage": end < self.comments.len(),
                "endCursor": end.to_string(),
            }}}})
        } else if query.contains("issueLabelCreate") {
            let id = format!("l-{}", self.labels.len());
            self.labels.push((
                id.clone(),
                vars["n"].as_str().unwrap().to_owned(),
                vars["t"].as_str().map(str::to_owned),
            ));
            json!({"issueLabelCreate": {"success": true, "issueLabel": {"id": id}}})
        } else if query.contains("issueLabels") {
            let nodes: Vec<Value> = self
                .labels
                .iter()
                .filter(|(_, name, _)| vars["n"] == *name)
                .map(|(id, _, team)| {
                    json!({"id": id, "team": team.as_ref().map(|t| json!({"id": t}))})
                })
                .collect();
            json!({"issueLabels": {"nodes": nodes}})
        } else if query.contains("issueCreate") {
            self.created.push(vars["i"].clone());
            json!({"issueCreate": {"success": true, "issue": {
                "identifier": "ABC-9", "url": "https://linear.example/ABC-9"}}})
        } else if query.contains("teams(") {
            let nodes = if vars["k"] == "TEAM" {
                json!([{"id": "t-1"}])
            } else {
                json!([])
            };
            json!({"teams": {"nodes": nodes}})
        } else if query.contains("projects(") {
            let nodes = if vars["n"] == "Alpha" {
                json!([{"id": "p-1"}])
            } else {
                json!([])
            };
            json!({"team": {"projects": {"nodes": nodes}}})
        } else if query.contains("issue(") {
            let states: Vec<Value> = self
                .states
                .iter()
                .map(|(id, name, kind, position)| {
                    json!({"id": id, "name": name, "type": kind, "position": position})
                })
                .collect();
            json!({"issue": {"id": "i-1", "identifier": ISSUE,
                "state": {"name": self.issue_state},
                "team": {"states": {"nodes": states}}}})
        } else if query.contains("viewer") {
            json!({"viewer": {"id": self.viewer_id}})
        } else {
            panic!("the fake server does not know {query}");
        }
    }
}

struct Fake {
    url: String,
    state: Arc<Mutex<State>>,
    /// `ATB_HOME` for runs against this server, so no test reads the real
    /// `~/.config/linear/config.json`.
    home: PathBuf,
}

impl Fake {
    fn start(accepted: &[&str]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/graphql");
        let mut state = State {
            accepted: accepted.iter().map(|k| (*k).to_owned()).collect(),
            revoke_after: usize::MAX,
            requests: Vec::new(),
            states: Vec::new(),
            issue_state: "Todo".into(),
            comments: Vec::new(),
            labels: Vec::new(),
            created: Vec::new(),
            viewer_id: "u-1".into(),
        };
        state.set_states(&[
            ("Backlog", "backlog", 0.0),
            ("Todo", "unstarted", 1.0),
            ("In Progress", "started", 2.0),
            ("Done", "completed", 3.0),
            ("Canceled", "canceled", 4.0),
        ]);
        let state = Arc::new(Mutex::new(state));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                serve(stream.unwrap(), &shared);
            }
        });
        let home = scratch(&format!("home-{port}"));
        Self { url, state, home }
    }

    fn write_config(&self, text: &str) -> PathBuf {
        let dir = self.home.join(".config/linear");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, text).unwrap();
        path
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn comment_bodies(&self) -> Vec<String> {
        self.state().comments.iter().map(|c| c.2.clone()).collect()
    }

    fn mutations(&self) -> usize {
        let state = self.state();
        state
            .requests
            .iter()
            .filter(|r| r.query.starts_with("mutation"))
            .count()
    }

    /// The index of the first request whose query contains `needle`.
    fn first(&self, needle: &str) -> Option<usize> {
        self.state()
            .requests
            .iter()
            .position(|r| r.query.contains(needle))
    }
}

/// Answer one request on one connection, then close it.
fn serve(stream: TcpStream, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream);
    let mut length = 0;
    let mut auth = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap(),
            "authorization" => auth = value.trim().to_owned(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let query = body["query"].as_str().unwrap().to_owned();
    let (status, payload) = {
        let mut state = state.lock().unwrap();
        let accepted = state.accepted.contains(&auth) && state.requests.len() < state.revoke_after;
        state.requests.push(Request {
            auth,
            query: query.clone(),
            variables: body["variables"].clone(),
        });
        if accepted {
            (
                "200 OK",
                json!({"data": state.answer(&query, &body["variables"])}),
            )
        } else {
            (
                "401 Unauthorized",
                json!({"errors": [{"message": "Authentication required"}]}),
            )
        }
    };
    let payload = payload.to_string();
    let mut stream = reader.into_inner();
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
    .unwrap();
}

/// Run `atb` with nothing inherited but `PATH`, and check that no synthetic
/// key reaches its output.
fn atb(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atb"));
    command
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap());
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    for key in [KEY, NEW_KEY] {
        assert!(
            !stdout(&output).contains(key),
            "key in stdout: {}",
            stdout(&output)
        );
        assert!(
            !stderr(&output).contains(key),
            "key in stderr: {}",
            stderr(&output)
        );
    }
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn linear(fake: &Fake, args: &[&str]) -> Output {
    let mut all = vec!["linear"];
    all.extend_from_slice(args);
    atb(
        &all,
        &[
            ("LINEAR_API_URL", &fake.url),
            ("LINEAR_API_KEY", KEY),
            ("ATB_HOME", fake.home.to_str().unwrap()),
        ],
    )
}

fn release(fake: &Fake, extra: &[&str]) -> Output {
    let mut args = vec!["release", ISSUE, "--agent", "agent-a"];
    args.extend_from_slice(extra);
    linear(fake, &args)
}

fn claim(fake: &Fake, agent: &str) -> Output {
    linear(
        fake,
        &[
            "claim",
            ISSUE,
            "--agent",
            agent,
            "--source",
            "thread-1",
            "--scope",
            "repo: src/",
        ],
    )
}

#[test]
fn claim_sets_the_started_state_and_writes_three_lines() {
    let fake = Fake::start(&[KEY]);
    let output = claim(&fake, "agent-a");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "claimed ABC-123");
    assert_eq!(fake.state().issue_state, "In Progress");
    assert_eq!(
        fake.comment_bodies(),
        ["claim: agent-a thread-1\nscope: repo: src/\nfrom: Todo"]
    );
    assert!(fake.state().requests.iter().all(|r| r.auth == KEY));
}

#[test]
fn claim_picks_the_started_state_with_the_lowest_position_then_name() {
    let fake = Fake::start(&[KEY]);
    // List or name order would pick Alpha; position alone would pick Working.
    fake.state().set_states(&[
        ("Todo", "unstarted", 0.0),
        ("Alpha", "started", 3.0),
        ("Working", "started", 1.0),
        ("Doing", "started", 1.0),
    ]);
    let output = claim(&fake, "agent-a");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.state().issue_state, "Doing");
}

#[test]
fn a_config_override_names_the_state_and_a_missing_one_is_an_error() {
    let fake = Fake::start(&[KEY]);
    fake.state().set_states(&[
        ("Todo", "unstarted", 0.0),
        ("In Progress", "started", 1.0),
        ("Active", "started", 2.0),
    ]);
    fake.write_config(r#"{"states": {"started": "Active"}}"#);
    let output = claim(&fake, "agent-a");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.state().issue_state, "Active");

    let fake = Fake::start(&[KEY]);
    fake.write_config(r#"{"states": {"started": "Active"}}"#);
    let output = claim(&fake, "agent-a");
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(
        err.contains("started") && err.contains("\"Active\""),
        "{err}"
    );
    assert_eq!(fake.mutations(), 0);
}

#[test]
fn a_malformed_config_is_an_error_naming_the_path_without_its_content() {
    let fake = Fake::start(&[KEY]);
    let path = fake.write_config(r#"{"states": {"started": distinctive-config-text"#);
    let output = claim(&fake, "agent-a");
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains(path.to_str().unwrap()), "{err}");
    assert!(!err.contains("distinctive-config-text"), "{err}");
    assert!(fake.state().requests.is_empty());
}

#[test]
fn release_restores_the_state_recorded_by_claim() {
    let fake = Fake::start(&[KEY]);
    fake.state().issue_state = "Backlog".into();
    let output = claim(&fake, "agent-a");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.state().issue_state, "In Progress");
    let output = release(&fake, &["--reason", "blocked"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.state().issue_state, "Backlog");
}

#[test]
fn release_falls_back_to_the_first_unstarted_then_the_first_backlog_state() {
    // A 0.2.0 claim, without a `from:` line.
    let fake = Fake::start(&[KEY]);
    fake.state().set_states(&[
        ("Backlog", "backlog", 0.0),
        ("Later", "unstarted", 5.0),
        ("Ready", "unstarted", 1.0),
        ("In Progress", "started", 2.0),
    ]);
    fake.state().issue_state = "In Progress".into();
    fake.state()
        .add_comment("claim: agent-a thread-1\nscope: repo: src/");
    let output = release(&fake, &["--reason", "blocked"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.state().issue_state, "Ready");

    // The recorded state is gone and so is every unstarted state.
    let fake = Fake::start(&[KEY]);
    fake.state().set_states(&[
        ("Archive", "backlog", 4.0),
        ("Backlog", "backlog", 0.0),
        ("In Progress", "started", 2.0),
    ]);
    fake.state().issue_state = "In Progress".into();
    fake.state()
        .add_comment("claim: agent-a thread-1\nscope: repo: src/\nfrom: Todo");
    let output = release(&fake, &["--reason", "blocked"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.state().issue_state, "Backlog");
}

#[test]
fn the_unstarted_fallback_honours_the_config_override() {
    let fake = Fake::start(&[KEY]);
    fake.state().set_states(&[
        ("Todo", "unstarted", 0.0),
        ("Ready", "unstarted", 1.0),
        ("In Progress", "started", 2.0),
    ]);
    fake.write_config(r#"{"states": {"unstarted": "Ready"}}"#);
    fake.state().issue_state = "In Progress".into();
    fake.state().add_comment("claim: agent-a thread-1");
    let output = release(&fake, &["--reason", "blocked"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.state().issue_state, "Ready");
}

#[test]
fn release_without_a_state_to_restore_writes_the_comment_then_fails() {
    let fake = Fake::start(&[KEY]);
    fake.state()
        .set_states(&[("In Progress", "started", 0.0), ("Done", "completed", 1.0)]);
    fake.state().issue_state = "In Progress".into();
    fake.state().add_comment("claim: agent-a thread-1");
    let output = release(&fake, &["--reason", "blocked"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("fall back"), "{}", stderr(&output));
    assert_eq!(fake.comment_bodies()[1], "release: agent-a blocked");
    assert_eq!(fake.first("issueUpdate"), None);
}

#[test]
fn claim_after_another_agents_unreleased_claim_loses() {
    let fake = Fake::start(&[KEY]);
    {
        let mut state = fake.state();
        state.add_comment("note");
        state.add_comment("claim: agent-b thread-2\nscope: repo: docs/");
    }
    let output = claim(&fake, "agent-a");
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("held by agent-b"),
        "{}",
        stderr(&output)
    );
    let bodies = fake.comment_bodies();
    assert_eq!(bodies.len(), 4);
    assert_eq!(bodies[3], "release: agent-a lost");
}

#[test]
fn claim_after_a_released_claim_or_its_own_claim_wins() {
    let fake = Fake::start(&[KEY]);
    {
        let mut state = fake.state();
        state.add_comment("claim: agent-b thread-2");
        state.add_comment("release: agent-b merged");
        state.add_comment("claim: agent-a thread-1");
    }
    let output = claim(&fake, "agent-a");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.comment_bodies().len(), 4);
}

#[test]
fn release_by_a_non_holder_or_without_a_holder_changes_nothing() {
    let fake = Fake::start(&[KEY]);
    let args = [
        "release", ISSUE, "--agent", "agent-a", "--reason", "merged", "--done",
    ];
    let output = linear(&fake, &args);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(stderr(&output).contains("no holder"), "{}", stderr(&output));

    fake.state().add_comment("claim: agent-b thread-2");
    let output = linear(&fake, &args);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("held by agent-b"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);
    assert_eq!(fake.comment_bodies().len(), 1);
    assert_eq!(fake.state().issue_state, "Todo");
}

#[test]
fn release_by_the_holder_comments_then_sets_the_first_completed_state() {
    let fake = Fake::start(&[KEY]);
    fake.state().set_states(&[
        ("Todo", "unstarted", 0.0),
        ("Closed", "completed", 9.0),
        ("Done", "completed", 3.0),
    ]);
    fake.state().add_comment("claim: agent-a thread-1");
    let output = linear(
        &fake,
        &[
            "release", ISSUE, "--agent", "agent-a", "--reason", "merged", "--done",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.comment_bodies()[1], "release: agent-a merged");
    assert_eq!(fake.state().issue_state, "Done");
    assert!(fake.first("commentCreate").unwrap() < fake.first("issueUpdate").unwrap());
}

#[test]
fn forced_release_names_the_holder_and_restores_the_state_with_the_deprecated_todo() {
    let fake = Fake::start(&[KEY]);
    fake.state().issue_state = "In Progress".into();
    fake.state()
        .add_comment("claim: agent-b thread-2\nscope: repo: src/\nfrom: Backlog");
    let output = release(&fake, &["--force", "stale for three days", "--todo"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        fake.comment_bodies()[1],
        "release: agent-b forced by agent-a: stale for three days"
    );
    assert_eq!(fake.state().issue_state, "Backlog");
}

/// A key command that records each run in `counter` and prints `key_file`.
struct KeyCommand {
    home: PathBuf,
    counter: PathBuf,
}

impl KeyCommand {
    fn new(name: &str, prints: &str) -> Self {
        let home = scratch(name);
        fs::write(home.join("key"), format!("{prints}\n")).unwrap();
        let counter = home.join("runs");
        Self { home, counter }
    }

    fn cache(&self) -> PathBuf {
        self.home.join(".config/linear/api-key")
    }

    fn runs(&self) -> usize {
        fs::read_to_string(&self.counter).map_or(0, |text| text.lines().count())
    }

    fn query(&self, fake: &Fake) -> Output {
        self.run(fake, &["linear", "query", "{ viewer { id } }"])
    }

    fn run(&self, fake: &Fake, args: &[&str]) -> Output {
        let command = format!(
            "echo run >> '{}'; cat '{}'",
            self.counter.display(),
            self.home.join("key").display()
        );
        atb(
            args,
            &[
                ("LINEAR_API_URL", &fake.url),
                ("LINEAR_API_KEY_CMD", &command),
                ("ATB_HOME", self.home.to_str().unwrap()),
            ],
        )
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn key_command_without_a_cache_runs_once_and_writes_a_private_cache() {
    let fake = Fake::start(&[KEY]);
    let cmd = KeyCommand::new("key-no-cache", KEY);
    let output = cmd.query(&fake);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(cmd.runs(), 1);
    assert_eq!(fs::read_to_string(cmd.cache()).unwrap(), KEY);
    assert_eq!(mode(&cmd.cache()), 0o600);
    assert_eq!(mode(cmd.cache().parent().unwrap()), 0o700);
}

#[test]
fn a_fresh_cache_is_used_without_running_the_command() {
    let fake = Fake::start(&[KEY]);
    let cmd = KeyCommand::new("key-fresh-cache", "synthetic-wrong-key");
    fs::create_dir_all(cmd.cache().parent().unwrap()).unwrap();
    fs::write(cmd.cache(), KEY).unwrap();
    let output = cmd.query(&fake);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(cmd.runs(), 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"viewer": {"id": "u-1"}})
    );
}

#[test]
fn a_401_on_a_cached_key_refetches_once_and_retries_once() {
    let fake = Fake::start(&[NEW_KEY]);
    let cmd = KeyCommand::new("key-401-refresh", NEW_KEY);
    fs::create_dir_all(cmd.cache().parent().unwrap()).unwrap();
    fs::write(cmd.cache(), KEY).unwrap();
    let output = cmd.query(&fake);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(cmd.runs(), 1);
    let auths: Vec<String> = fake
        .state()
        .requests
        .iter()
        .map(|r| r.auth.clone())
        .collect();
    assert_eq!(auths, [KEY, NEW_KEY]);
    assert_eq!(fs::read_to_string(cmd.cache()).unwrap(), NEW_KEY);
}

#[test]
fn a_second_401_is_an_error() {
    let fake = Fake::start(&[]);
    let cmd = KeyCommand::new("key-401-twice", NEW_KEY);
    fs::create_dir_all(cmd.cache().parent().unwrap()).unwrap();
    fs::write(cmd.cache(), KEY).unwrap();
    let output = cmd.query(&fake);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("HTTP 401"), "{}", stderr(&output));
    assert_eq!(cmd.runs(), 1);
    assert_eq!(fake.state().requests.len(), 2);
}

#[test]
fn a_run_refreshes_the_key_at_most_once() {
    // The first request refreshes the cached key; the fourth (the claim
    // comment) is refused again and must not run the command a second time.
    let fake = Fake::start(&[NEW_KEY]);
    fake.state().revoke_after = 3;
    let cmd = KeyCommand::new("key-refresh-once", NEW_KEY);
    fs::create_dir_all(cmd.cache().parent().unwrap()).unwrap();
    fs::write(cmd.cache(), KEY).unwrap();
    let output = cmd.run(
        &fake,
        &[
            "linear", "claim", ISSUE, "--agent", "agent-a", "--source", "s", "--scope", "r: x",
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(cmd.runs(), 1);
    assert_eq!(fake.state().requests.len(), 4);
}

#[test]
fn a_failing_key_command_reports_only_its_exit_status() {
    let fake = Fake::start(&[KEY]);
    let home = scratch("key-command-fails");
    let output = atb(
        &["linear", "query", "{ viewer { id } }"],
        &[
            ("LINEAR_API_URL", &fake.url),
            (
                "LINEAR_API_KEY_CMD",
                "echo distinctive-vault-message >&2; exit 7",
            ),
            ("ATB_HOME", home.to_str().unwrap()),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(
        err.contains("LINEAR_API_KEY_CMD failed") && err.contains('7'),
        "{err}"
    );
    assert!(!err.contains("distinctive-vault-message"), "{err}");
    assert!(!stdout(&output).contains("distinctive-vault-message"));
    assert!(fake.state().requests.is_empty());
}

fn create(fake: &Fake, extra: &[&str]) -> Output {
    let description = fake.home.join("description.md");
    fs::write(&description, "Body text.\n").unwrap();
    let mut args = vec![
        "create",
        "--team",
        "TEAM",
        "--project",
        "Alpha",
        "--title",
        "Do a thing",
        "--description-file",
        description.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    linear(fake, &args)
}

#[test]
fn create_without_labels_adds_none_and_looks_none_up() {
    let fake = Fake::start(&[KEY]);
    let output = create(&fake, &["--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"identifier": "ABC-9", "url": "https://linear.example/ABC-9"})
    );
    assert_eq!(
        fake.state().created,
        [
            json!({"teamId": "t-1", "title": "Do a thing", "description": "Body text.\n",
                "projectId": "p-1"})
        ]
    );
    assert_eq!(fake.first("issueLabel"), None);
}

#[test]
fn create_merges_flag_and_default_labels_and_creates_a_missing_one_once() {
    let fake = Fake::start(&[KEY]);
    fake.state().labels.push(("l-ws".into(), "x".into(), None));
    fake.write_config(r#"{"default_labels": ["y", "z"]}"#);
    let args = ["--label", "x", "--label", "y", "--label", "x"];

    let output = create(&fake, &args);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "ABC-9 https://linear.example/ABC-9");
    let output = create(&fake, &args);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let state = fake.state();
    assert_eq!(
        state.labels[1..],
        [
            ("l-1".to_owned(), "y".to_owned(), Some("t-1".to_owned())),
            ("l-2".to_owned(), "z".to_owned(), Some("t-1".to_owned())),
        ]
    );
    assert_eq!(state.created.len(), 2);
    for input in &state.created {
        assert_eq!(input["labelIds"], json!(["l-ws", "l-1", "l-2"]));
    }
    let creates = state
        .requests
        .iter()
        .filter(|r| r.query.contains("issueLabelCreate"));
    assert_eq!(creates.count(), 2);
}

#[test]
fn a_key_echoed_by_the_server_is_masked_on_stdout_and_stderr() {
    // atb() itself fails the test if a key reaches stdout or stderr.
    let fake = Fake::start(&[KEY]);
    fake.state().viewer_id = format!("prefix {KEY} suffix");
    let output = linear(&fake, &["query", "{ viewer { id } }"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"viewer": {"id": "prefix *** suffix"}})
    );

    fake.state().add_comment(&format!("claim: {KEY} thread-2"));
    let output = linear(
        &fake,
        &["release", ISSUE, "--agent", "agent-a", "--reason", "merged"],
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("held by ***"),
        "{}",
        stderr(&output)
    );

    let output = linear(
        &fake,
        &["release", ISSUE, "--agent", "agent-a", "--force", "stale"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "released ABC-123 (held by ***)");
}

#[test]
fn query_refuses_a_mutation_before_any_request_and_passes_a_query() {
    let fake = Fake::start(&[KEY]);
    let output = linear(
        &fake,
        &[
            "query",
            "query Q { viewer { id } }\nmutation { issueUpdate(id: \"x\", input: {}) { success } }",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("read-only"), "{}", stderr(&output));
    assert!(fake.state().requests.is_empty());

    let home = scratch("query-file");
    let file = home.join("q.graphql");
    fs::write(&file, "{ viewer { id } } # not a mutation").unwrap();
    let output = linear(&fake, &["query", file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"viewer": {"id": "u-1"}})
    );
    let state = fake.state();
    assert_eq!(state.requests.len(), 1);
    assert_eq!(state.requests[0].variables, json!({}));
}
