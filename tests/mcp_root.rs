//! How the MCP server finds its project root, and that its start-up overview
//! describes that root, checked on the built binary.
//!
//! The server resolves its root once at startup, from `--scope` or the
//! directory it was launched in, and again when the client answers
//! roots/list. Each test launches the binary in a directory of its own, so
//! the launch directory is a child process's and never this process's. The
//! cases without any root are in `dropped_errors.rs`.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// What one server run printed: its responses.
struct Run {
    responses: Vec<Value>,
}

impl Run {
    /// The text of the tool result with `id`.
    #[expect(
        clippy::expect_used,
        reason = "by design: a test helper, where a panic is the test failure"
    )]
    fn text(&self, id: u64) -> &str {
        self.result(id)["content"][0]["text"]
            .as_str()
            .expect("a tool result with this id")
    }

    /// The instructions the `initialize` answer (id 0) gave.
    #[expect(
        clippy::expect_used,
        reason = "by design: a test helper, where a panic is the test failure"
    )]
    fn instructions(&self) -> &str {
        self.result(0)["instructions"]
            .as_str()
            .expect("an initialize result")
    }

    /// The result of the response with `id`, or `null` when there is none.
    fn result(&self, id: u64) -> &Value {
        self.responses
            .iter()
            .find(|r| r["id"] == id)
            .map_or(&Value::Null, |r| &r["result"])
    }
}

/// Run `tilth --mcp <flags>` from `launch`, send it `messages`, and close its
/// stdin. The start-up overview is on only when `overview` is set.
#[expect(
    clippy::expect_used,
    reason = "by design: a test helper, where a panic is the test failure"
)]
fn serve(launch: &Path, flags: &[&str], overview: bool, messages: &[Value]) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tilth"));
    if overview {
        command.env_remove("TILTH_NO_OVERVIEW");
    } else {
        command.env("TILTH_NO_OVERVIEW", "1");
    }
    let mut child = command
        .arg("--mcp")
        .args(flags)
        .current_dir(launch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start tilth");
    let mut stdin = child.stdin.take().expect("stdin");
    for message in messages {
        writeln!(stdin, "{message}").expect("write a message");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("run tilth");
    assert!(out.status.success(), "tilth exited with {:?}", out.status);
    Run {
        responses: String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON-RPC line"))
            .collect(),
    }
}

fn initialize(roots: bool) -> Value {
    let capabilities = if roots {
        json!({ "roots": {} })
    } else {
        json!({})
    };
    json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "capabilities": capabilities },
    })
}

/// The client's answer to the server's roots/list request.
fn roots_answer(root: &Path) -> Value {
    json!({
        "jsonrpc": "2.0", "id": "tilth_roots_1",
        "result": { "roots": [{ "uri": format!("file://{}", root.to_string_lossy()) }] },
    })
}

fn call(id: u64, tool: &str, arguments: &Value) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    })
}

/// A directory holding one source file that defines `name`.
#[expect(
    clippy::unwrap_used,
    reason = "by design: a test helper, where a panic is the test failure"
)]
fn defining(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lib.rs"), format!("fn {name}() {{}}\n")).unwrap();
    dir
}

/// A server launched in one directory, whose client names another as its
/// root, answers for the client's root: a search with no scope, and a
/// relative write with no `root`.
#[test]
fn the_clients_root_replaces_the_launch_directory() {
    let launch = defining("in_launch_zq");
    let client = defining("in_client_zq");
    let run = serve(
        launch.path(),
        &["--edit"],
        false,
        &[
            initialize(true),
            roots_answer(client.path()),
            call(1, "tilth_search", &json!({ "query": "in_client_zq" })),
            call(2, "tilth_search", &json!({ "query": "in_launch_zq" })),
            call(
                3,
                "tilth_write",
                &json!({ "files": [{ "path": "new.txt", "mode": "overwrite", "content": "x\n" }] }),
            ),
        ],
    );
    assert!(run.text(1).contains("lib.rs:1"), "{}", run.text(1));
    assert!(run.text(2).contains("0 matches"), "{}", run.text(2));
    assert!(run.text(3).contains("created"), "{}", run.text(3));
    assert!(client.path().join("new.txt").exists());
    assert!(!launch.path().join("new.txt").exists());
}

