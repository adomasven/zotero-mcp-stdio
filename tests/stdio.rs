//! Drives the built binary over stdio against a fake Zotero HTTP server.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOOLS: &str = r#"{
	"tools": [
		{
			"name": "zotero_search_items",
			"description": "Search the library",
			"inputSchema": { "type": "object", "properties": { "query": { "type": "string" } } }
		}
	]
}"#;

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

	fn write_sidecar(&self, sidecar: Value) {
		std::fs::write(self.path().join("profile/zotero-mcp.json"), sidecar.to_string()).unwrap();
	}

	fn command(&self) -> Command {
		let mut command = Command::new(env!("CARGO_BIN_EXE_zotero-mcp-stdio"));
		command
			.args(["--client", "test-client"])
			.arg("--profile")
			.arg(self.path().join("profile"))
			.arg("--tools")
			.arg(self.path().join("tools.json"));
		command
	}

	fn spawn(&self) -> Session {
		self.spawn_with(&[])
	}

	fn spawn_with(&self, args: &[&str]) -> Session {
		let mut child = self
			.command()
			.args(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::inherit())
			.spawn()
			.unwrap();
		let stdin = child.stdin.take().unwrap();
		let stdout = BufReader::new(child.stdout.take().unwrap());
		Session {
			child,
			stdin,
			stdout,
			next_id: 1,
		}
	}
}

struct Session {
	child: Child,
	stdin: ChildStdin,
	stdout: BufReader<ChildStdout>,
	next_id: u64,
}

impl Session {
	fn send(&mut self, message: Value) {
		writeln!(self.stdin, "{message}").unwrap();
		self.stdin.flush().unwrap();
	}

