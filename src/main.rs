//! zotero-mcp-stdio: a stdio MCP server that AI apps launch, which forwards
//! tool calls to the MCP endpoint of a running Zotero.
//!
//! - Tool definitions come from `tools.json` next to the executable, so the
//!   list is available even when Zotero isn't running.
//! - Tool calls are forwarded to `http://127.0.0.1:<port>/mcp`. The port and
//!   the command that starts Zotero come from the sidecar file Zotero writes
//!   into its profile directory on every launch.
//! - If Zotero isn't running, this server starts it and waits for it.
//! - Every failure becomes a tool result the model can relay. The exception
//!   is a panic: the release build aborts, and the AI app sees the server exit.

mod profile;
mod sidecar;
mod zotero;

use clap::Parser;
use rmcp::model::{
	CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, InputRequiredResult,
	JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion, RequestMetaObject, ServerCapabilities,
	ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

const DOWNLOAD_URL: &str = "https://www.zotero.org/download/";

/// Stdio MCP server that connects AI apps to a running Zotero
#[derive(Parser)]
#[command(version)]
struct Options {
	/// Zotero profile directory [default: the default profile in profiles.ini]
	#[arg(long = "profile", value_name = "DIR")]
	profile_dir: Option<PathBuf>,
	/// Tool definitions [default: tools.json next to this executable]
	#[arg(long = "tools", value_name = "FILE")]
	tools_file: Option<PathBuf>,
	/// Longest time one tool call may take, including starting Zotero. Calls
	/// can wait on a consent dialog in Zotero; past this, the model is told
	/// the outcome is unknown.
	#[arg(long, value_name = "SECONDS", default_value_t = 50.0)]
	call_timeout: f64,
}

#[derive(Debug, thiserror::Error)]
enum StartupError {
	#[error("can't locate tools.json next to the executable; pass --tools")]
	NoToolsPath,
	#[error("can't read tool definitions from {path}: {source}")]
	ToolsUnreadable { path: PathBuf, source: std::io::Error },
	#[error("invalid tool definitions in {path}: {source}")]
	ToolsInvalid { path: PathBuf, source: serde_json::Error },
	#[error("invalid tool {name:?} in {path}: {reason}")]
	ToolInvalid {
		path: PathBuf,
		name: String,
		reason: &'static str,
	},
	#[error("--call-timeout must be a positive number of seconds")]
	BadTimeout,
	#[error(transparent)]
	Mcp(Box<rmcp::service::ServerInitializeError>),
	#[error(transparent)]
	Join(#[from] tokio::task::JoinError),
}

fn load_tools(path: &Path) -> Result<Vec<Tool>, StartupError> {
	#[derive(serde::Deserialize)]
	struct ToolsFile {
		tools: Vec<Tool>,
	}
	let text = std::fs::read_to_string(path).map_err(|source| StartupError::ToolsUnreadable {
		path: path.to_owned(),
		source,
	})?;
	let file: ToolsFile = serde_json::from_str(&text).map_err(|source| StartupError::ToolsInvalid {
		path: path.to_owned(),
		source,
	})?;
	// The MCP rules decoding doesn't check. Names are also sent as a header.
	let mut names = HashSet::new();
	for tool in &file.tools {
		let invalid = |reason| StartupError::ToolInvalid {
			path: path.to_owned(),
			name: tool.name.to_string(),
			reason,
		};
		let name_ok = (1..=128).contains(&tool.name.len())
			&& tool
				.name
				.bytes()
				.all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b));
		if !name_ok {
			return Err(invalid("names must be 1-128 letters, digits, '_', '-' or '.'"));
		}
		if !names.insert(&tool.name) {
			return Err(invalid("defined more than once"));
		}
		if tool.input_schema.get("type") != Some(&Value::from("object")) {
			return Err(invalid("inputSchema must have \"type\": \"object\""));
		}
	}
	Ok(file.tools)
}

struct Server {
	tools: Vec<Tool>,
	/// From `--profile`. Otherwise the default profile is looked up on every
	/// call, so a profile created after this server started is found.
	profile_dir: Option<PathBuf>,
	client: zotero::Client,
	call_timeout: Duration,
	/// Held while starting Zotero, so concurrent calls start it once.
	launching: Mutex<()>,
}

impl Server {
	/// The sidecar Zotero wrote, `Ok(None)` if there's none, or a message for
	/// the model if it exists but can't be used.
	fn read_sidecar(&self) -> Result<Option<sidecar::Sidecar>, String> {
		let unusable = |e: &dyn std::fmt::Display| {
			tracing::warn!("{e}");
			format!(
				"Zotero's connection settings can't be used ({e}). Ask the user to restart Zotero, which \
				rewrites them."
			)
		};
		let profile_dir = match &self.profile_dir {
			Some(dir) => Some(dir.clone()),
			None => match profile::profile_root() {
				Some(root) => profile::default_profile(&root).map_err(|e| unusable(&e))?,
				None => None,
			},
		};
		match profile_dir {
			Some(dir) => sidecar::read(&dir).map_err(|e| unusable(&e)),
			None => Ok(None),
		}
	}

