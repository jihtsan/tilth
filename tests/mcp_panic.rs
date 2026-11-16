//! A panic does not end the MCP server, checked on the built binary.
//!
//! The `panic-probe` feature, which only this test enables, makes the server
//! panic on purpose in two places: a tool call whose arguments hold
//! `panic_probe`, and the start-up overview of a root named
//! `tilth-panic-probe`. Each case checks that the server answers the call
//! that panicked and then the next one.
//! CI runs them in the release profile: it is the one that ships, and it
//! sets its own panic strategy.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use serde_json::{json, Value};

/// What one server run printed, and how it ended.
struct Run {
    responses: Vec<Value>,
    status: ExitStatus,
}

impl Run {
    /// The result of the response with `id`.
    fn result(&self, id: u64) -> &Value {
        self.responses.iter().find(|r| r["id"] == id).map_or_else(
            || panic!("no response {id}; the server ended with {}", self.status),
            |r| &r["result"],
        )
    }
}

/// Run `tilth --mcp <flags>` from `launch`, send it `messages`, and close
/// its stdin. With `overview` false, the server skips the start-up overview.
#[expect(
    clippy::expect_used,
    reason = "by design: a test helper, where a panic is the test failure"
)]
fn serve(launch: &Path, flags: &[&str], overview: bool, messages: &[Value]) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tilth"));
    command
        .arg("--mcp")
        .args(flags)
        .current_dir(launch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if overview {
        command.env_remove("TILTH_NO_OVERVIEW");
    } else {
        command.env("TILTH_NO_OVERVIEW", "1");
    }
    let mut child = command.spawn().expect("start tilth");
    let mut stdin = child.stdin.take().expect("stdin");
    for message in messages {
        // A server that died early has closed the pipe. The missing
        // responses report that, more clearly than a failed write would.
        if writeln!(stdin, "{message}").is_err() {
            break;
        }
    }
    drop(stdin);
    let out = child.wait_with_output().expect("run tilth");
    Run {
        responses: String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON-RPC line"))
            .collect(),
        status: out.status,
    }
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "capabilities": {} },
    })
}

fn call(id: u64, tool: &str, arguments: &Value) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    })
}

/// `dir` with one source file that defines `probe_target_zq`.
#[expect(
    clippy::unwrap_used,
    reason = "by design: a test helper, where a panic is the test failure"
)]
fn with_source(dir: &Path) {
    std::fs::write(dir.join("lib.rs"), "fn probe_target_zq() {}\n").unwrap();
}

fn search() -> Value {
    json!({ "query": "probe_target_zq" })
}

/// Call 1 panicked and got the error result, and call 2, a [`search`], was
/// answered.
fn assert_survived(run: &Run) {
    assert_eq!(
        run.result(1),
        &json!({
            "content": [{ "type": "text", "text": "tool panicked during execution" }],
            "isError": true,
        })
    );
    assert_answered(run, 2);
}

/// Call `id`, a [`search`], was answered, and the server exited 0 when its
/// stdin closed.
fn assert_answered(run: &Run, id: u64) {
    let result = run.result(id);
    assert_eq!(result.get("isError"), None, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("lib.rs:1"), "{text}");
    assert!(run.status.success(), "the server ended with {}", run.status);
}

/// A tool call run on a worker thread, as all but writes are.
#[test]
fn a_tool_panic_is_an_error_result_and_the_next_call_is_answered() {
    let dir = tempfile::tempdir().unwrap();
    with_source(dir.path());
    let mut panicking = search();
    panicking["panic_probe"] = json!(true);
    let run = serve(
        dir.path(),
        &[],
        false,
        &[
            initialize(),
            call(1, "tilth_search", &panicking),
            call(2, "tilth_search", &search()),
        ],
    );
    assert_survived(&run);
}

/// A write, which runs on the server's own thread so it is never abandoned.
#[test]
fn a_write_panic_is_an_error_result_and_the_next_call_is_answered() {
    let dir = tempfile::tempdir().unwrap();
    with_source(dir.path());
    let write = json!({
        "files": [{ "path": "new.txt", "mode": "overwrite", "content": "x\n" }],
        "panic_probe": true,
    });
    let run = serve(
        dir.path(),
        &["--edit"],
        false,
        &[
            initialize(),
            call(1, "tilth_write", &write),
            call(2, "tilth_search", &search()),
        ],
    );
    assert_survived(&run);
}

/// An overview that panics is left out of the instructions, and the server
/// answers the next call.
#[test]
fn an_overview_panic_leaves_initialize_answered() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("tilth-panic-probe");
    std::fs::create_dir(&root).unwrap();
    with_source(&root);
    let run = serve(
        &root,
        &[],
        true,
        &[initialize(), call(1, "tilth_search", &search())],
    );
    assert_eq!(
        run.result(0)["instructions"],
        include_str!("../prompts/mcp-base.md")
    );
    assert_answered(&run, 1);
}
