use std::io::Read as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use kanso::auth::AuthConfig;
use kanso::db::Database;
use kanso::server::{router, AppState};
use serde_json::{json, Value};
use tokio::net::TcpListener;

const PASSWORD: &str = "cli-test-password";
static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        for _ in 0..100 {
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("kanso-cli-{}-{sequence}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create temporary directory {}: {error}", path.display()),
            }
        }
        panic!("could not create a unique temporary directory");
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct TestServer {
    _dir: TempDir,
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn start() -> Self {
        let dir = TempDir::new();
        let database = Database::open(&dir.path().join("kanso.db")).expect("open database");
        let auth = AuthConfig::load(PASSWORD, None, dir.path()).expect("auth config");
        let state = AppState::new(database, dir.path().join("web"), auth);

        let (address_tx, address_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let handle = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(async move {
                let listener = TcpListener::bind(("127.0.0.1", 0))
                    .await
                    .expect("bind test server");
                let addr = listener.local_addr().expect("local address");
                let _ = address_tx.send(addr);
                axum::serve(listener, router(state).into_make_service())
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("serve");
            });
        });
        let addr = address_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server address");
        Self {
            _dir: dir,
            addr,
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn config_path(&self) -> PathBuf {
        self._dir.path().join("config.toml")
    }

    fn data_dir(&self) -> &Path {
        self._dir.path()
    }

    fn login_cookie(&self) -> String {
        let response = api_agent()
            .post(format!("{}/api/auth/login", self.url()))
            .header("Content-Type", "application/json")
            .send(json!({"password": PASSWORD}).to_string())
            .expect("login");
        assert_eq!(response.status().as_u16(), 204);
        let cookie = response
            .headers()
            .get("set-cookie")
            .expect("set-cookie")
            .to_str()
            .expect("cookie")
            .to_owned();
        cookie.split(';').next().expect("cookie pair").to_owned()
    }

    fn create_token(&self, name: &str, scope: &str) -> String {
        let cookie = self.login_cookie();
        let response = api_agent()
            .post(format!("{}/api/tokens", self.url()))
            .header("Cookie", &cookie)
            .header("Origin", self.url())
            .header("Content-Type", "application/json")
            .send(json!({"name": name, "scope": scope}).to_string())
            .expect("create token");
        assert_eq!(response.status().as_u16(), 201);
        let mut response = response;
        let body = response.body_mut().read_to_string().expect("token body");
        let value: Value = serde_json::from_str(&body).expect("token JSON");
        value["token"].as_str().expect("token").to_owned()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn api_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn api_get(server: &TestServer, path: &str, token: &str) -> String {
    let mut response = api_agent()
        .get(format!("{}{path}", server.url()))
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .expect("api get");
    assert_eq!(response.status().as_u16(), 200, "{path}");
    response.body_mut().read_to_string().expect("api body")
}

fn kanso_command(server: &TestServer, token: Option<&str>, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kanso"));
    command
        .args(args)
        .env_remove("KANSO_URL")
        .env_remove("KANSO_TOKEN")
        .env("KANSO_CONFIG", server.config_path())
        .env("HOME", server.data_dir())
        .env("USERPROFILE", server.data_dir());
    if let Some(token) = token {
        command
            .env("KANSO_URL", server.url())
            .env("KANSO_TOKEN", token);
    }
    command
}

fn command(server: &TestServer, token: Option<&str>, args: &[&str]) -> Output {
    kanso_command(server, token, args)
        .output()
        .expect("run kanso")
}

fn api_post(server: &TestServer, token: &str, path: &str, body: Value) -> Value {
    let mut response = api_agent()
        .post(format!("{}{path}", server.url()))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .expect("api post");
    assert_eq!(response.status().as_u16(), 201, "{path}");
    let text = response.body_mut().read_to_string().expect("api body");
    serde_json::from_str(&text).expect("api JSON")
}

fn assert_broken_pipe_exits_zero(server: &TestServer, token: &str, args: &[&str]) {
    let mut child = kanso_command(server, Some(token), args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kanso");
    let mut stdout = child.stdout.take().expect("child stdout");
    let mut prefix = [0_u8; 32];
    stdout
        .read_exact(&mut prefix)
        .expect("read the start of the output");
    drop(stdout);

    let output = child.wait_with_output().expect("wait for kanso");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn non_broken_stdout_errors_exit_two() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");
    api_post(
        &server,
        &human,
        "/api/projects",
        json!({"key": "ALP", "name": "Alpha"}),
    );

    let full = std::fs::File::create("/dev/full").expect("/dev/full");
    let output = kanso_command(&server, Some(&human), &["project", "list"])
        .stdout(Stdio::from(full))
        .output()
        .expect("run kanso");
    assert_exit(&output, 2);
    assert!(
        stderr(&output).contains("failed to write stdout"),
        "{}",
        stderr(&output)
    );
}

fn config_init(server: &TestServer, token: &str, force: bool) -> Output {
    let url = server.url();
    let mut args = vec!["config", "init", "--url", url.as_str(), "--token", token];
    if force {
        args.push("--force");
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_kanso"));
    command
        .args(args)
        .env_remove("KANSO_URL")
        .env_remove("KANSO_TOKEN")
        .env("KANSO_CONFIG", server.config_path())
        .env("HOME", server.data_dir())
        .env("USERPROFILE", server.data_dir());
    command.output().expect("run kanso config init")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_str(&stdout(output)).expect("stdout is JSON")
}

fn create_project_and_card(server: &TestServer, human: &str) -> Value {
    let output = command(
        server,
        Some(human),
        &["project", "add", "--key", "ALP", "--name", "Alpha"],
    );
    assert_exit(&output, 0);
    let output = command(
        server,
        Some(human),
        &["task", "add", "First task", "--project", "ALP"],
    );
    assert_exit(&output, 0);
    let output = command(server, Some(human), &["task", "show", "ALP-1", "--json"]);
    assert_exit(&output, 0);
    json_stdout(&output)
}

#[test]
fn config_init_writes_0600_validates_and_refuses_to_clobber() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");

    let invalid = config_init(&server, "kanso_not_a_token", false);
    assert_exit(&invalid, 2);
    assert!(
        stderr(&invalid).contains("unauthorized"),
        "{}",
        stderr(&invalid)
    );
    assert!(!server.config_path().exists());

    let created = config_init(&server, &human, false);
    assert_exit(&created, 0);
    assert!(stdout(&created).contains("authenticated as human-cli (human)"));
    assert!(stdout(&created).contains("wrote"));
    assert!(server.config_path().is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(server.config_path())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    let content = std::fs::read_to_string(server.config_path()).expect("read config");
    assert!(content.contains(&human), "{content}");

    let refused = config_init(&server, &human, false);
    assert_exit(&refused, 1);
    assert!(stderr(&refused).contains("--force"), "{}", stderr(&refused));

    let replacement = server.create_token("human-two", "human");
    let forced = config_init(&server, &replacement, true);
    assert_exit(&forced, 0);
    let content = std::fs::read_to_string(server.config_path()).expect("read config");
    assert!(content.contains(&replacement), "{content}");
    assert!(!content.contains(&human), "{content}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(server.config_path())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn the_agent_loop_works_end_to_end() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");
    let agent = server.create_token("agent", "agent");
    create_project_and_card(&server, &human);

    let next = command(
        &server,
        Some(&agent),
        &["next", "--project", "ALP", "--claim", "--agent", "reviewer"],
    );
    assert_exit(&next, 0);
    assert!(stdout(&next).contains("ALP-1"), "{}", stdout(&next));
    assert!(stdout(&next).contains("in_progress"));

    let comment = command(
        &server,
        Some(&agent),
        &["comment", "ALP-1", "started the work"],
    );
    assert_exit(&comment, 0);

    let added = command(
        &server,
        Some(&agent),
        &["check", "ALP-1", "--add", "step one", "--agent", "reviewer"],
    );
    assert_exit(&added, 0);
    assert!(stdout(&added).contains("step one"));

    let done = command(&server, Some(&agent), &["check", "ALP-1", "--done", "1"]);
    assert_exit(&done, 0);

    let listed = command(&server, Some(&agent), &["check", "ALP-1", "--list"]);
    assert_exit(&listed, 0);
    assert!(stdout(&listed).contains("step one"));
    assert!(stdout(&listed).contains('x'));

    let moved = command(
        &server,
        Some(&agent),
        &["move", "ALP-1", "in_review", "--agent", "reviewer"],
    );
    assert_exit(&moved, 0);
    assert!(stdout(&moved).contains("in_review"));

    let detail = command(&server, Some(&human), &["task", "show", "ALP-1", "--json"]);
    assert_exit(&detail, 0);
    let detail = json_stdout(&detail);
    assert_eq!(detail["status"], "in_review");
    assert_eq!(detail["claimed_by"], "agent");
    assert_eq!(detail["checklist"][0]["done"], true);
    assert!(detail["comments"]
        .as_array()
        .expect("comments")
        .iter()
        .any(|comment| comment["body"] == "started the work"));
    for kind in ["claimed", "commented", "checklist_added", "moved"] {
        assert!(
            detail["activity"]
                .as_array()
                .expect("activity")
                .iter()
                .any(|entry| entry["kind"] == kind),
            "{kind}"
        );
    }
    for kind in ["claimed", "checklist_added", "moved"] {
        assert!(
            detail["activity"]
                .as_array()
                .expect("activity")
                .iter()
                .any(|entry| entry["kind"] == kind && entry["actor"] == "reviewer"),
            "{kind} attribution"
        );
    }

    let empty = command(&server, Some(&agent), &["next", "--project", "ALP"]);
    assert_exit(&empty, 0);
    assert!(stdout(&empty).contains("no cards ready"));
}

#[test]
fn api_failures_map_to_exit_codes_two_and_three() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");
    let agent = server.create_token("agent", "agent");
    let second_agent = server.create_token("agent-two", "agent");
    create_project_and_card(&server, &human);

    let unauthorized = command(&server, Some("kanso_invalid"), &["task", "list"]);
    assert_exit(&unauthorized, 2);
    assert!(
        stderr(&unauthorized).contains("unauthorized"),
        "{}",
        stderr(&unauthorized)
    );

    let forbidden = command(
        &server,
        Some(&agent),
        &["task", "edit", "ALP-1", "--title", "nope"],
    );
    assert_exit(&forbidden, 2);
    assert!(
        stderr(&forbidden).contains("forbidden"),
        "{}",
        stderr(&forbidden)
    );

    let not_found = command(&server, Some(&human), &["task", "show", "ALP-999"]);
    assert_exit(&not_found, 2);
    assert!(stderr(&not_found).contains("not_found"));

    let not_found_json = command(
        &server,
        Some(&human),
        &["task", "show", "ALP-999", "--json"],
    );
    assert_exit(&not_found_json, 2);
    assert!(stderr(&not_found_json).contains("not_found"));
    assert!(stdout(&not_found_json).is_empty());

    let validation = command(
        &server,
        Some(&human),
        &[
            "task",
            "add",
            "Bad",
            "--project",
            "ALP",
            "--status",
            "doing",
        ],
    );
    assert_exit(&validation, 2);
    assert!(stderr(&validation).contains("validation"));

    let usage = command(&server, Some(&human), &["frobnicate"]);
    assert_exit(&usage, 1);
    assert!(!stderr(&usage).is_empty());
    assert!(stdout(&usage).is_empty());

    let zero = command(&server, Some(&human), &["check", "ALP-1", "--done", "0"]);
    assert_exit(&zero, 1);
    assert!(stderr(&zero).contains("1-based"), "{}", stderr(&zero));

    let claimed = command(&server, Some(&agent), &["claim", "ALP-1"]);
    assert_exit(&claimed, 0);
    let already = command(&server, Some(&second_agent), &["claim", "ALP-1"]);
    assert_exit(&already, 3);
    assert!(
        stderr(&already).contains("already_claimed"),
        "{}",
        stderr(&already)
    );

    let dependency = command(
        &server,
        Some(&human),
        &[
            "task",
            "add",
            "Dependent",
            "--project",
            "ALP",
            "--status",
            "todo",
        ],
    );
    assert_exit(&dependency, 0);
    let depend = command(&server, Some(&human), &["depend", "ALP-2", "on", "ALP-1"]);
    assert_exit(&depend, 0);
    let unmet = command(&server, Some(&agent), &["claim", "ALP-2"]);
    assert_exit(&unmet, 3);
    assert!(
        stderr(&unmet).contains("dependency_unmet"),
        "{}",
        stderr(&unmet)
    );
}

#[test]
fn json_output_matches_the_api_payload() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");
    create_project_and_card(&server, &human);

    let cli = command(&server, Some(&human), &["task", "list", "--json"]);
    assert_exit(&cli, 0);
    let api_body = api_get(&server, "/api/tasks", &human);
    assert_eq!(stdout(&cli), format!("{api_body}\n"));

    let cli = command(&server, Some(&human), &["task", "show", "ALP-1", "--json"]);
    assert_exit(&cli, 0);
    let api_body = api_get(&server, "/api/tasks/ALP-1", &human);
    assert_eq!(stdout(&cli), format!("{api_body}\n"));
}

#[test]
fn agent_attribution_session_links_and_proposals() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");
    let agent = server.create_token("agent", "agent");
    create_project_and_card(&server, &human);

    let claim = command(
        &server,
        Some(&agent),
        &["claim", "ALP-1", "--agent", "reviewer"],
    );
    assert_exit(&claim, 0);
    let comment = command(
        &server,
        Some(&agent),
        &["comment", "ALP-1", "from reviewer", "--agent", "reviewer"],
    );
    assert_exit(&comment, 0);
    let proposed = command(
        &server,
        Some(&agent),
        &[
            "propose",
            "An idea",
            "--project",
            "ALP",
            "--description",
            "details",
            "--agent",
            "planner",
        ],
    );
    assert_exit(&proposed, 0);
    assert!(stdout(&proposed).contains("proposed"));

    let linked = command(
        &server,
        Some(&human),
        &[
            "session",
            "link",
            "ALP-1",
            "ses_123",
            "--directory",
            "/tmp/worktree",
        ],
    );
    assert_exit(&linked, 0);
    assert!(stdout(&linked).contains("ses_123"));

    let shown = command(&server, Some(&human), &["task", "show", "ALP-1"]);
    assert_exit(&shown, 0);
    assert!(
        stdout(&shown).contains("session: ses_123  /tmp/worktree  (running)"),
        "{}",
        stdout(&shown)
    );

    let detail = command(&server, Some(&human), &["task", "show", "ALP-1", "--json"]);
    assert_exit(&detail, 0);
    let detail = json_stdout(&detail);
    assert_eq!(detail["claimed_by"], "agent");
    assert!(detail["activity"]
        .as_array()
        .expect("activity")
        .iter()
        .any(|entry| entry["kind"] == "claimed" && entry["actor"] == "reviewer"));
    assert_eq!(detail["session_id"], "ses_123");
    assert_eq!(detail["session_directory"], "/tmp/worktree");
    assert_eq!(detail["session_status"], "running");
    assert_eq!(detail["comments"][0]["author"], "reviewer");

    let inbox = command(&server, Some(&human), &["inbox", "--json"]);
    assert_exit(&inbox, 0);
    let inbox = json_stdout(&inbox);
    assert_eq!(inbox[0]["created_by"], "planner");
    assert_eq!(inbox[0]["state"], "proposed");

    let denied = command(
        &server,
        Some(&agent),
        &["session", "link", "ALP-1", "ses_2", "--directory", "/tmp"],
    );
    assert_exit(&denied, 2);
    assert!(stderr(&denied).contains("forbidden"));
}

#[test]
fn blocker_reason_round_trips_and_rejects_a_reason_elsewhere() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");
    let agent = server.create_token("agent", "agent");
    create_project_and_card(&server, &human);
    assert_exit(&command(&server, Some(&agent), &["claim", "ALP-1"]), 0);

    let usage = command(
        &server,
        Some(&agent),
        &["move", "ALP-1", "in_review", "--reason", "not a blocker"],
    );
    assert_exit(&usage, 1);

    let blocked = command(
        &server,
        Some(&agent),
        &["move", "ALP-1", "blocker", "--reason", "waiting on a key"],
    );
    assert_exit(&blocked, 0);
    assert!(stdout(&blocked).contains("waiting on a key"));

    let detail = command(&server, Some(&human), &["task", "show", "ALP-1", "--json"]);
    assert_exit(&detail, 0);
    let detail = json_stdout(&detail);
    assert_eq!(detail["status"], "blocker");
    assert_eq!(detail["blocked_reason"], "waiting on a key");
    assert_eq!(detail["blocked_from"], "in_progress");

    let restored = command(&server, Some(&agent), &["move", "ALP-1", "restore"]);
    assert_exit(&restored, 0);
    let detail = command(&server, Some(&human), &["task", "show", "ALP-1", "--json"]);
    assert_exit(&detail, 0);
    let detail = json_stdout(&detail);
    assert_eq!(detail["status"], "in_progress");
    assert!(detail["blocked_reason"].is_null());
}

#[test]
fn backup_skill_and_tokens_work_from_the_cli() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");
    let agent = server.create_token("agent", "agent");

    let backup = command(&server, Some(&human), &["backup", "--json"]);
    assert_exit(&backup, 0);
    let backup = json_stdout(&backup);
    let path = backup["path"].as_str().expect("backup path");
    assert!(
        path.contains("backups") && path.contains("kanso-"),
        "{path}"
    );
    assert!(Path::new(path).is_file(), "{path}");
    assert!(backup["size"].as_u64().unwrap_or_default() > 0);

    let backup = command(
        &server,
        Some(&human),
        &["backup", "--out", "nested", "--json"],
    );
    assert_exit(&backup, 0);
    let backup = json_stdout(&backup);
    assert!(backup["path"].as_str().expect("path").contains("nested"));

    let escape = command(&server, Some(&human), &["backup", "--out", "../escape"]);
    assert_exit(&escape, 2);
    assert!(stderr(&escape).contains("validation"));

    let denied = command(&server, Some(&agent), &["backup"]);
    assert_exit(&denied, 2);
    assert!(stderr(&denied).contains("forbidden"));

    let printed = command(&server, None, &["skill", "--print"]);
    assert_exit(&printed, 0);
    assert_eq!(stdout(&printed), kanso::skill::SKILL);

    let install_dir = server.data_dir().join("installed-skills");
    let installed = command(
        &server,
        None,
        &[
            "skill",
            "--install",
            "--dir",
            install_dir.to_str().expect("utf-8 path"),
        ],
    );
    assert_exit(&installed, 0);
    assert!(
        stdout(&installed).starts_with("installed:"),
        "{}",
        stdout(&installed)
    );
    let skill_path = install_dir.join("kanso").join("SKILL.md");
    assert_eq!(
        std::fs::read_to_string(&skill_path).expect("skill file"),
        kanso::skill::SKILL
    );
    let again = command(
        &server,
        None,
        &[
            "skill",
            "--install",
            "--dir",
            install_dir.to_str().expect("utf-8 path"),
        ],
    );
    assert_exit(&again, 0);
    assert!(stdout(&again).contains("up to date"));

    let bad = command(
        &server,
        None,
        &["skill", "--dir", install_dir.to_str().expect("utf-8 path")],
    );
    assert_exit(&bad, 1);

    let conflict = command(&server, None, &["skill", "--print", "--install"]);
    assert_exit(&conflict, 1);

    let created = command(
        &server,
        Some(&human),
        &["token", "create", "--name", "build", "--scope", "agent"],
    );
    assert_exit(&created, 0);
    assert!(stdout(&created).contains("created token \"build\" (agent)"));
    let token = stdout(&created)
        .lines()
        .last()
        .expect("token line")
        .trim()
        .to_owned();
    assert!(token.starts_with("kanso_"), "{token}");

    let list = command(&server, Some(&human), &["token", "list"]);
    assert_exit(&list, 0);
    assert!(stdout(&list).contains("build"));
    assert!(stdout(&list).contains("agent"));

    let revoked = command(&server, Some(&human), &["token", "revoke", "build"]);
    assert_exit(&revoked, 0);
    assert!(stdout(&revoked).contains("revoked token \"build\""));

    let missing = command(&server, Some(&human), &["token", "revoke", "nope"]);
    assert_exit(&missing, 2);
    assert!(stderr(&missing).contains("not_found"));
}