	async fn forward(
		&self,
		request: &CallToolRequestParams,
		meta: &RequestMetaObject,
		client_info: Option<Implementation>,
		modern: bool,
		deadline: Instant,
	) -> Result<CallToolResponse, String> {
		let sidecar = self.read_sidecar()?.ok_or_else(not_set_up)?;
		let params = upstream_params(request, meta, client_info, modern);
		let send = |port, params| {
			self.client
				.request(port, "tools/call", Some(&request.name), params, deadline)
		};
		let mut result = send(sidecar.port, params.clone()).await;
		if matches!(result, Err(zotero::Error::NotRunning)) {
			let port = self.start_zotero(deadline).await?;
			result = send(port, params).await;
		}
		match result {
			Ok(value) => decode_result(value, modern),
			Err(zotero::Error::NotRunning) => Err("Zotero isn't running. Ask the user to open Zotero.".into()),
			Err(zotero::Error::NoEndpoint) => Err(
				"This version of Zotero doesn't support AI apps, or AI app access is turned off in Zotero's \
				settings. Ask the user to update Zotero or turn the setting on."
					.into(),
			),
			// The request reached Zotero, which may still carry it out. Zotero
			// can't undo an operation it has started, so the model must not
			// assume a retry is safe.
			Err(zotero::Error::Timeout) => Err(format!(
				"Zotero didn't respond within {} seconds. The request may be waiting for the user's approval \
				in a Zotero window, or may still be running, and it can still complete after this message. \
				Don't repeat this operation until you've asked the user whether it went through, or \
				inspected the state with a read tool.",
				self.call_timeout.as_secs_f64()
			)),
			Err(zotero::Error::Other(text)) => Err(text),
		}
	}

	/// Start Zotero if it isn't running and wait until it listens, returning
	/// its port.
	async fn start_zotero(&self, deadline: Instant) -> Result<u16, String> {
		let not_ready = || {
			format!(
				"Zotero is starting but wasn't ready within {} seconds. Nothing was sent to it, so it's safe \
				to call the tool again.",
				self.call_timeout.as_secs_f64()
			)
		};
		let _guard = tokio::time::timeout_at(deadline, self.launching.lock())
			.await
			.map_err(|_| not_ready())?;
		// Another call may have started Zotero while this one waited
		let sidecar = self.read_sidecar()?.ok_or_else(not_set_up)?;
		if zotero::Client::is_listening(sidecar.port, deadline).await {
			return Ok(sidecar.port);
		}
		let Some(launch) = &sidecar.launch else {
			return Err("Zotero isn't running. Ask the user to open Zotero.".into());
		};
		spawn_detached(&launch.command, &launch.args)
			.map_err(|e| format!("Zotero isn't running, and starting it failed ({e}). Ask the user to open Zotero."))?;
		let mut port = sidecar.port;
		while Instant::now() < deadline {
			// Zotero rewrites the sidecar as it starts, possibly with a new port
			if let Ok(Some(sidecar)) = self.read_sidecar() {
				port = sidecar.port;
			}
			if zotero::Client::is_listening(port, deadline).await {
				return Ok(port);
			}
			tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_millis(250))).await;
		}
		Err(not_ready())
	}
}

fn not_set_up() -> String {
	format!(
		"Zotero isn't set up for AI apps on this computer. If Zotero is installed, ask the user to open it \
		once. Otherwise, the user can download it from {DOWNLOAD_URL}"
	)
}

/// The `tools/call` params for Zotero: the client's request as received, with
/// the per-request `_meta` that 2026-07-28 requires.
///
/// `client_info` is how the AI app names itself: from this request's `_meta`,
/// or from `initialize` for a legacy client. Zotero keys per-app permissions on
/// its name.
fn upstream_params(
	request: &CallToolRequestParams,
	meta: &RequestMetaObject,
	client_info: Option<Implementation>,
	modern: bool,
) -> Value {
	let mut params = serde_json::to_value(request).expect("request params serialize");
	// rmcp moves the request's _meta into the request context
	let mut meta: JsonObject = meta.0.0.clone();
	meta.insert(
		"io.modelcontextprotocol/protocolVersion".into(),
		zotero::PROTOCOL_VERSION.into(),
	);
	if let Some(client_info) = client_info {
		meta.insert(
			"io.modelcontextprotocol/clientInfo".into(),
			serde_json::to_value(client_info).expect("client info serializes"),
		);
	}
	if !modern {
		// A legacy client can't take part in multi-round requests, so it's
		// described to Zotero as having no capabilities
		meta.insert("io.modelcontextprotocol/clientCapabilities".into(), json!({}));
	}
	meta.entry("io.modelcontextprotocol/clientCapabilities")
		.or_insert_with(|| json!({}));
	params["_meta"] = Value::Object(meta);
	params
}

