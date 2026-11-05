//! Drives the built binary over stdio against a fake Zotero HTTP server.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a test waits for any one response before failing, so a broken
/// build fails the test instead of hanging CI.
const RESPONSE_DEADLINE: Duration = Duration::from_secs(10);

const TOOLS: &str = r#"{
	"tools": [
		{
			"name": "zotero_search_items",
			"description": "Search the library",
			"inputSchema": { "type": "object", "properties": { "query": { "type": "string" } } }
		}
	]
}"#;

// Fixture: a temp dir with tools.json and a profile dir, and the binary

struct Fixture {
	dir: tempfile::TempDir,
}

impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		std::fs::create_dir(dir.path().join("profile")).unwrap();
		std::fs::write(dir.path().join("tools.json"), TOOLS).unwrap();
		Fixture { dir }
	}

	fn path(&self) -> &Path {
		self.dir.path()
	}

	fn sidecar_path(&self) -> std::path::PathBuf {
		self.path().join("profile/zotero-mcp.json")
	}

	fn write_sidecar(&self, sidecar: Value) {
		std::fs::write(self.sidecar_path(), sidecar.to_string()).unwrap();
	}

	/// The binary with `--profile` pointing at the fixture.
	fn command(&self) -> Command {
		let mut command = self.command_without_profile();
		command.arg("--profile").arg(self.path().join("profile"));
		command
	}

	fn command_without_profile(&self) -> Command {
		let mut command = Command::new(env!("CARGO_BIN_EXE_zotero-mcp-stdio"));
		command
			.args(["--client", "test-client"])
			.arg("--tools")
			.arg(self.path().join("tools.json"));
		command
	}

	fn spawn(&self) -> Session {
		Session::spawn(self.command())
	}

	fn spawn_with(&self, args: &[&str]) -> Session {
		let mut command = self.command();
		command.args(args);
		Session::spawn(command)
	}
}

// Session: one running binary, spoken to as an MCP client

struct Session {
	child: Child,
	stdin: ChildStdin,
	lines: Receiver<String>,
	next_id: u64,
}

impl Session {
	fn spawn(mut command: Command) -> Self {
		let mut child = command
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::inherit())
			.spawn()
			.unwrap();
		let stdin = child.stdin.take().unwrap();
		let stdout = BufReader::new(child.stdout.take().unwrap());
		// Read on a thread, so waiting for a response can time out
		let (sender, lines) = channel();
		std::thread::spawn(move || {
			for line in stdout.lines() {
				let Ok(line) = line else { break };
				if sender.send(line).is_err() {
					break;
				}
			}
		});
		Session {
			child,
			stdin,
			lines,
			next_id: 1,
		}
	}

	fn send(&mut self, message: Value) {
		writeln!(self.stdin, "{message}").unwrap();
		self.stdin.flush().unwrap();
	}

	/// Send a request without waiting for its response, returning its id.
	fn send_request(&mut self, method: &str, params: Value) -> u64 {
		let id = self.next_id;
		self.next_id += 1;
		self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
		id
	}

	fn response(&mut self, id: u64) -> Value {
		let deadline = Instant::now() + RESPONSE_DEADLINE;
		loop {
			let remaining = deadline.saturating_duration_since(Instant::now());
			let line = self
				.lines
				.recv_timeout(remaining)
				.unwrap_or_else(|_| panic!("no response to request {id} within {RESPONSE_DEADLINE:?}"));
			let message: Value = serde_json::from_str(&line).unwrap();
			if message["id"] == json!(id) {
				return message;
			}
		}
	}

	fn request(&mut self, method: &str, params: Value) -> Value {
		let id = self.send_request(method, params);
		self.response(id)
	}

	/// Call a tool as a 2026-07-28 client, with per-request metadata.
	fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
		self.request(
			"tools/call",
			json!({ "name": name, "arguments": arguments, "_meta": modern_meta() }),
		)
	}

	/// Open a session the pre-2026 way, with `initialize`.
	fn initialize_legacy(&mut self) {
		let response = self.request(
			"initialize",
			json!({
				"protocolVersion": "2025-11-25",
				"capabilities": {},
				"clientInfo": { "name": "test", "version": "0" }
			}),
		);
		assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
		self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
	}
}