#[test]
fn broken_pipe_stops_quietly_with_exit_zero() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");
    api_post(
        &server,
        &human,
        "/api/projects",
        json!({"key": "ALP", "name": "Alpha"}),
    );
    let labels: Vec<String> = (0..30)
        .map(|index| format!("label-{index:02}-{}", "x".repeat(22)))
        .collect();
    for label in &labels {
        api_post(
            &server,
            &human,
            "/api/labels",
            json!({"name": label, "color": "#123456"}),
        );
    }
    for index in 0..100 {
        api_post(
            &server,
            &human,
            "/api/tasks",
            json!({
                "project_key": "ALP",
                "title": format!("card-{index:03}-{}", "y".repeat(185)),
                "labels": labels,
            }),
        );
    }

    // Both a wide table and the much larger JSON list exceed a pipe buffer.
    assert_broken_pipe_exits_zero(&server, &human, &["task", "list"]);
    assert_broken_pipe_exits_zero(&server, &human, &["task", "list", "--json"]);
}

#[test]
fn world_readable_config_is_repaired_on_load() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");
    assert_exit(&config_init(&server, &human, false), 0);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(server.config_path(), std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");

        let output = command(&server, None, &["project", "list"]);
        assert_exit(&output, 0);
        assert!(
            stderr(&output).contains("was readable by other users; set to 0600"),
            "{}",
            stderr(&output)
        );
        let mode = std::fs::metadata(server.config_path())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn complete_environment_skips_a_broken_config_file() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");
    std::fs::write(server.config_path(), "this is not toml = [").expect("write broken config");

    let output = command(&server, Some(&human), &["project", "list"]);
    assert_exit(&output, 0);
    assert!(stdout(&output).contains("KEY"));
}