fn decode_result(value: Value, modern: bool) -> Result<CallToolResponse, String> {
	let invalid = |e: serde_json::Error| format!("Unexpected tool result from Zotero: {e}");
	match value.get("resultType").and_then(Value::as_str) {
		// Zotero needs the client to supply input, e.g. an elicitation, and to
		// call again with the answers
		Some("input_required") if modern => serde_json::from_value::<InputRequiredResult>(value)
			.map(Into::into)
			.map_err(invalid),
		Some("input_required") => {
			Err("Zotero needs more input for this request, which this AI app doesn't support.".into())
		}
		Some("task") => Err("Zotero answered with a background task, which isn't supported yet.".into()),
		_ => serde_json::from_value::<CallToolResult>(value)
			.map(Into::into)
			.map_err(invalid),
	}
}

fn spawn_detached(command: &str, args: &[String]) -> std::io::Result<()> {
	let mut cmd = std::process::Command::new(command);
	cmd.args(args)
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null());
	#[cfg(unix)]
	{
		// Its own process group, so an AI app that stops its servers by
		// signalling their group doesn't take Zotero down too
		use std::os::unix::process::CommandExt;
		cmd.process_group(0);
	}
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		const DETACHED_PROCESS: u32 = 0x0000_0008;
		const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
		cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
	}
	let mut child = cmd.spawn()?;
	// Reap the child when it exits, so it doesn't linger as a zombie
	std::thread::spawn(move || child.wait());
	Ok(())
}

impl ServerHandler for Server {
	fn get_info(&self) -> ServerConfig {
		let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
		info.server_info = Implementation::new("zotero", env!("CARGO_PKG_VERSION"));
		info
	}

	async fn list_tools(
		&self,
		_request: Option<PaginatedRequestParams>,
		_context: RequestContext<RoleServer>,
	) -> Result<ListToolsResult, McpError> {
		Ok(ListToolsResult::with_all_items(self.tools.clone()))
	}

	async fn call_tool(
		&self,
		request: CallToolRequestParams,
		context: RequestContext<RoleServer>,
	) -> Result<CallToolResponse, McpError> {
		if !self.tools.iter().any(|tool| tool.name == request.name) {
			return Err(McpError::invalid_params(
				format!("Unknown tool: {}", request.name),
				None,
			));
		}
		let modern = context
			.protocol_version()
			.is_some_and(|v| v.as_str() >= ProtocolVersion::V_2026_07_28.as_str());
		let deadline = Instant::now() + self.call_timeout;
		let result = tokio::select! {
			result = self.forward(&request, &context.meta, context.client_info(), modern, deadline) => result,
			// rmcp drops the response to a cancelled call but keeps running
			// this handler. Stop it, so a cancelled call doesn't start Zotero
			// or send the request later. One already sent can't be recalled.
			() = context.ct.cancelled() => return Err(McpError::internal_error("Cancelled", None)),
		};
		Ok(result.unwrap_or_else(|text| CallToolResult::error(vec![ContentBlock::text(text)]).into()))
	}
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
	// Logs go to stderr; stdout carries the MCP messages
	tracing_subscriber::fmt()
		.with_writer(std::io::stderr)
		.with_ansi(false)
		.with_target(false)
		.init();
	match run(Options::parse()).await {
		Ok(()) => std::process::ExitCode::SUCCESS,
		Err(e) => {
			tracing::error!("{e}");
			std::process::ExitCode::FAILURE
		}
	}
}

async fn run(options: Options) -> Result<(), StartupError> {
	let call_timeout = Duration::try_from_secs_f64(options.call_timeout)
		.ok()
		.filter(|d| !d.is_zero())
		.ok_or(StartupError::BadTimeout)?;
	let tools_file = match options.tools_file {
		Some(path) => path,
		None => std::env::current_exe()
			.ok()
			.and_then(|exe| Some(exe.parent()?.join("tools.json")))
			.ok_or(StartupError::NoToolsPath)?,
	};
	// Fail at startup rather than serve an empty list: the AI app then shows
	// the server as failed, with this message in its log
	let tools = load_tools(&tools_file)?;
	let server = Arc::new(Server {
		tools,
		profile_dir: options.profile_dir,
		client: zotero::Client::new(),
		call_timeout,
		launching: Mutex::new(()),
	});
	let service = server
		.serve(rmcp::transport::stdio())
		.await
		.map_err(|e| StartupError::Mcp(Box::new(e)))?;
	service.waiting().await?;
	Ok(())
}