impl Drop for Session {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

fn modern_meta() -> Value {
	json!({
		"io.modelcontextprotocol/protocolVersion": "2026-07-28",
		"io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0" },
		"io.modelcontextprotocol/clientCapabilities": {}
	})
}

fn text_of(response: &Value) -> &str {
	response["result"]["content"][0]["text"].as_str().unwrap_or_default()
}

fn assert_tool_error(response: &Value, expected_text: &str) {
	assert_eq!(response["result"]["isError"], true, "{response}");
	assert!(text_of(response).contains(expected_text), "{response}");
}

// FakeZotero: a minimal HTTP/1.0 server like Zotero's

/// Recorded requests: the lowercased request head and the JSON body.
type Requests = Arc<Mutex<Vec<(String, Value)>>>;

/// Builds the raw HTTP response for a JSON-RPC request.
type Responder = Arc<dyn Fn(&Value) -> String + Send + Sync>;

struct FakeZotero {
	port: u16,
	requests: Requests,
}

impl FakeZotero {
	/// Answers every request with `result` as its JSON-RPC result.
	fn replying(result: Value) -> Self {
		Self::start(TcpListener::bind("127.0.0.1:0").unwrap(), answer_with(result))
	}

	fn start(listener: TcpListener, respond: Responder) -> Self {
		let port = listener.local_addr().unwrap().port();
		let requests = Requests::default();
		let recorded = requests.clone();
		std::thread::spawn(move || {
			for stream in listener.incoming() {
				let Ok(stream) = stream else { break };
				handle(stream, &*respond, &recorded);
			}
		});
		FakeZotero { port, requests }
	}

	fn requests(&self) -> Vec<(String, Value)> {
		self.requests.lock().unwrap().clone()
	}
}

fn answer_with(result: Value) -> Responder {
	Arc::new(move |request| {
		let body = json!({ "jsonrpc": "2.0", "id": request["id"], "result": result });
		format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}")
	})
}

fn handle(
	mut stream: TcpStream,
	respond: &(dyn Fn(&Value) -> String + Send + Sync),
	recorded: &Mutex<Vec<(String, Value)>>,
) {
	stream.set_read_timeout(Some(RESPONSE_DEADLINE)).unwrap();
	let mut reader = BufReader::new(stream.try_clone().unwrap());
	let mut head = String::new();
	loop {
		let mut line = String::new();
		if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
			break;
		}
		// Header names are case-insensitive; lowercase them like Zotero's server does
		head.push_str(&match line.split_once(':') {
			Some((name, value)) => format!("{}:{value}", name.to_lowercase()),
			None => line,
		});
	}
	// A readiness probe connects and closes without sending anything
	if head.is_empty() {
		return;
	}
	let length: usize = head
		.lines()
		.find_map(|l| l.strip_prefix("content-length: "))
		.map(|v| v.trim().parse().unwrap())
		.unwrap_or(0);
	let mut body = vec![0; length];
	if reader.read_exact(&mut body).is_err() {
		return;
	}
	let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
	recorded.lock().unwrap().push((head, request.clone()));
	let _ = stream.write_all(respond(&request).as_bytes());
}