	fn request(&mut self, method: &str, params: Value) -> Value {
		let id = self.next_id;
		self.next_id += 1;
		self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
		loop {
			let mut line = String::new();
			assert!(self.stdout.read_line(&mut line).unwrap() > 0, "server closed stdout");
			let message: Value = serde_json::from_str(&line).unwrap();
			if message["id"] == json!(id) {
				return message;
			}
		}
	}

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

/// A fake Zotero: answers every POST /mcp with `reply` and records requests.
struct FakeZotero {
	port: u16,
	requests: Requests,
}

impl FakeZotero {
	fn start(listener: TcpListener, reply: Value) -> Self {
		let port = listener.local_addr().unwrap().port();
		let requests = Arc::new(Mutex::new(Vec::new()));
		serve(listener, reply, requests.clone());
		FakeZotero { port, requests }
	}
}

type Requests = Arc<Mutex<Vec<(String, Value)>>>;

fn serve(listener: TcpListener, reply: Value, recorded: Requests) {
	std::thread::spawn(move || {
		for stream in listener.incoming() {
			let Ok(stream) = stream else { break };
			handle(stream, &reply, &recorded);
		}
	});
}

fn handle(mut stream: TcpStream, reply: &Value, recorded: &Mutex<Vec<(String, Value)>>) {
	let mut reader = BufReader::new(stream.try_clone().unwrap());
	let mut head = String::new();
	loop {
		let mut line = String::new();
		if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
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
	reader.read_exact(&mut body).unwrap();
	let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
	recorded.lock().unwrap().push((head, request.clone()));

	let response = json!({ "jsonrpc": "2.0", "id": request["id"], "result": reply }).to_string();
	let _ = write!(
		stream,
		"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{response}"
	);
}

fn free_port() -> u16 {
	TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn text_of(response: &Value) -> &str {
	response["result"]["content"][0]["text"].as_str().unwrap_or_default()
}

fn search_reply() -> Value {
	json!({ "resultType": "complete", "content": [{ "type": "text", "text": "3 items" }] })
}

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
fn forwards_tool_calls_to_zotero() {
	let fixture = Fixture::new();
	let zotero = FakeZotero::start(TcpListener::bind("127.0.0.1:0").unwrap(), search_reply());
	fixture.write_sidecar(json!({ "version": 1, "port": zotero.port }));

	let mut session = fixture.spawn();
	session.initialize_legacy();
	let response = session.request(
		"tools/call",
		json!({ "name": "zotero_search_items", "arguments": { "query": "climate" } }),
	);
	assert_eq!(text_of(&response), "3 items", "{response}");
	assert_ne!(response["result"]["isError"], true);

	let requests = zotero.requests.lock().unwrap();
	let (head, body) = &requests[0];
	assert!(head.starts_with("POST /mcp HTTP/1.1\r\n"), "{head}");
	// Zotero rejects any other Host, to block DNS rebinding
	assert!(head.contains(&format!("host: 127.0.0.1:{}\r\n", zotero.port)), "{head}");
	assert!(head.contains("zotero-mcp-client: test-client\r\n"), "{head}");
	assert!(head.contains("mcp-name: zotero_search_items\r\n"), "{head}");
	assert!(!head.contains("origin:"), "browser guard would drop the request");
	assert_eq!(body["method"], "tools/call");
	assert_eq!(body["params"]["arguments"]["query"], "climate");
	assert_eq!(
		body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
		"2026-07-28"
	);
}

#[test]
fn explains_when_zotero_is_not_set_up() {
	let fixture = Fixture::new();
	let mut session = fixture.spawn();
	let response = session.request(
		"tools/call",
		json!({ "name": "zotero_search_items", "arguments": {}, "_meta": modern_meta() }),
	);
	assert_eq!(response["result"]["isError"], true, "{response}");
	assert!(text_of(&response).contains("zotero.org/download"), "{response}");
}

#[test]
fn explains_when_zotero_is_not_running_and_cannot_be_started() {
	let fixture = Fixture::new();
	fixture.write_sidecar(json!({ "version": 1, "port": free_port() }));
	let mut session = fixture.spawn();
	let response = session.request(
		"tools/call",
		json!({ "name": "zotero_search_items", "arguments": {}, "_meta": modern_meta() }),
	);
	assert_eq!(response["result"]["isError"], true, "{response}");
	assert!(text_of(&response).contains("open Zotero"), "{response}");
}

#[test]
fn reports_timeout_as_pending_approval() {
	let fixture = Fixture::new();
	// Accepts connections but never answers, like a call waiting on a consent dialog
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();
	std::thread::spawn(move || {
		let held: Vec<_> = listener.incoming().take(1).collect();
		std::thread::sleep(Duration::from_secs(30));
		drop(held);
	});
	fixture.write_sidecar(json!({ "version": 1, "port": port }));
	let mut session = fixture.spawn_with(&["--call-timeout", "0.3"]);
	let response = session.request(
		"tools/call",
		json!({ "name": "zotero_search_items", "arguments": {}, "_meta": modern_meta() }),
	);
	assert_eq!(response["result"]["isError"], true, "{response}");
	assert!(text_of(&response).contains("approve"), "{response}");
}

#[test]
fn rejects_a_sidecar_format_it_does_not_know() {
	let fixture = Fixture::new();
	// A future format whose fields this build would otherwise misread
	fixture.write_sidecar(json!({ "version": 2, "endpoints": [{ "port": 1 }] }));
	let mut session = fixture.spawn();
	let response = session.request(
		"tools/call",
		json!({ "name": "zotero_search_items", "arguments": {}, "_meta": modern_meta() }),
	);
	assert_eq!(response["result"]["isError"], true, "{response}");
	assert!(text_of(&response).contains("format 2"), "{response}");
}

#[test]
fn exits_with_an_error_when_tools_json_is_missing() {
	let fixture = Fixture::new();
	std::fs::remove_file(fixture.path().join("tools.json")).unwrap();
	let output = fixture.command().stdin(Stdio::null()).output().unwrap();
	assert!(!output.status.success());
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(stderr.contains("tools.json"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn starts_zotero_with_the_sidecar_command_and_retries() {
	let fixture = Fixture::new();
	let port = free_port();
	let marker = fixture.path().join("launched");
	fixture.write_sidecar(json!({
		"version": 1,
		"port": port,
		"launch": { "command": "touch", "args": [marker] }
	}));

	// The fake Zotero only starts listening once the launch command has run
	let requests = wait_then_serve(marker.clone(), port);

	let mut session = fixture.spawn();
	let response = session.request(
		"tools/call",
		json!({ "name": "zotero_search_items", "arguments": {}, "_meta": modern_meta() }),
	);
	assert_eq!(text_of(&response), "3 items", "{response}");
	assert!(marker.exists());
	assert_eq!(requests.lock().unwrap().len(), 1);
}

#[cfg(unix)]
fn wait_then_serve(marker: PathBuf, port: u16) -> Requests {
	use std::time::Instant;

	let requests = Requests::default();
	let recorded = requests.clone();
	std::thread::spawn(move || {
		let deadline = Instant::now() + Duration::from_secs(20);
		while !marker.exists() && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(50));
		}
		serve(
			TcpListener::bind(("127.0.0.1", port)).unwrap(),
			search_reply(),
			recorded,
		);
	});
	requests
}
