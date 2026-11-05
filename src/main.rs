//! zotero-mcp-stdio: a stdio MCP server that AI apps launch, which forwards
//! tool calls to the MCP endpoint of a running Zotero.
//!
//! - Tool definitions come from `tools.json` next to the executable, so the
//!   list is available even when Zotero isn't running.
//! - Tool calls are forwarded to `http://127.0.0.1:<port>/mcp`. The port and
//!   the command that starts Zotero come from the sidecar file Zotero writes
//!   into its profile directory on every launch.
//! - If Zotero isn't running, this server starts it and waits for it.

mod profile;
mod sidecar;
mod zotero;

use clap::Parser;
use rmcp::model::{
	CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, ListToolsResult,
	PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};
use sidecar::Sidecar;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const DOWNLOAD_URL: &str = "https://www.zotero.org/download/";
/// How long to wait for a started Zotero to open its port.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Stdio MCP server that connects AI apps to a running Zotero
#[derive(Parser)]
#[command(version)]
struct Options {
	/// Identifies the AI app (claude-desktop, codex, ...) to Zotero, for per-app permissions
	#[arg(long = "client", value_name = "ID")]
	client_id: Option<String>,
	/// Zotero profile directory [default: the default profile in profiles.ini]
	#[arg(long = "profile", value_name = "DIR")]
	profile_dir: Option<PathBuf>,
	/// Tool definitions [default: tools.json next to this executable]
	#[arg(long = "tools", value_name = "FILE")]
	tools_file: Option<PathBuf>,
	/// How long to wait for one tool call before telling the model Zotero may
	/// be waiting for the user's approval. Calls can wait on a consent dialog.
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
	Ok(file.tools)
}

struct Server {
	tools: Vec<Tool>,
	profile_dir: Option<PathBuf>,
	client: zotero::Client,
	/// Held while starting Zotero, so concurrent calls start it once.
	launching: Mutex<()>,
}

impl Server {
	fn sidecar(&self) -> Result<Option<Sidecar>, sidecar::Error> {
		match &self.profile_dir {
			Some(dir) => sidecar::read(dir),
			None => Ok(None),
		}
	}

	async fn forward(&self, request: &CallToolRequestParams) -> Result<CallToolResult, String> {
		let sidecar = self.sidecar().map_err(|e| {
			tracing::warn!("{e}");
			format!(
				"Zotero's connection settings can't be read ({e}). Ask the user to restart Zotero, which updates them."
			)
		})?;
		let Some(sidecar) = sidecar else {
			return Err(format!(
				"Zotero isn't set up for AI apps on this computer. If Zotero is installed, ask the user to open it \
				once. Otherwise, the user can download it from {DOWNLOAD_URL}"
			));
		};
		let params = json!({
			"name": request.name,
			"arguments": request.arguments.clone().map(Value::Object).unwrap_or_else(|| json!({})),
			"_meta": {
				"io.modelcontextprotocol/protocolVersion": zotero::PROTOCOL_VERSION,
				"io.modelcontextprotocol/clientInfo": {
					"name": "zotero-mcp-stdio",
					"version": env!("CARGO_PKG_VERSION"),
				},
				"io.modelcontextprotocol/clientCapabilities": {},
			},
		});
		let mut result = self
			.client
			.request(sidecar.port, "tools/call", Some(&request.name), params.clone())
			.await;
		if matches!(result, Err(zotero::Error::NotRunning)) {
			self.start_zotero(&sidecar).await?;
			result = self
				.client
				.request(sidecar.port, "tools/call", Some(&request.name), params)
				.await;
		}
		match result {
			Ok(value) => serde_json::from_value(value).map_err(|e| format!("Unexpected tool result from Zotero: {e}")),
			Err(zotero::Error::NotRunning) => Err("Zotero isn't running. Ask the user to open Zotero.".into()),
			Err(zotero::Error::NoEndpoint) => Err(
				"This version of Zotero doesn't support AI apps, or AI app access is turned off in Zotero's \
				settings. Ask the user to update Zotero or turn the setting on."
					.into(),
			),
			Err(zotero::Error::Timeout) => Err(
				"Zotero didn't answer in time. It may be waiting for the user to approve this request in a \
				Zotero window. Ask the user to respond there, then call the tool again."
					.into(),
			),
			Err(zotero::Error::Other(text)) => Err(text),
		}
	}

	async fn start_zotero(&self, sidecar: &Sidecar) -> Result<(), String> {
		let _guard = self.launching.lock().await;
		if zotero::Client::is_listening(sidecar.port).await {
			return Ok(());
		}
		let Some(launch) = &sidecar.launch else {
			return Err("Zotero isn't running. Ask the user to open Zotero.".into());
		};
		spawn_detached(&launch.command, &launch.args)
			.map_err(|e| format!("Zotero isn't running, and starting it failed ({e}). Ask the user to open Zotero."))?;
		let deadline = tokio::time::Instant::now() + LAUNCH_TIMEOUT;
		while tokio::time::Instant::now() < deadline {
			if zotero::Client::is_listening(sidecar.port).await {
				return Ok(());
			}
			tokio::time::sleep(Duration::from_millis(250)).await;
		}
		Err("Zotero was started but didn't become ready. Ask the user to check that Zotero is open.".into())
	}
}

fn spawn_detached(command: &str, args: &[String]) -> std::io::Result<()> {
	let mut cmd = std::process::Command::new(command);
	cmd.args(args)
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null());
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
		_context: RequestContext<RoleServer>,
	) -> Result<CallToolResponse, McpError> {
		let result = match self.forward(&request).await {
			Ok(result) => result,
			Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
		};
		Ok(result.into())
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
	let profile_dir = options
		.profile_dir
		.or_else(|| profile::default_profile(&profile::profile_root()?));
	if profile_dir.is_none() {
		tracing::info!("no Zotero profile found; tool calls will say Zotero isn't set up");
	}

	let server = Arc::new(Server {
		tools,
		profile_dir,
		client: zotero::Client::new(options.client_id, call_timeout),
		launching: Mutex::new(()),
	});
	let service = server
		.serve(rmcp::transport::stdio())
		.await
		.map_err(|e| StartupError::Mcp(Box::new(e)))?;
	service.waiting().await?;
	Ok(())
}