/// `--scope` outranks the client's roots.
#[test]
fn an_explicit_scope_outranks_the_clients_root() {
    let launch = tempfile::tempdir().unwrap();
    let scope = defining("in_scope_zq");
    let client = defining("in_client_zq");
    let run = serve(
        launch.path(),
        &["--scope", scope.path().to_str().unwrap()],
        false,
        &[
            initialize(true),
            roots_answer(client.path()),
            call(1, "tilth_search", &json!({ "query": "in_scope_zq" })),
        ],
    );
    assert!(run.text(1).contains("lib.rs:1"), "{}", run.text(1));
}

// -- The start-up overview (#384) ---------------------------------------------

/// How every overview starts.
const OVERVIEW: &str = "[tilth] ";

/// A directory holding one Python file, so its overview reads "Python
/// project" where a `defining` directory's reads "Rust project".
#[expect(
    clippy::unwrap_used,
    reason = "by design: a test helper, where a panic is the test failure"
)]
fn python_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.py"), "def main():\n    pass\n").unwrap();
    dir
}

/// A client with roots names its root only after `initialize`, so an
/// overview there would describe the launch directory, which the tools stop
/// answering from. The instructions carry none, and the first tool result
/// after the roots answer opens with the client root's, once.
#[test]
fn with_client_roots_the_overview_describes_the_clients_root() {
    let launch = python_project();
    let client = defining("in_client_zq");
    let search = json!({ "query": "in_client_zq" });
    let run = serve(
        launch.path(),
        &[],
        true,
        &[
            initialize(true),
            roots_answer(client.path()),
            call(1, "tilth_search", &search),
            call(2, "tilth_search", &search),
        ],
    );
    assert!(
        !run.instructions().contains(OVERVIEW),
        "{}",
        run.instructions()
    );
    assert!(
        run.text(1).starts_with("[tilth] Rust project") && run.text(1).contains("lib.rs:1"),
        "{}",
        run.text(1)
    );
    assert!(!run.text(2).contains(OVERVIEW), "{}", run.text(2));
}

/// A call sent before the roots answer runs against the launch directory,
/// whose overview was never given, so it gets none. The first call after the
/// answer gets the client root's.
#[test]
fn a_call_before_the_roots_answer_gets_no_overview() {
    let launch = python_project();
    let client = defining("in_client_zq");
    let search = json!({ "query": "in_client_zq" });
    let run = serve(
        launch.path(),
        &[],
        true,
        &[
            initialize(true),
            call(1, "tilth_search", &search),
            roots_answer(client.path()),
            call(2, "tilth_search", &search),
        ],
    );
    assert!(
        !run.text(1).contains(OVERVIEW) && run.text(1).contains("0 matches"),
        "{}",
        run.text(1)
    );
    assert!(
        run.text(2).starts_with("[tilth] Rust project"),
        "{}",
        run.text(2)
    );
}

/// With `--scope` the root is known at `initialize`, so the overview stays
/// in the instructions and no tool result repeats it.
#[test]
fn with_a_scope_the_overview_stays_in_the_instructions() {
    let launch = python_project();
    let scope = defining("in_scope_zq");
    let run = serve(
        launch.path(),
        &["--scope", scope.path().to_str().unwrap()],
        true,
        &[
            initialize(true),
            call(1, "tilth_search", &json!({ "query": "in_scope_zq" })),
        ],
    );
    assert!(
        run.instructions().starts_with("[tilth] Rust project"),
        "{}",
        run.instructions()
    );
    assert!(!run.text(1).contains(OVERVIEW), "{}", run.text(1));
}

/// A client without roots leaves the launch directory as the root, so the
/// overview describes it in the instructions and no tool result repeats it.
#[test]
fn without_client_roots_the_overview_stays_in_the_instructions() {
    let launch = python_project();
    let run = serve(
        launch.path(),
        &[],
        true,
        &[
            initialize(false),
            call(1, "tilth_search", &json!({ "query": "main" })),
        ],
    );
    assert!(
        run.instructions().starts_with("[tilth] Python project"),
        "{}",
        run.instructions()
    );
    assert!(!run.text(1).contains(OVERVIEW), "{}", run.text(1));
}