#[test]
fn project_commands_and_task_edit_work() {
    let server = TestServer::start();
    let human = server.create_token("human-cli", "human");

    let add = command(
        &server,
        Some(&human),
        &[
            "project",
            "add",
            "--key",
            "ALP",
            "--name",
            "Alpha",
            "--description",
            "first",
        ],
    );
    assert_exit(&add, 0);
    let list = command(&server, Some(&human), &["project", "list"]);
    assert_exit(&list, 0);
    assert!(stdout(&list).contains("ALP"));
    assert!(stdout(&list).contains("Alpha"));

    let show = command(&server, Some(&human), &["project", "show", "ALP", "--json"]);
    assert_exit(&show, 0);
    assert_eq!(json_stdout(&show)["description"], "first");

    let edit = command(
        &server,
        Some(&human),
        &["project", "edit", "ALP", "--name", "Renamed"],
    );
    assert_exit(&edit, 0);
    let show = command(&server, Some(&human), &["project", "show", "ALP", "--json"]);
    assert_exit(&show, 0);
    assert_eq!(json_stdout(&show)["name"], "Renamed");

    let task = command(
        &server,
        Some(&human),
        &[
            "task",
            "add",
            "Editable",
            "--project",
            "ALP",
            "--status",
            "todo",
            "--priority",
            "high",
        ],
    );
    assert_exit(&task, 0);
    let edit = command(
        &server,
        Some(&human),
        &[
            "task",
            "edit",
            "ALP-1",
            "--title",
            "Edited",
            "--description",
            "body",
            "--due",
            "2026-12-01",
        ],
    );
    assert_exit(&edit, 0);
    let detail = command(&server, Some(&human), &["task", "show", "ALP-1", "--json"]);
    assert_exit(&detail, 0);
    let detail = json_stdout(&detail);
    assert_eq!(detail["title"], "Edited");
    assert_eq!(detail["description"], "body");
    assert_eq!(detail["due_date"], "2026-12-01");

    let ansi = command(
        &server,
        Some(&human),
        &["task", "add", "bad\u{1b}[31mtitle", "--project", "ALP"],
    );
    assert_exit(&ansi, 0);

    assert_exit(&command(&server, Some(&human), &["claim", "ALP-1"]), 0);
    let mine = command(&server, Some(&human), &["task", "list", "--mine"]);
    assert_exit(&mine, 0);
    assert!(stdout(&mine).contains("ALP-1"));
    assert!(!stdout(&mine).contains("ALP-2"), "{}", stdout(&mine));

    let shown = command(&server, Some(&human), &["task", "show", "ALP-2"]);
    assert_exit(&shown, 0);
    assert!(!stdout(&shown).contains('\u{1b}'), "{:?}", stdout(&shown));
}
