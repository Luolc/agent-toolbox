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
const ISSUE_URL: &str = "https://linear.example/ABC-123";
/// A second issue on the same team, id `i-2`, for `relate`.
const OTHER: &str = "ABC-124";

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
    /// This many state updates get a 503 before one goes through.
    failing_state_updates: usize,
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
    /// (id, name, team ids, archived, trashed)
    projects: Vec<(String, String, Vec<String>, bool, bool)>,
    created_projects: Vec<Value>,
    viewer_id: String,
    /// The id of the project ISSUE is in.
    issue_project: Option<String>,
    /// (issue id, related issue id, type)
    relations: Vec<(String, String, String)>,
    issue_title: String,
    issue_description: Option<String>,
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

    fn add_project(&mut self, name: &str, teams: &[&str]) -> String {
        let id = format!("p-{}", self.projects.len() + 10);
        let teams = teams.iter().map(|t| (*t).to_owned()).collect();
        self.projects
            .push((id.clone(), name.to_owned(), teams, false, false));
        id
    }

    fn add_archived_project(&mut self, name: &str, teams: &[&str]) {
        self.add_project(name, teams);
        self.projects.last_mut().unwrap().3 = true;
    }

    /// Trashed (deleted) projects are archived too.
    fn add_trashed_project(&mut self, name: &str, teams: &[&str]) {
        self.add_archived_project(name, teams);
        self.projects.last_mut().unwrap().4 = true;
    }

    fn answer(&mut self, query: &str, vars: &Value) -> Value {
        if query.contains("team(id") && query.contains("projects(first") {
            // A team's projects in pages of one, names compared ignoring case;
            // archived (and so trashed) ones left out, as Linear does without
            // includeArchived.
            let matching: Vec<_> = self
                .projects
                .iter()
                .filter(|p| p.1.eq_ignore_ascii_case(vars["n"].as_str().unwrap()))
                .filter(|p| p.2.iter().any(|t| vars["t"] == *t))
                .filter(|p| !p.3 || query.contains("includeArchived: true"))
                .collect();
            let start: usize = vars["after"].as_str().map_or(0, |c| c.parse().unwrap());
            let end = (start + 1).min(matching.len());
            let nodes: Vec<Value> = matching[start..end]
                .iter()
                .map(|p| json!({"id": p.0, "name": p.1, "url": project_url(&p.0)}))
                .collect();
            json!({"team": {"projects": {"nodes": nodes, "pageInfo": {
                "hasNextPage": end < matching.len(),
                "endCursor": end.to_string(),
            }}}})
        } else if query.contains("projectCreate") {
            let input = vars["i"].clone();
            let name = input["name"].as_str().unwrap().to_owned();
            let teams: Vec<&str> = input["teamIds"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap())
                .collect();
            let id = self.add_project(&name, &teams);
            self.created_projects.push(input);
            json!({"projectCreate": {"success": true, "project": {
                "id": id, "name": name, "url": project_url(&id)}}})
        } else if query.contains("projects(first") {
            // Pages of one, and names compared ignoring case as a collation
            // might: atb must paginate and compare names itself. Archived
            // projects only with includeArchived, as Linear does.
            let archived = query.contains("includeArchived: true");
            let matching: Vec<_> = self
                .projects
                .iter()
                .filter(|p| p.1.eq_ignore_ascii_case(vars["n"].as_str().unwrap()))
                .filter(|p| archived || !p.3)
                .collect();
            let start: usize = vars["after"].as_str().map_or(0, |c| c.parse().unwrap());
            let end = (start + 1).min(matching.len());
            let nodes: Vec<Value> = matching[start..end]
                .iter()
                .map(|(id, name, teams, archived, trashed)| {
                    let teams: Vec<Value> = teams
                        .iter()
                        .filter(|t| vars["t"] == **t)
                        .map(|t| json!({"id": t}))
                        .collect();
                    let archived_at = archived.then_some("2026-10-01T00:00:00.000Z");
                    json!({"id": id, "name": name, "url": project_url(id),
                        "archivedAt": archived_at, "trashed": trashed.then_some(true),
                        "teams": {"nodes": teams}})
                })
                .collect();
            json!({"projects": {"nodes": nodes, "pageInfo": {
                "hasNextPage": end < matching.len(),
                "endCursor": end.to_string(),
            }}})
        } else if query.contains("IssueUpdateInput") {
            let input = &vars["i"];
            if let Some(title) = input["title"].as_str() {
                self.issue_title = title.to_owned();
            }
            if let Some(description) = input["description"].as_str() {
                self.issue_description = Some(description.to_owned());
            }
            json!({"issueUpdate": {"success": true}})
        } else if query.contains("issueUpdate") && query.contains("projectId") {
            self.issue_project = Some(vars["p"].as_str().unwrap().to_owned());
            json!({"issueUpdate": {"success": true}})
        } else if query.contains("issueRelationCreate") {
            let input = &vars["i"];
            self.relations.push((
                input["issueId"].as_str().unwrap().to_owned(),
                input["relatedIssueId"].as_str().unwrap().to_owned(),
                input["type"].as_str().unwrap().to_owned(),
            ));
            json!({"issueRelationCreate": {"success": true}})
        } else if query.contains("issueUpdate") {
            let state = self.states.iter().find(|s| vars["s"] == s.0).unwrap();
            self.issue_state = state.1.clone();
            json!({"issueUpdate": {"success": true}})
        } else if query.contains("commentCreate") {
            let id = self.add_comment(vars["b"].as_str().unwrap());
            let url = format!("https://linear.example/comment/{id}");
            json!({"commentCreate": {"success": true, "comment": {"id": id, "url": url}}})
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
        } else if query.contains("elations(first") {
            // Pages of one, from the side of the issue asked about.
            let inverse = query.contains("inverseRelations");
            let field = if inverse {
                "inverseRelations"
            } else {
                "relations"
            };
            let mine: Vec<_> = self
                .relations
                .iter()
                .filter(|r| vars["id"] == *if inverse { &r.1 } else { &r.0 })
                .collect();
            let start: usize = vars["after"].as_str().map_or(0, |c| c.parse().unwrap());
            let end = (start + 1).min(mine.len());
            let nodes: Vec<Value> = mine[start..end]
                .iter()
                .map(|(from, to, kind)| {
                    if inverse {
                        json!({"type": kind, "issue": {"id": from}})
                    } else {
                        json!({"type": kind, "relatedIssue": {"id": to}})
                    }
                })
                .collect();
            json!({"issue": {field: {"nodes": nodes, "pageInfo": {
                "hasNextPage": end < mine.len(),
                "endCursor": end.to_string(),
            }}}})
        } else if query.contains("issue(") && (vars["id"] == OTHER || vars["id"] == "i-2") {
            json!({"issue": {"id": "i-2", "identifier": OTHER, "project": null,
                "team": {"id": "t-1", "key": "TEAM"}}})
        } else if query.contains("issue(") {
            // One issue, looked up by identifier or by id.
            if vars["id"] != ISSUE && vars["id"] != "i-1" {
                return json!({"issue": null});
            }
            let project = self.issue_project.as_ref().map(|id| {
                let name = &self.projects.iter().find(|p| &p.0 == id).unwrap().1;
                json!({"id": id, "name": name, "url": project_url(id)})
            });
            let states: Vec<Value> = self
                .states
                .iter()
                .map(|(id, name, kind, position)| {
                    json!({"id": id, "name": name, "type": kind, "position": position})
                })
                .collect();
            let kind = &self
                .states
                .iter()
                .find(|s| s.1 == self.issue_state)
                .unwrap()
                .2;
            json!({"issue": {"id": "i-1", "identifier": ISSUE, "url": ISSUE_URL,
                "title": self.issue_title, "description": self.issue_description,
                "state": {"name": self.issue_state, "type": kind}, "project": project,
                "team": {"id": "t-1", "key": "TEAM", "states": {"nodes": states}}}})
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
            failing_state_updates: 0,
            requests: Vec::new(),
            states: Vec::new(),
            issue_state: "Todo".into(),
            comments: Vec::new(),
            labels: Vec::new(),
            created: Vec::new(),
            projects: Vec::new(),
            created_projects: Vec::new(),
            viewer_id: "u-1".into(),
            issue_project: None,
            relations: Vec::new(),
            issue_title: "Old title".into(),
            issue_description: None,
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

    /// State updates sent so far, including those that failed.
    fn state_updates(&self) -> usize {
        let state = self.state();
        state
            .requests
            .iter()
            .filter(|r| r.query.contains("stateId"))
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

fn project_url(id: &str) -> String {
    format!("https://linear.example/project/{id}")
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
        if accepted && query.contains("stateId") && state.failing_state_updates > 0 {
            state.failing_state_updates -= 1;
            (
                "503 Service Unavailable",
                json!({"errors": [{"message": "Service unavailable"}]}),
            )
        } else if accepted {
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
    // List or name order would pick Alpha; position, then id, would pick Working.
    fake.state().set_states(&[
        ("Todo", "unstarted", 0.0),
        ("Alpha", "started", 3.0),
        ("Working", "started", 1.0),
        ("Doing", "started", 1.0),
    ]);
    // Ids that sort the other way round, so only the name breaks the tie.
    fake.state().states[2].0 = "s-1".into();
    fake.state().states[3].0 = "s-2".into();
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
fn release_without_a_state_to_set_writes_nothing_and_keeps_the_holder() {
    // No state to restore, then no completed state for --done.
    for (states, extra, says) in [
        (
            &[("In Progress", "started", 0.0), ("Done", "completed", 1.0)][..],
            &["--reason", "blocked"][..],
            "fall back",
        ),
        (
            &[("Todo", "unstarted", 0.0), ("In Progress", "started", 1.0)][..],
            &["--reason", "merged", "--done"][..],
            "no completed state",
        ),
    ] {
        let fake = Fake::start(&[KEY]);
        fake.state().set_states(states);
        fake.state().issue_state = "In Progress".into();
        fake.state()
            .add_comment("claim: agent-a thread-1\nfrom: Todo");
        let output = release(&fake, extra);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(stderr(&output).contains(says), "{}", stderr(&output));
        assert_eq!(fake.mutations(), 0);
        assert_eq!(fake.comment_bodies().len(), 1);
        // agent-a still holds the issue.
        let args = ["release", ISSUE, "--agent", "agent-b", "--reason", "x"];
        let output = linear(&fake, &args);
        assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("held by agent-a"),
            "{}",
            stderr(&output)
        );
    }
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

fn create_project(fake: &Fake, extra: &[&str]) -> Output {
    let mut args = vec![
        "project",
        "create",
        "--team",
        "TEAM",
        "--name",
        "Project name",
    ];
    args.extend_from_slice(extra);
    linear(fake, &args)
}

#[test]
fn project_create_creates_a_missing_project_once_and_then_prints_it() {
    let fake = Fake::start(&[KEY]);
    // Same name in another case, on the team: not the project asked for.
    fake.state().add_project("project name", &["t-1"]);
    let description = fake.home.join("description.md");
    fs::write(&description, "# Goal\n\nBody text.\n").unwrap();

    let output = create_project(
        &fake,
        &["--description-file", description.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        "Project name https://linear.example/project/p-11"
    );
    assert_eq!(
        fake.state().created_projects,
        [json!({"name": "Project name", "teamIds": ["t-1"],
            "content": "# Goal\n\nBody text.\n"})]
    );

    let output = create_project(&fake, &["--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"name": "Project name", "url": "https://linear.example/project/p-11",
            "id": "p-11", "created": false})
    );
    assert_eq!(fake.mutations(), 1);
}

#[test]
fn project_create_prints_a_project_already_on_the_team_without_writing() {
    let fake = Fake::start(&[KEY]);
    fake.state().add_project("Project name", &["t-0", "t-1"]);
    let output = create_project(&fake, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        "Project name https://linear.example/project/p-10"
    );
    assert_eq!(fake.mutations(), 0);
}

#[test]
fn project_create_refuses_a_project_on_another_team_or_several_with_the_name() {
    let fake = Fake::start(&[KEY]);
    fake.state().add_project("Project name", &["t-2"]);
    let output = create_project(&fake, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("is not on team TEAM"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);

    // The one on the team comes first; the second is on the next page.
    let fake = Fake::start(&[KEY]);
    fake.state().add_project("Project name", &["t-1"]);
    fake.state().add_project("Project name", &["t-2"]);
    let output = create_project(&fake, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("2 projects are named"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);
}

#[test]
fn project_create_refuses_an_archived_project_on_any_team() {
    for team in ["t-1", "t-2"] {
        let fake = Fake::start(&[KEY]);
        fake.state().add_archived_project("Project name", &[team]);
        let output = create_project(&fake, &[]);
        assert_eq!(output.status.code(), Some(1), "{team}");
        assert!(
            stderr(&output).contains("exists but is archived"),
            "{}",
            stderr(&output)
        );
        assert_eq!(fake.mutations(), 0);
    }

    // An archived project counts toward "more than one".
    let fake = Fake::start(&[KEY]);
    fake.state().add_project("Project name", &["t-1"]);
    fake.state().add_archived_project("Project name", &["t-1"]);
    let output = create_project(&fake, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("2 projects are named"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);
}

#[test]
fn project_create_ignores_a_trashed_project_with_the_name() {
    let fake = Fake::start(&[KEY]);
    fake.state().add_trashed_project("Project name", &["t-1"]);
    let output = create_project(&fake, &["--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"name": "Project name", "url": "https://linear.example/project/p-11",
            "id": "p-11", "created": true})
    );
    assert_eq!(fake.state().created_projects.len(), 1);
}

fn comment(fake: &Fake, issue: &str, body: &str, extra: &[&str]) -> Output {
    let file = fake.home.join("comment.md");
    fs::write(&file, body).unwrap();
    let mut args = vec!["comment", issue, "--body-file", file.to_str().unwrap()];
    args.extend_from_slice(extra);
    linear(fake, &args)
}

#[test]
fn comment_writes_the_file_verbatim_and_prints_its_url() {
    let fake = Fake::start(&[KEY]);
    let body = "  Review notes\n\nclaim: on a later line is text\n\n";
    let output = comment(&fake, ISSUE, body, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "https://linear.example/comment/c000\n");
    let output = comment(&fake, ISSUE, "Second.", &["--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"id": "c001", "url": "https://linear.example/comment/c001"})
    );
    assert_eq!(fake.comment_bodies(), [body, "Second."]);
    assert_eq!(fake.mutations(), 2);
}

#[test]
fn comment_refuses_a_claim_or_release_body_or_an_empty_file_before_any_request() {
    let fake = Fake::start(&[KEY]);
    for body in [
        "claim: agent-b thread-9",
        "\n  release: agent-b merged\nmore",
        "",
        " \n\t\n",
    ] {
        let output = comment(&fake, ISSUE, body, &[]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{body:?}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("nothing written"),
            "{}",
            stderr(&output)
        );
    }
    let missing = fake.home.join("missing.md");
    let output = linear(
        &fake,
        &["comment", ISSUE, "--body-file", missing.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(fake.state().requests.is_empty());
}

#[test]
fn comment_on_a_missing_issue_writes_nothing() {
    let fake = Fake::start(&[KEY]);
    let output = comment(&fake, "ABC-404", "Hello.", &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("no issue ABC-404"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);
}

#[test]
fn create_with_a_parent_sends_the_parents_id() {
    let fake = Fake::start(&[KEY]);
    let output = create(&fake, &["--parent", ISSUE]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.state().created[0]["parentId"], "i-1");
}

#[test]
fn create_with_a_missing_parent_writes_nothing() {
    let fake = Fake::start(&[KEY]);
    let output = create(&fake, &["--parent", "ABC-404", "--label", "new"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("no parent issue ABC-404"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);
}

/// Two canceled states, the configured one second, so that picking the first
/// canceled state would show.
fn abandon_fake(config: Option<&str>) -> Fake {
    let fake = Fake::start(&[KEY]);
    fake.state().set_states(&[
        ("Todo", "unstarted", 0.0),
        ("In Progress", "started", 1.0),
        ("Done", "completed", 2.0),
        ("Canceled", "canceled", 3.0),
        ("Abandoned", "canceled", 4.0),
    ]);
    fake.state().issue_state = "In Progress".into();
    if let Some(config) = config {
        fake.write_config(config);
    }
    fake
}

const ABANDONED_CONFIG: &str = r#"{"states": {"abandoned": "Abandoned"}}"#;

#[test]
fn release_abandon_by_the_holder_comments_then_sets_the_configured_canceled_state() {
    let fake = abandon_fake(Some(ABANDONED_CONFIG));
    fake.state()
        .add_comment("claim: agent-a thread-1\nscope: repo: src/\nfrom: Todo");
    let output = release(&fake, &["--reason", "superseded", "--abandon"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        fake.comment_bodies()[1],
        "release: agent-a abandoned: superseded"
    );
    assert_eq!(fake.state().issue_state, "Abandoned");
    assert!(fake.first("commentCreate").unwrap() < fake.first("issueUpdate").unwrap());
}

#[test]
fn forced_release_abandon_names_the_holder_and_sets_the_configured_state() {
    let fake = abandon_fake(Some(ABANDONED_CONFIG));
    fake.state()
        .add_comment("claim: agent-b thread-2\nfrom: Todo");
    let output = release(&fake, &["--force", "holder gone", "--abandon"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        fake.comment_bodies()[1],
        "release: agent-b forced by agent-a: holder gone"
    );
    assert_eq!(fake.state().issue_state, "Abandoned");
}

#[test]
fn release_abandon_without_a_canceled_state_configured_writes_nothing() {
    for (config, message) in [
        (None, "states"),
        (Some(r#"{"states": {"canceled": "Canceled"}}"#), "states"),
        (
            Some(r#"{"states": {"abandoned": "Done"}}"#),
            "of type completed",
        ),
        (
            Some(r#"{"states": {"abandoned": "Dropped"}}"#),
            "no state \"Dropped\"",
        ),
    ] {
        let fake = abandon_fake(config);
        fake.state()
            .add_comment("claim: agent-a thread-1\nfrom: Todo");
        let output = release(&fake, &["--reason", "superseded", "--abandon"]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{config:?}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(message),
            "{config:?}: {}",
            stderr(&output)
        );
        assert_eq!(fake.mutations(), 0, "{config:?}");
        assert_eq!(fake.state().issue_state, "In Progress");
    }
}

#[test]
fn release_abandon_conflicts_with_done() {
    let fake = abandon_fake(Some(ABANDONED_CONFIG));
    let output = release(&fake, &["--reason", "x", "--abandon", "--done"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(fake.state().requests.is_empty());
}

fn set_project(fake: &Fake, extra: &[&str]) -> Output {
    let mut args = vec!["set-project", ISSUE, "--project", "Project name"];
    args.extend_from_slice(extra);
    linear(fake, &args)
}

#[test]
fn set_project_puts_an_issue_without_a_project_into_the_teams_project() {
    let fake = Fake::start(&[KEY]);
    // Another case, another team, archived: none of them is the project.
    fake.state().add_project("project name", &["t-1"]);
    fake.state().add_project("Project name", &["t-2"]);
    fake.state().add_archived_project("Project name", &["t-1"]);
    let id = fake.state().add_project("Project name", &["t-1"]);
    let output = set_project(&fake, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        format!("{ISSUE} Project name {}", project_url(&id))
    );
    let state = fake.state();
    let writes: Vec<_> = state
        .requests
        .iter()
        .filter(|r| r.query.starts_with("mutation"))
        .collect();
    assert_eq!(writes.len(), 1);
    assert!(writes[0].query.contains("issueUpdate"));
    assert_eq!(writes[0].variables, json!({"id": "i-1", "p": id}));
}

#[test]
fn set_project_prints_an_issue_already_in_the_project_without_writing() {
    let fake = Fake::start(&[KEY]);
    let id = fake.state().add_project("Project name", &["t-1"]);
    fake.state().issue_project = Some(id.clone());
    let output = set_project(&fake, &["--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"identifier": ISSUE, "project": "Project name",
            "url": project_url(&id), "changed": false})
    );
    assert!(
        stderr(&output).contains("already in project"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);
}

#[test]
fn set_project_refuses_an_issue_in_another_project() {
    let fake = Fake::start(&[KEY]);
    let current = fake.state().add_project("Current", &["t-1"]);
    fake.state().add_project("Project name", &["t-1"]);
    fake.state().issue_project = Some(current.clone());
    let output = set_project(&fake, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("is in project \"Current\""),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);
    assert_eq!(fake.state().issue_project, Some(current));
}

#[test]
fn set_project_refuses_a_missing_or_ambiguous_project_or_a_missing_issue() {
    let fake = Fake::start(&[KEY]);
    fake.state().add_archived_project("Project name", &["t-1"]);
    fake.state().add_project("Project name", &["t-2"]);
    let output = set_project(&fake, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("has 0 unarchived projects"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);

    // The second is on the next page.
    let fake = Fake::start(&[KEY]);
    fake.state().add_project("Project name", &["t-1"]);
    fake.state().add_project("Project name", &["t-1"]);
    let output = set_project(&fake, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("has 2 unarchived projects"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);

    let fake = Fake::start(&[KEY]);
    fake.state().add_project("Project name", &["t-1"]);
    let output = linear(
        &fake,
        &["set-project", "ABC-999", "--project", "Project name"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no issue ABC-999"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);
}

fn relate(fake: &Fake, issue: &str, other: &str, extra: &[&str]) -> Output {
    let mut args = vec!["relate", issue, other];
    args.extend_from_slice(extra);
    linear(fake, &args)
}

fn relation(from: &str, to: &str, kind: &str) -> (String, String, String) {
    (from.to_owned(), to.to_owned(), kind.to_owned())
}

#[test]
fn relate_creates_one_related_relation_when_there_is_none() {
    let fake = Fake::start(&[KEY]);
    // Relations with a third issue, on both sides, are not between the two.
    fake.state().relations = vec![
        relation("i-1", "i-9", "blocks"),
        relation("i-9", "i-1", "related"),
    ];
    let output = relate(&fake, ISSUE, OTHER, &["--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"issue": ISSUE, "other": OTHER, "type": "related", "created": true})
    );
    assert_eq!(fake.mutations(), 1);
    assert_eq!(fake.state().relations[2], relation("i-1", "i-2", "related"));
}

#[test]
fn relate_writes_nothing_when_any_relation_links_the_two_either_way() {
    for (existing, note) in [
        (
            relation("i-1", "i-2", "related"),
            "a `related` relation already links ABC-123 to ABC-124",
        ),
        (
            relation("i-2", "i-1", "related"),
            "a `related` relation already links ABC-124 to ABC-123",
        ),
        (
            relation("i-2", "i-1", "blocks"),
            "a `blocks` relation already links ABC-124 to ABC-123; nothing written; no `related` relation added",
        ),
    ] {
        let fake = Fake::start(&[KEY]);
        // The match is on a later page of both sides.
        fake.state().relations = vec![
            relation("i-1", "i-9", "similar"),
            relation("i-9", "i-1", "similar"),
            existing.clone(),
        ];
        let output = relate(&fake, ISSUE, OTHER, &[]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert_eq!(stdout(&output).trim(), format!("{ISSUE} {OTHER}"));
        assert!(stderr(&output).contains(note), "{}", stderr(&output));
        assert_eq!(fake.mutations(), 0, "{existing:?}");
    }
}

#[test]
fn relate_refuses_the_same_issue_twice() {
    // As text, ignoring case: before any request.
    let fake = Fake::start(&[KEY]);
    let output = relate(&fake, ISSUE, "abc-123", &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("two different issues"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.state().requests.len(), 0);

    // An id and an identifier of one issue: after the lookups.
    let output = relate(&fake, ISSUE, "i-1", &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("are the same issue"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);
}

#[test]
fn relate_with_a_missing_issue_writes_nothing() {
    for (issue, other) in [("ABC-999", OTHER), (ISSUE, "ABC-999")] {
        let fake = Fake::start(&[KEY]);
        let output = relate(&fake, issue, other, &[]);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            stderr(&output).contains("no issue ABC-999"),
            "{}",
            stderr(&output)
        );
        assert_eq!(fake.mutations(), 0);
    }
}

/// Release with `extra`, the state update failing once: exit 1 with the
/// comment written. Returns the comment count.
fn interrupted_release(fake: &Fake, extra: &[&str]) -> usize {
    fake.state().failing_state_updates = 1;
    let output = release(fake, extra);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(fake.state().issue_state, "In Progress");
    fake.comment_bodies().len()
}

#[test]
fn an_interrupted_release_is_resumed_without_a_second_comment() {
    for (extra, end) in [
        (&["--reason", "blocked"][..], "Todo"),
        (&["--reason", "merged", "--done"][..], "Done"),
        (&["--reason", "superseded", "--abandon"][..], "Abandoned"),
    ] {
        let fake = abandon_fake(Some(ABANDONED_CONFIG));
        fake.state()
            .add_comment("claim: agent-a thread-1\nscope: repo: src/\nfrom: Todo");
        let written = interrupted_release(&fake, extra);
        assert_eq!(written, 2);
        let updates = fake.state_updates();
        let output = release(&fake, extra);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(stderr(&output).contains("resuming"), "{}", stderr(&output));
        assert_eq!(fake.comment_bodies().len(), written);
        assert_eq!(fake.state_updates(), updates + 1);
        assert_eq!(fake.state().issue_state, end);
    }
}

#[test]
fn an_interrupted_forced_release_is_resumed_only_by_the_forcing_agent() {
    let fake = abandon_fake(None);
    fake.state()
        .add_comment("claim: agent-b thread-2\nscope: repo: src/\nfrom: Todo");
    let written = interrupted_release(&fake, &["--force", "stale"]);
    assert_eq!(
        fake.comment_bodies()[1],
        "release: agent-b forced by agent-a: stale"
    );
    for (agent, why) in [("agent-b", "--reason"), ("agent-c", "--force")] {
        let args = ["release", ISSUE, "--agent", agent, why, "stale"];
        let output = linear(&fake, &args);
        assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    }
    let updates = fake.state_updates();
    let output = release(&fake, &["--force", "stale"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fake.comment_bodies().len(), written);
    assert_eq!(fake.state_updates(), updates + 1);
    assert_eq!(fake.state().issue_state, "Todo");
}

#[test]
fn release_without_a_holder_is_not_a_resume_unless_its_own_release_is_latest_and_started() {
    // Another agent's release is the latest.
    let another = [
        "claim: agent-b thread-2\nfrom: Todo",
        "release: agent-b merged",
    ];
    // The issue left the started state after the release.
    let moved_on = [
        "claim: agent-a thread-1\nfrom: Todo",
        "release: agent-a merged",
    ];
    // A later claim by another agent: agent-b holds the issue.
    let claimed = [
        "claim: agent-a thread-1\nfrom: Todo",
        "release: agent-a merged",
        "claim: agent-b thread-2\nfrom: In Progress",
    ];
    for (bodies, state, says) in [
        (&another[..], "In Progress", "no holder"),
        (&moved_on[..], "Done", "no holder"),
        (&claimed[..], "In Progress", "held by agent-b"),
    ] {
        let fake = Fake::start(&[KEY]);
        fake.state().issue_state = state.into();
        for body in bodies {
            fake.state().add_comment(body);
        }
        let output = release(&fake, &["--reason", "merged", "--done"]);
        assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
        assert!(stderr(&output).contains(says), "{}", stderr(&output));
        assert_eq!(fake.mutations(), 0);
        assert_eq!(fake.state().issue_state, state);
    }
}

fn edit(fake: &Fake, extra: &[&str]) -> Output {
    let mut args = vec!["edit", ISSUE];
    args.extend_from_slice(extra);
    linear(fake, &args)
}

/// The inputs of the edits sent so far.
fn edits(fake: &Fake) -> Vec<Value> {
    let state = fake.state();
    state
        .requests
        .iter()
        .filter(|r| r.query.starts_with("mutation"))
        .map(|r| {
            assert!(r.query.contains("issueUpdate"), "{}", r.query);
            r.variables["i"].clone()
        })
        .collect()
}

#[test]
fn edit_sends_only_the_fields_given_and_the_description_verbatim() {
    let fake = Fake::start(&[KEY]);
    let output = edit(&fake, &["--title", " New title "]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{ISSUE} {ISSUE_URL}\n"));
    assert_eq!(edits(&fake), [json!({"title": " New title "})]);

    let fake = Fake::start(&[KEY]);
    let body = "  Status: open\n\nclaim: on the first line needs nothing special\n\n";
    let file = fake.home.join("description.md");
    fs::write(&file, body).unwrap();
    let file = file.to_str().unwrap();
    let output = edit(&fake, &["--description-file", file, "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"identifier": ISSUE, "url": ISSUE_URL, "changed": true})
    );
    assert_eq!(edits(&fake), [json!({"description": body})]);
    assert_eq!(fake.state().issue_title, "Old title");

    let fake = Fake::start(&[KEY]);
    let output = edit(&fake, &["--title", "T", "--description-file", file]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(edits(&fake), [json!({"title": "T", "description": body})]);
}

#[test]
fn edit_with_the_current_values_writes_nothing() {
    let fake = Fake::start(&[KEY]);
    fake.state().issue_description = Some("Body\n".into());
    let file = fake.home.join("description.md");
    fs::write(&file, "Body\n").unwrap();
    let file = file.to_str().unwrap();
    for extra in [
        &["--title", "Old title"][..],
        &["--description-file", file],
        &["--title", "Old title", "--description-file", file, "--json"],
    ] {
        let output = edit(&fake, extra);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("nothing written"),
            "{}",
            stderr(&output)
        );
    }
    let output = edit(&fake, &["--title", "Old title", "--json"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"identifier": ISSUE, "url": ISSUE_URL, "changed": false})
    );
    assert_eq!(fake.mutations(), 0);

    // Compared exactly: one more newline is a change.
    fs::write(file, "Body\n\n").unwrap();
    let output = edit(&fake, &["--title", "Old title", "--description-file", file]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        edits(&fake),
        [json!({"title": "Old title", "description": "Body\n\n"})]
    );
}

#[test]
fn edit_of_a_missing_issue_writes_nothing() {
    let fake = Fake::start(&[KEY]);
    let output = linear(&fake, &["edit", "ABC-404", "--title", "T"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("no issue ABC-404"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fake.mutations(), 0);
}

#[test]
fn edit_refuses_no_change_or_an_empty_title_or_description_before_any_request() {
    let fake = Fake::start(&[KEY]);
    let output = edit(&fake, &[]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let file = fake.home.join("description.md");
    let path = file.to_str().unwrap();
    for (title, description) in [(" \t", "Body"), ("T", ""), ("T", " \n\t\n")] {
        fs::write(&file, description).unwrap();
        let output = edit(&fake, &["--title", title, "--description-file", path]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{title:?} {description:?}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("nothing sent"),
            "{}",
            stderr(&output)
        );
    }
    let missing = fake.home.join("missing.md");
    let output = edit(&fake, &["--description-file", missing.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(fake.state().requests.is_empty());

    // Refused before the key is read: the key command never runs.
    let cmd = KeyCommand::new("key-edit-refused", KEY);
    let output = cmd.run(&fake, &["linear", "edit", ISSUE, "--title", " "]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(cmd.runs(), 0);
}