/// A port nothing listens on. Another process could take it before the test
/// uses it, which is unlikely enough on a test machine.
fn free_port() -> u16 {
	TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn search_reply() -> Value {
	json!({ "resultType": "complete", "content": [{ "type": "text", "text": "3 items" }] })
}

// Tool list

#[test]
fn lists_tools_from_file_in_both_protocol_eras_without_zotero() {
	let fixture = Fixture::new();
	// No sidecar and no Zotero: the tool list must still be served

	let mut legacy = fixture.spawn();
	legacy.initialize_legacy();
	let tools = legacy.request("tools/list", json!({}));
	assert_eq!(tools["result"]["tools"][0]["name"], "zotero_search_items");

	let mut modern = fixture.spawn();
	let tools = modern.request("tools/list", json!({ "_meta": modern_meta() }));
	assert_eq!(tools["result"]["tools"][0]["name"], "zotero_search_items", "{tools}");
}

#[test]
fn exits_with_an_error_for_missing_or_invalid_tools_json() {
	let fixture = Fixture::new();
	let tools_json = fixture.path().join("tools.json");
	let stderr_of_run = || {
		let output = fixture.command().stdin(Stdio::null()).output().unwrap();
		assert!(!output.status.success());
		String::from_utf8_lossy(&output.stderr).into_owned()
	};

	std::fs::remove_file(&tools_json).unwrap();
	let stderr = stderr_of_run();
	assert!(stderr.contains("tools.json"), "{stderr}");

	// Valid JSON, but MCP requires tool input schemas to be objects
	std::fs::write(
		&tools_json,
		r#"{ "tools": [ { "name": "zotero_x", "inputSchema": { "type": "string" } } ] }"#,
	)
	.unwrap();
	let stderr = stderr_of_run();
	assert!(stderr.contains("inputSchema"), "{stderr}");
}

#[test]
fn rejects_calls_to_tools_not_in_the_list() {
	let fixture = Fixture::new();
	let zotero = FakeZotero::replying(search_reply());
	fixture.write_sidecar(json!({ "version": 1, "port": zotero.port }));

	let response = fixture.spawn().call_tool("zotero_delete_everything", json!({}));
	assert_eq!(response["error"]["code"], -32602, "{response}");
	assert!(zotero.requests().is_empty());
}

// Forwarding

#[test]
fn forwards_the_call_with_its_metadata_unchanged() {
	let fixture = Fixture::new();
	let zotero = FakeZotero::replying(search_reply());
	fixture.write_sidecar(json!({ "version": 1, "port": zotero.port }));

	let mut meta = modern_meta();
	meta["com.example/trace"] = json!("t-1");
	let response = fixture.spawn().request(
		"tools/call",
		json!({
			"name": "zotero_search_items",
			"arguments": { "query": "climate" },
			"requestState": "opaque-state",
			"_meta": meta,
		}),
	);
	assert_eq!(text_of(&response), "3 items", "{response}");
	assert_ne!(response["result"]["isError"], true);

	let (head, body) = &zotero.requests()[0];
	assert!(head.starts_with("POST /mcp HTTP/1.1\r\n"), "{head}");
	// Zotero rejects any other Host, to block DNS rebinding
	assert!(head.contains(&format!("host: 127.0.0.1:{}\r\n", zotero.port)), "{head}");
	assert!(head.contains("zotero-mcp-client: test-client\r\n"), "{head}");
	assert!(head.contains("mcp-name: zotero_search_items\r\n"), "{head}");
	assert!(!head.contains("origin:"), "browser guard would drop the request");

	assert_eq!(body["method"], "tools/call");
	let params = &body["params"];
	assert_eq!(params["arguments"], json!({ "query": "climate" }));
	assert_eq!(params["requestState"], "opaque-state");
	assert_eq!(params["_meta"]["com.example/trace"], "t-1");
	assert_eq!(params["_meta"]["io.modelcontextprotocol/clientInfo"]["name"], "test");
}

#[test]
fn sends_legacy_calls_as_2026_requests_from_this_server() {
	let fixture = Fixture::new();
	let zotero = FakeZotero::replying(search_reply());
	fixture.write_sidecar(json!({ "version": 1, "port": zotero.port }));

	let mut session = fixture.spawn();
	session.initialize_legacy();
	let response = session.request("tools/call", json!({ "name": "zotero_search_items", "arguments": {} }));
	assert_eq!(text_of(&response), "3 items", "{response}");

	let meta = &zotero.requests()[0].1["params"]["_meta"];
	assert_eq!(meta["io.modelcontextprotocol/protocolVersion"], "2026-07-28");
	assert_eq!(meta["io.modelcontextprotocol/clientInfo"]["name"], "zotero-mcp-stdio");
	// No capabilities, so Zotero won't ask for input a legacy client can't give
	assert_eq!(meta["io.modelcontextprotocol/clientCapabilities"], json!({}));
}

#[test]
fn passes_input_requests_to_clients_that_support_them() {
	let fixture = Fixture::new();
	let zotero = FakeZotero::replying(json!({
		"resultType": "input_required",
		"inputRequests": {},
		"requestState": "round-1",
	}));
	fixture.write_sidecar(json!({ "version": 1, "port": zotero.port }));

	let response = fixture.spawn().call_tool("zotero_search_items", json!({}));
	assert_eq!(response["result"]["resultType"], "input_required", "{response}");
	assert_eq!(response["result"]["requestState"], "round-1");

	let mut legacy = fixture.spawn();
	legacy.initialize_legacy();
	let response = legacy.request("tools/call", json!({ "name": "zotero_search_items", "arguments": {} }));
	assert_tool_error(&response, "doesn't support");
}

#[test]
fn does_not_follow_redirects() {
	let fixture = Fixture::new();
	let elsewhere = FakeZotero::replying(search_reply());
	let location = format!("http://127.0.0.1:{}/mcp", elsewhere.port);
	let redirecting = FakeZotero::start(
		TcpListener::bind("127.0.0.1:0").unwrap(),
		Arc::new(move |_| format!("HTTP/1.0 307 Temporary Redirect\r\nLocation: {location}\r\n\r\n")),
	);
	fixture.write_sidecar(json!({ "version": 1, "port": redirecting.port }));

	let response = fixture
		.spawn()
		.call_tool("zotero_search_items", json!({ "query": "x" }));
	assert_tool_error(&response, "unexpected response");
	assert!(
		elsewhere.requests().is_empty(),
		"the request was sent to the redirect target"
	);
}

#[test]
fn rejects_a_response_to_a_different_request() {
	let fixture = Fixture::new();
	let zotero = FakeZotero::start(
		TcpListener::bind("127.0.0.1:0").unwrap(),
		Arc::new(|_| {
			let body = json!({ "jsonrpc": "2.0", "id": 999, "result": search_reply() });
			format!("HTTP/1.0 200 OK\r\n\r\n{body}")
		}),
	);
	fixture.write_sidecar(json!({ "version": 1, "port": zotero.port }));

	let response = fixture.spawn().call_tool("zotero_search_items", json!({}));
	assert_tool_error(&response, "unexpected response");
}

#[test]
fn reports_a_timeout_without_suggesting_a_retry_is_safe() {
	let fixture = Fixture::new();
	// Accepts connections but never answers, like a call waiting on a consent dialog
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();
	std::thread::spawn(move || {
		let held: Vec<_> = listener.incoming().take(1).collect();
		std::thread::sleep(RESPONSE_DEADLINE);
		drop(held);
	});
	fixture.write_sidecar(json!({ "version": 1, "port": port }));

	let response = fixture
		.spawn_with(&["--call-timeout", "0.3"])
		.call_tool("zotero_search_items", json!({}));
	assert_tool_error(&response, "approval");
	assert_tool_error(&response, "Don't repeat this operation");
}

// Finding Zotero

#[test]
fn explains_when_zotero_is_not_set_up() {
	let response = Fixture::new().spawn().call_tool("zotero_search_items", json!({}));
	assert_tool_error(&response, "zotero.org/download");
}

#[test]
fn explains_an_unreadable_sidecar_instead_of_calling_it_missing() {
	let fixture = Fixture::new();
	std::fs::create_dir(fixture.sidecar_path()).unwrap();
	let response = fixture.spawn().call_tool("zotero_search_items", json!({}));
	assert_tool_error(&response, "can't be used");
	assert!(!text_of(&response).contains("download"), "{response}");
}

#[test]
fn rejects_a_sidecar_format_it_does_not_know() {
	let fixture = Fixture::new();
	// A future format whose fields this build would otherwise misread
	fixture.write_sidecar(json!({ "version": 2, "endpoints": [{ "port": 1 }] }));
	let response = fixture.spawn().call_tool("zotero_search_items", json!({}));
	assert_tool_error(&response, "format 2");
}

#[cfg(target_os = "linux")]
#[test]
fn finds_a_profile_created_after_it_started() {
	let fixture = Fixture::new();
	let home = fixture.path().join("home");
	let mut command = fixture.command_without_profile();
	command
		.env("HOME", &home)
		.env_remove("XDG_CONFIG_HOME")
		.env_remove("MOZ_LEGACY_HOME");
	let mut session = Session::spawn(command);
	assert_tool_error(&session.call_tool("zotero_search_items", json!({})), "isn't set up");

	// The user opens Zotero for the first time, which creates the profile
	let zotero = FakeZotero::replying(search_reply());
	let root = home.join(".config/zotero/zotero");
	std::fs::create_dir_all(root.join("Profiles/abc.default")).unwrap();
	std::fs::write(
		root.join("profiles.ini"),
		"[Profile0]\nName=default\nIsRelative=1\nPath=Profiles/abc.default\nDefault=1\n",
	)
	.unwrap();
	std::fs::write(
		root.join("Profiles/abc.default/zotero-mcp.json"),
		json!({ "version": 1, "port": zotero.port }).to_string(),
	)
	.unwrap();

	let response = session.call_tool("zotero_search_items", json!({}));
	assert_eq!(text_of(&response), "3 items", "{response}");
}

// Starting Zotero

#[test]
fn explains_when_zotero_is_not_running_and_cannot_be_started() {
	let fixture = Fixture::new();
	fixture.write_sidecar(json!({ "version": 1, "port": free_port() }));
	let response = fixture.spawn().call_tool("zotero_search_items", json!({}));
	assert_tool_error(&response, "open Zotero");
}

/// A launch command that rewrites the sidecar with `port`, as Zotero does
/// when it starts.
#[cfg(unix)]
fn launch_rewriting_sidecar(fixture: &Fixture, port: u16) -> Value {
	let sidecar = json!({ "version": 1, "port": port }).to_string();
	json!({
		"command": "sh",
		"args": ["-c", "printf '%s' \"$1\" > \"$2\"", "sh", sidecar, fixture.sidecar_path()]
	})
}

#[cfg(unix)]
#[test]
fn starts_zotero_and_uses_the_port_it_starts_on() {
	let fixture = Fixture::new();
	let zotero = FakeZotero::replying(search_reply());
	// The old sidecar names a port nothing listens on
	fixture.write_sidecar(json!({
		"version": 1,
		"port": free_port(),
		"launch": launch_rewriting_sidecar(&fixture, zotero.port),
	}));

	let response = fixture.spawn().call_tool("zotero_search_items", json!({}));
	assert_eq!(text_of(&response), "3 items", "{response}");
	assert_eq!(zotero.requests().len(), 1);
}

#[cfg(unix)]
#[test]
fn call_timeout_includes_waiting_for_zotero_to_start() {
	let fixture = Fixture::new();
	// Zotero "starts" but never listens
	fixture.write_sidecar(json!({ "version": 1, "port": free_port(), "launch": { "command": "true" } }));

	let started = Instant::now();
	let response = fixture
		.spawn_with(&["--call-timeout", "0.5"])
		.call_tool("zotero_search_items", json!({}));
	assert!(
		started.elapsed() < Duration::from_secs(3),
		"took {:?}",
		started.elapsed()
	);
	// Nothing reached Zotero, so this one may say a retry is safe
	assert_tool_error(&response, "safe to call the tool again");
}

#[cfg(unix)]
#[test]
fn a_cancelled_call_never_reaches_zotero() {
	let fixture = Fixture::new();
	let port = free_port();
	fixture.write_sidecar(json!({ "version": 1, "port": port, "launch": { "command": "true" } }));

	let mut session = fixture.spawn();
	session.initialize_legacy();
	let id = session.send_request("tools/call", json!({ "name": "zotero_search_items", "arguments": {} }));
	// While it waits for Zotero to start, the client cancels the call
	std::thread::sleep(Duration::from_millis(300));
	session.send(json!({
		"jsonrpc": "2.0",
		"method": "notifications/cancelled",
		"params": { "requestId": id },
	}));
	std::thread::sleep(Duration::from_millis(100));

	// Then Zotero comes up. A call that kept running would now be sent.
	let zotero = FakeZotero::start(
		TcpListener::bind(("127.0.0.1", port)).unwrap(),
		answer_with(search_reply()),
	);
	std::thread::sleep(Duration::from_secs(1));
	assert!(zotero.requests().is_empty(), "the cancelled call was sent");

	// The server keeps serving
	let tools = session.request("tools/list", json!({}));
	assert_eq!(tools["result"]["tools"][0]["name"], "zotero_search_items");
}
