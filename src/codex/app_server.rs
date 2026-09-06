//! Bounded, private stdio client for the pinned Codex app-server protocol.
//!
//! Only this module constructs requests. Staff text cannot select an RPC method, turn,
//! model, or sandbox. The inspector retains emitted data within the shared output budget.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{Instrument, info, warn};
use uuid::Uuid;

use crate::evidence::{EvidenceKind, EvidenceRequests};

use super::steering::{SteerOutcome, SteerRequest, SteeringUpdate};
use super::{
    BoundedLine, BudgetDecision, CodexRunner, EventBudget, MAX_FINAL_REPORT_BYTES,
    MAX_LOG_LINE_BYTES, OutputLog, PipeTasks, ProcessGroupGuard, ToolActivityTracker,
    persist_budget_decision, public_progress_note, read_bounded_line, read_jsonl, tail_utf8,
    truncate_utf8,
};

const RPC_DEADLINE: Duration = Duration::from_secs(10);
const STARTUP_RPC_DEADLINE: Duration = Duration::from_secs(60);
const CLEANUP_DEADLINE: Duration = Duration::from_secs(2);
const STDOUT_QUEUE: usize = 8;
const MAX_PENDING_DYNAMIC_TOOLS: usize = 4;
const MAX_DYNAMIC_TOOL_ATTEMPTS: usize = 16;
const MAX_DYNAMIC_REQUEST_ID_BYTES: usize = 256;
const MAX_DYNAMIC_CALL_ID_BYTES: usize = 256;
const MAX_DYNAMIC_RECEIPT_BYTES: usize = 16 * 1024;

enum Frame {
    Line(String),
    Oversized,
    Eof,
}

#[derive(Clone, Copy)]
enum DynamicEvidenceTool {
    BugReport,
    GameArtifacts,
}

impl DynamicEvidenceTool {
    const fn name(self) -> &'static str {
        match self {
            Self::BugReport => "request_bug_report",
            Self::GameArtifacts => "request_game_artifacts",
        }
    }

    const fn argument_name(self) -> &'static str {
        match self {
            Self::BugReport => "report_id",
            Self::GameArtifacts => "game_id",
        }
    }

    const fn evidence_kind(self) -> EvidenceKind {
        match self {
            Self::BugReport => EvidenceKind::BugReport,
            Self::GameArtifacts => EvidenceKind::GameArtifacts,
        }
    }
}

struct DynamicToolCompletion {
    request_id: Value,
    call_id: String,
    tool: DynamicEvidenceTool,
    receipt: Result<Value, String>,
}

fn dynamic_tools() -> Value {
    json!([
        {
            "type": "function",
            "name": "request_bug_report",
            "description": "Collect ShieldBattery bug report metadata and available bounded client ZIP logs into this investigation's evidence workspace. The receipt provides the UUID, local evidence and manifest paths, file count, and cached status for repeats.",
            "inputSchema": {
                "type": "object",
                "properties": {"report_id": {"type": "string", "format": "uuid"}},
                "required": ["report_id"],
                "additionalProperties": false,
            },
        },
        {
            "type": "function",
            "name": "request_game_artifacts",
            "description": "Collect the available map file, replays, flight recordings, and bounded artifact metadata for one ShieldBattery game into this investigation's evidence workspace. The receipt provides the UUID, local evidence and manifest paths, file count, and cached status for repeats.",
            "inputSchema": {
                "type": "object",
                "properties": {"game_id": {"type": "string", "format": "uuid"}},
                "required": ["game_id"],
                "additionalProperties": false,
            },
        },
    ])
}

fn parse_dynamic_tool(
    params: &Value,
) -> std::result::Result<(DynamicEvidenceTool, Uuid, &str), &'static str> {
    if params.get("namespace") != Some(&Value::Null) {
        return Err("dynamic evidence request used an unsupported namespace");
    }
    let tool_name = params
        .get("tool")
        .and_then(Value::as_str)
        .ok_or("dynamic evidence request did not name a supported tool")?;
    let tool = match tool_name {
        "request_bug_report" => DynamicEvidenceTool::BugReport,
        "request_game_artifacts" => DynamicEvidenceTool::GameArtifacts,
        _ => return Err("dynamic evidence request named an unsupported tool"),
    };
    let arguments = params
        .get("arguments")
        .and_then(Value::as_object)
        .ok_or("dynamic evidence request had invalid arguments")?;
    let id = arguments
        .get(tool.argument_name())
        .and_then(Value::as_str)
        .ok_or("dynamic evidence request had invalid arguments")?;
    if arguments.len() != 1 {
        return Err("dynamic evidence request had invalid arguments");
    }
    let id =
        Uuid::parse_str(id).map_err(|_| "dynamic evidence request had an invalid identifier")?;
    let call_id = params
        .get("callId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= MAX_DYNAMIC_CALL_ID_BYTES)
        .ok_or("dynamic evidence request had an invalid call identifier")?;
    Ok((tool, id, call_id))
}

#[derive(Clone, Copy)]
enum Method {
    Initialize,
    ThreadStart,
    TurnStart,
    TurnSteer,
}

impl Method {
    const fn deadline(self) -> Duration {
        match self {
            // Thread initialization can wait for the reviewed required MCP startup timeouts.
            Self::Initialize | Self::ThreadStart => STARTUP_RPC_DEADLINE,
            Self::TurnStart | Self::TurnSteer => RPC_DEADLINE,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::ThreadStart => "thread/start",
            Self::TurnStart => "turn/start",
            Self::TurnSteer => "turn/steer",
        }
    }
}

enum RpcReply {
    Success(Value),
    Rejected(Value),
}

impl RpcReply {
    fn require_success(self) -> Result<Value> {
        match self {
            Self::Success(value) => Ok(value),
            Self::Rejected(error) => bail!(
                "Codex app-server rejected request: {}",
                error_summary(&error)
            ),
        }
    }
}

#[derive(Default)]
struct State {
    thread_id: Option<String>,
    turn_id: Option<String>,
    starting_turn: bool,
    completed: Option<Value>,
    final_answer: Option<String>,
    fallback_answer: Option<String>,
}

impl State {
    fn set_turn(&mut self, id: &str) -> Result<()> {
        if id.is_empty() || id.len() > 256 {
            bail!("Codex app-server returned an invalid turn ID");
        }
        if self.turn_id.as_deref().is_some_and(|known| known != id) {
            bail!("Codex app-server returned a different turn than the one it started");
        }
        self.turn_id = Some(id.to_owned());
        Ok(())
    }

    fn target(&self, params: &Value) -> bool {
        self.thread_id
            .as_deref()
            .is_some_and(|thread| params.get("threadId").and_then(Value::as_str) == Some(thread))
            && self
                .turn_id
                .as_deref()
                .is_some_and(|turn| notification_turn_id(params) == Some(turn))
    }

    fn observe(&mut self, method: &str, params: &Value) -> Result<()> {
        // Codex can emit the entire turn before replying to turn/start. This is the sole turn
        // requested on a fresh thread; require its eventual RPC response to confirm the same ID.
        if self.starting_turn
            && self.turn_id.is_none()
            && self.thread_id.as_deref().is_some_and(|thread| {
                params.get("threadId").and_then(Value::as_str) == Some(thread)
            })
            && matches!(
                method,
                "turn/started" | "turn/completed" | "item/started" | "item/completed"
            )
            && let Some(turn) = notification_turn_id(params)
        {
            self.set_turn(turn)?;
        }
        if !self.target(params) {
            return Ok(());
        }
        if method == "item/completed"
            && params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage")
        {
            let item = &params["item"];
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                if text.len() as u64 > MAX_FINAL_REPORT_BYTES {
                    bail!("Codex final answer exceeded {MAX_FINAL_REPORT_BYTES} bytes");
                }
                // Progress and commentary are never a substitute for a final diagnosis.
                if !text.trim().is_empty() && !text.trim_start().starts_with("ADJUTANT_PROGRESS:") {
                    match item.get("phase").and_then(Value::as_str) {
                        Some("final_answer") => self.final_answer = Some(text.to_owned()),
                        None => self.fallback_answer = Some(text.to_owned()),
                        _ => {}
                    }
                }
            }
        }
        if method == "turn/completed" {
            self.completed = Some(params["turn"].clone());
        }
        Ok(())
    }

    fn report(&self) -> Result<String> {
        let turn = self
            .completed
            .as_ref()
            .context("Codex app-server did not complete a turn")?;
        if turn.get("status").and_then(Value::as_str) != Some("completed") {
            bail!(
                "Codex app-server turn failed or was interrupted: {}",
                error_summary(&turn["error"])
            );
        }
        self.final_answer
            .clone()
            .or_else(|| self.fallback_answer.clone())
            .context("Codex app-server completed without a final agent message")
    }
}

fn command(runner: &CodexRunner) -> Command {
    let mut command = runner.base_command();
    // Profile is a top-level flag. Model is passed to thread/start; omitting it preserves config.
    if let Some(profile) = &runner.config.codex_profile {
        command.arg("--profile").arg(profile);
    }
    command
        .arg("app-server")
        .arg("--stdio")
        .arg("--strict-config");
    command
}

pub(super) async fn run(
    runner: &CodexRunner,
    run_id: Uuid,
    conversation_id: Uuid,
    working_directory: &Path,
    prompt: &str,
    requests: EvidenceRequests,
) -> Result<String> {
    let mut child = command(runner)
        .spawn()
        .context("failed to start Codex app-server")?;
    let mut process_group =
        ProcessGroupGuard::new(child.id().context("Codex app-server has no process ID")?);
    let stdin = child
        .stdin
        .take()
        .context("Codex app-server stdin was not piped")?;
    let stdout = child
        .stdout
        .take()
        .context("Codex app-server stdout was not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("Codex app-server stderr was not piped")?;
    let budget = Arc::new(EventBudget::new(&runner.config));
    let (sender, mut frames) = mpsc::channel(STDOUT_QUEUE);
    // PipeTasks aborts both readers on every drop path, including an outer timeout or panic.
    let pipes = PipeTasks::new(
        tokio::spawn(
            stdout_pump(BufReader::new(stdout), sender, Arc::clone(&budget)).in_current_span(),
        ),
        tokio::spawn(
            read_jsonl(
                BufReader::new(stderr),
                runner.store.clone(),
                run_id,
                true,
                Arc::clone(&budget),
            )
            .in_current_span(),
        ),
    );
    let mut session =
        Session::new(runner, run_id, stdin, &mut frames, budget).with_evidence(requests);
    let result = session
        .investigate(conversation_id, working_directory, prompt)
        .await;
    drop(session);
    // Close the receiver too: a pump blocked on a full queue must be able to finish after kill.
    drop(frames);
    process_group.terminate();
    let cleanup = tokio::time::timeout(CLEANUP_DEADLINE, async {
        let _ = child.kill().await;
        let _ = child.wait().await;
        pipes.finish().await
    })
    .await;
    match (result, cleanup) {
        (Err(error), Ok(Ok((_, stderr)))) if !stderr.trim().is_empty() => {
            Err(error.context(format!(
                "Codex app-server stderr: {}",
                tail_utf8(stderr.trim(), MAX_LOG_LINE_BYTES)
            )))
        }
        (Ok(_), Ok(Err(error))) => Err(error.context("failed to finish Codex output capture")),
        (Ok(_), Err(error)) => {
            Err(anyhow::Error::new(error).context("Codex output cleanup deadline exceeded"))
        }
        (result, _) => result,
    }
}

struct Session<'a, W> {
    runner: &'a CodexRunner,
    run_id: Uuid,
    stdin: W,
    frames: &'a mut mpsc::Receiver<Frame>,
    budget: Arc<EventBudget>,
    state: State,
    tools: ToolActivityTracker,
    output_log: OutputLog,
    next_id: u64,
    evidence: Option<EvidenceRequests>,
    dynamic_jobs: JoinSet<DynamicToolCompletion>,
    dynamic_call_ids: HashSet<String>,
    dynamic_attempts: usize,
}

impl<'a, W: AsyncWrite + Unpin> Session<'a, W> {
    fn new(
        runner: &'a CodexRunner,
        run_id: Uuid,
        stdin: W,
        frames: &'a mut mpsc::Receiver<Frame>,
        budget: Arc<EventBudget>,
    ) -> Self {
        Self {
            runner,
            run_id,
            stdin,
            frames,
            budget,
            state: State::default(),
            tools: ToolActivityTracker::default(),
            output_log: OutputLog::default(),
            next_id: 0,
            evidence: None,
            dynamic_jobs: JoinSet::new(),
            dynamic_call_ids: HashSet::new(),
            dynamic_attempts: 0,
        }
    }

    fn with_evidence(mut self, evidence: EvidenceRequests) -> Self {
        self.evidence = Some(evidence);
        self
    }

    async fn investigate(
        &mut self,
        conversation_id: Uuid,
        cwd: &Path,
        prompt: &str,
    ) -> Result<String> {
        tokio::time::timeout(
            self.runner.config.job_timeout,
            self.investigate_inner(conversation_id, cwd, prompt),
        )
        .await
        .context("Codex app-server exceeded the investigation deadline")?
    }

    async fn investigate_inner(
        &mut self,
        conversation_id: Uuid,
        cwd: &Path,
        prompt: &str,
    ) -> Result<String> {
        self.call(Method::Initialize, json!({
            "clientInfo":{"name":"adjutant","title":"Adjutant","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":self.evidence.is_some(),"requestAttestation":false},
        })).await?.require_success()?;
        write_frame(&mut self.stdin, &json!({"method":"initialized"})).await?;
        let mut params = json!({"cwd":cwd,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true,"serviceName":"adjutant"});
        if let Some(model) = &self.runner.config.codex_model {
            params["model"] = json!(model);
        }
        if self.evidence.is_some() {
            params["dynamicTools"] = dynamic_tools();
        }
        let thread = self
            .call(Method::ThreadStart, params)
            .await?
            .require_success()?;
        validate_thread_security(&thread)?;
        let thread_id = thread
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .context("Codex app-server returned an invalid thread ID")?;
        self.state.thread_id = Some(thread_id.to_owned());
        // Publish a bounded mailbox while turn/start is pending. Updates wait until its response
        // confirms the turn ID; the startup RPC is bounded by 10s, below mailbox confirmation's 15s.
        // Failed or already-completed startup drops pending requests as safely undelivered.
        let (_active, mut updates) = self
            .runner
            .steering
            .register(conversation_id, self.run_id)?;
        self.state.starting_turn = true;
        let turn = self.call(Method::TurnStart, json!({
            "threadId":self.state.thread_id,
            "input":[{"type":"text","text":prompt}],
            "approvalPolicy":"never","sandboxPolicy":{"type":"readOnly","networkAccess":false},
        })).await?.require_success()?;
        let turn_id = turn
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("Codex app-server turn/start response had no turn ID")?;
        self.state.set_turn(turn_id)?;
        self.state.starting_turn = false;
        if self.state.completed.is_some() {
            return self.state.report();
        }
        if turn
            .pointer("/turn/status")
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "failed" | "interrupted"))
        {
            bail!(
                "Codex app-server could not start the turn: {}",
                error_summary(&turn["turn"]["error"])
            );
        }
        loop {
            tokio::select! {
                frame = self.frames.recv() => {
                    if self.handle_frame(frame.context("Codex app-server stdout closed")?).await?.is_some() {
                        bail!("Codex app-server sent an unsolicited response");
                    }
                }
                request = updates.recv() => {
                    let Some(request) = request else { bail!("Codex steering mailbox unexpectedly closed"); };
                    if !request.is_cancelled() { self.steer(request).await?; }
                }
                completion = self.dynamic_jobs.join_next(), if !self.dynamic_jobs.is_empty() => {
                    self.finish_dynamic_tool(completion).await?;
                }
            }
            if self.state.completed.is_some() {
                return self.state.report();
            }
        }
    }

    async fn steer(&mut self, mut request: SteerRequest) -> Result<()> {
        if self.state.completed.is_some() {
            request.complete(SteerOutcome::Unavailable);
            return Ok(());
        }
        self.audit_steer(&request.update, "requested").await?;
        if request.is_cancelled() {
            return Ok(());
        }
        let params = json!({
            "threadId":self.state.thread_id,"expectedTurnId":self.state.turn_id,
            "clientUserMessageId":format!("discord:{}", request.update.message_id),
            "input":[{"type":"text","text":request.update.to_prompt()}],
        });
        request.mark_dispatched();
        let result = self.call(Method::TurnSteer, params).await;
        let (outcome, failure) = match result {
            Ok(RpcReply::Success(value))
                if value.get("turnId").and_then(Value::as_str) == self.state.turn_id.as_deref() =>
            {
                (SteerOutcome::Accepted, None)
            }
            Ok(RpcReply::Rejected(_)) => (SteerOutcome::Unavailable, None),
            Ok(RpcReply::Success(_)) => (
                SteerOutcome::Uncertain,
                Some(anyhow::anyhow!(
                    "Codex app-server acknowledged a different steering turn"
                )),
            ),
            Err(error) => (SteerOutcome::Uncertain, Some(error)),
        };
        // Completion may precede this RPC response. A matching success still means accepted;
        // treating that race as rejection would queue the same input a second time.
        let outcome_name = match outcome {
            SteerOutcome::Accepted => "accepted",
            SteerOutcome::Unavailable => "rejected",
            _ => "uncertain",
        };
        let audit = self.audit_steer(&request.update, outcome_name).await;
        request.complete(outcome);
        info!(run_id = %self.run_id, outcome = outcome_name, "Codex staff update delivery");
        audit?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }

    fn validate_dynamic_target(&mut self, params: &Value) -> Result<()> {
        if self.state.completed.is_some() {
            bail!("Codex app-server dynamic tool request arrived after the turn completed");
        }
        let thread = params
            .get("threadId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= MAX_DYNAMIC_REQUEST_ID_BYTES)
            .context("Codex app-server dynamic tool request had an invalid thread ID")?;
        if self.state.thread_id.as_deref() != Some(thread) {
            bail!("Codex app-server dynamic tool request targeted a different thread");
        }
        let turn = params
            .get("turnId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= MAX_DYNAMIC_REQUEST_ID_BYTES)
            .context("Codex app-server dynamic tool request had an invalid turn ID")?;
        if self.state.starting_turn && self.state.turn_id.is_none() {
            self.state.set_turn(turn)?;
        }
        if self.state.turn_id.as_deref() != Some(turn) {
            bail!("Codex app-server dynamic tool request targeted a different turn");
        }
        Ok(())
    }

    async fn respond_dynamic_failure(&mut self, request_id: &Value, message: &str) -> Result<()> {
        write_frame(
            &mut self.stdin,
            &json!({"id":request_id,"result":{"contentItems":[{"type":"inputText","text":message}],"success":false}}),
        )
        .await
    }

    async fn start_dynamic_tool(&mut self, request_id: &Value, params: &Value) -> Result<()> {
        let request_id_is_valid = match request_id {
            Value::String(id) => !id.is_empty() && id.len() <= MAX_DYNAMIC_REQUEST_ID_BYTES,
            Value::Number(id) => id.as_i64().is_some() || id.as_u64().is_some(),
            _ => false,
        };
        if !request_id_is_valid {
            bail!("Codex app-server dynamic tool request had an invalid ID");
        }
        if self.state.completed.is_some() {
            self.respond_dynamic_failure(
                request_id,
                "dynamic evidence request arrived after the active turn completed",
            )
            .await?;
            return Ok(());
        }
        if let Err(error) = self.validate_dynamic_target(params) {
            self.respond_dynamic_failure(
                request_id,
                "dynamic evidence request did not match the active investigation",
            )
            .await?;
            return Err(error);
        }
        if self.dynamic_attempts >= MAX_DYNAMIC_TOOL_ATTEMPTS {
            self.respond_dynamic_failure(request_id, "dynamic evidence request limit reached")
                .await?;
            return Ok(());
        }
        self.dynamic_attempts += 1;
        let (tool, id, call_id) = match parse_dynamic_tool(params) {
            Ok(call) => call,
            Err(message) => {
                self.respond_dynamic_failure(request_id, message).await?;
                return Ok(());
            }
        };
        if !self.dynamic_call_ids.insert(call_id.to_owned()) {
            self.respond_dynamic_failure(
                request_id,
                "dynamic evidence request was already handled",
            )
            .await?;
            return Ok(());
        }
        if self.dynamic_jobs.len() >= MAX_PENDING_DYNAMIC_TOOLS {
            self.respond_dynamic_failure(
                request_id,
                "too many dynamic evidence requests are pending",
            )
            .await?;
            return Ok(());
        }
        let Some(requests) = self.evidence.clone() else {
            self.respond_dynamic_failure(request_id, "dynamic evidence collection is unavailable")
                .await?;
            return Ok(());
        };
        let request_id = request_id.clone();
        let call_id = call_id.to_owned();
        self.dynamic_jobs.spawn(async move {
            let receipt = requests
                .request(tool.evidence_kind(), id)
                .await
                .and_then(|receipt| serde_json::to_value(receipt).map_err(Into::into))
                .map_err(|error| {
                    truncate_utf8(&format!("{error:#}"), MAX_LOG_LINE_BYTES).to_owned()
                });
            DynamicToolCompletion {
                request_id,
                call_id,
                tool,
                receipt,
            }
        });
        Ok(())
    }

    async fn finish_dynamic_tool(
        &mut self,
        completion: Option<Result<DynamicToolCompletion, tokio::task::JoinError>>,
    ) -> Result<()> {
        let completion = completion
            .context("Codex dynamic evidence task set closed unexpectedly")?
            .context("Codex dynamic evidence task failed")?;
        let response = match completion.receipt {
            Ok(receipt) => match self.evidence.as_ref() {
                Some(requests) => match requests.manifest_json().await {
                    Ok(manifest) => {
                        if let Err(error) = self
                            .runner
                            .store
                            .set_evidence_manifest(self.run_id, &manifest)
                            .await
                        {
                            warn!(run_id = %self.run_id, call_id = %completion.call_id, tool = completion.tool.name(), error = %truncate_utf8(&error.to_string(), MAX_LOG_LINE_BYTES), "could not persist dynamic evidence manifest");
                            json!({"contentItems":[{"type":"inputText","text":"dynamic evidence collection could not be recorded"}],"success":false})
                        } else {
                            match serde_json::to_string(&receipt) {
                                Ok(text) if text.len() <= MAX_DYNAMIC_RECEIPT_BYTES => {
                                    json!({"contentItems":[{"type":"inputText","text":text}],"success":true})
                                }
                                Ok(_) | Err(_) => {
                                    json!({"contentItems":[{"type":"inputText","text":"dynamic evidence receipt exceeded its safe size limit"}],"success":false})
                                }
                            }
                        }
                    }
                    Err(error) => {
                        warn!(run_id = %self.run_id, call_id = %completion.call_id, tool = completion.tool.name(), error = %truncate_utf8(&error.to_string(), MAX_LOG_LINE_BYTES), "could not serialize dynamic evidence manifest");
                        json!({"contentItems":[{"type":"inputText","text":"dynamic evidence collection could not be recorded"}],"success":false})
                    }
                },
                None => {
                    json!({"contentItems":[{"type":"inputText","text":"dynamic evidence collection is unavailable"}],"success":false})
                }
            },
            Err(error) => {
                warn!(run_id = %self.run_id, call_id = %completion.call_id, tool = completion.tool.name(), error = %error, "dynamic evidence collection failed");
                self.audit(
                    &json!({
                        "type":"adjutant.dynamic_evidence",
                        "outcome":"failed",
                        "tool":completion.tool.name(),
                        "error":error,
                    })
                    .to_string(),
                )
                .await?;
                json!({"contentItems":[{"type":"inputText","text":error}],"success":false})
            }
        };
        write_frame(
            &mut self.stdin,
            &json!({"id":completion.request_id,"result":response}),
        )
        .await
    }
    async fn call(&mut self, method: Method, params: Value) -> Result<RpcReply> {
        self.next_id += 1;
        let id = self.next_id;
        tokio::time::timeout(method.deadline(), async {
            write_frame(
                &mut self.stdin,
                &json!({"id":id,"method":method.name(),"params":params}),
            )
            .await?;
            loop {
                tokio::select! {
                    frame = self.frames.recv() => {
                        let frame = frame.context("Codex app-server stdout closed during RPC")?;
                        if let Some((reply_id, reply)) = self.handle_frame(frame).await? {
                            if reply_id != id {
                                bail!("Codex app-server replied to an unexpected request");
                            }
                            return Ok(reply);
                        }
                    }
                    completion = self.dynamic_jobs.join_next(), if !self.dynamic_jobs.is_empty() => {
                        self.finish_dynamic_tool(completion).await?;
                    }
                }
            }
        })
        .await
        .with_context(|| format!("Codex app-server {} deadline exceeded", method.name()))?
    }

    async fn handle_frame(&mut self, frame: Frame) -> Result<Option<(u64, RpcReply)>> {
        let line = match frame {
            Frame::Line(line) => line,
            Frame::Oversized => {
                persist_budget_decision(
                    &self.runner.store,
                    self.run_id,
                    &self.budget,
                    self.budget.oversized_line(),
                    "",
                )
                .await?;
                bail!(
                    "Codex app-server protocol line exceeded {} bytes",
                    self.budget.max_line_bytes
                );
            }
            Frame::Eof => bail!("Codex app-server closed stdout before protocol completion"),
        };
        // Also enforce the boundary when tests or future transports supply frames directly.
        if line.len() > self.budget.max_line_bytes {
            bail!("Codex app-server protocol line exceeded its byte limit");
        }
        let value: Value =
            serde_json::from_str(&line).context("Codex app-server emitted malformed JSON")?;
        let object = value
            .as_object()
            .context("Codex app-server frame was not an object")?;
        let accepted = self.audit_frame(&value).await?;
        if let Some(method) = object.get("method") {
            let method = method
                .as_str()
                .context("Codex app-server method was not a string")?;
            if let Some(id) = object.get("id") {
                if method == "item/tool/call" {
                    let params = object.get("params").unwrap_or(&Value::Null);
                    self.start_dynamic_tool(id, params).await?;
                } else {
                    self.deny_request(id, method).await?;
                }
            } else {
                let params = object.get("params").unwrap_or(&Value::Null);
                self.state.observe(method, params)?;
                if accepted && let Some(event) = normalize(method, params) {
                    self.output_log.observe("", false, Some(&event));
                    if self.state.target(params) {
                        if let Some(note) = public_progress_note(&event) {
                            self.runner
                                .store
                                .set_progress(self.run_id, Some(&note), None)
                                .await?;
                        }
                        if let Some(activity) = self.tools.observe(&event) {
                            self.runner
                                .store
                                .set_progress(self.run_id, None, Some(&activity))
                                .await?;
                        }
                    }
                }
            }
            return Ok(None);
        }
        let id = object
            .get("id")
            .and_then(Value::as_u64)
            .context("Codex app-server response lacked the expected numeric ID")?;
        let reply = match (object.get("result"), object.get("error")) {
            (Some(result), None) => RpcReply::Success(result.clone()),
            (None, Some(error)) => RpcReply::Rejected(error.clone()),
            _ => bail!("Codex app-server response must contain either result or error"),
        };
        Ok(Some((id, reply)))
    }

    async fn audit_frame(&self, value: &Value) -> Result<bool> {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("response");
        let event = json!({"type":"codex.app_server","method":method,"payload":value}).to_string();
        self.audit(&event).await
    }

    async fn audit_steer(&self, update: &SteeringUpdate, outcome: &str) -> Result<()> {
        self.audit(
            &json!({"type":"adjutant.steering","outcome":outcome,"update":update}).to_string(),
        )
        .await?;
        Ok(())
    }

    async fn audit(&self, event: &str) -> Result<bool> {
        let decision = self.budget.decide(event.len());
        let accepted = matches!(decision, BudgetDecision::Store);
        persist_budget_decision(
            &self.runner.store,
            self.run_id,
            &self.budget,
            decision,
            event,
        )
        .await?;
        Ok(accepted)
    }

    async fn deny_request(&mut self, id: &Value, method: &str) -> Result<()> {
        if !id.is_string() && !id.is_number() {
            bail!("Codex app-server request had an invalid ID");
        }
        let result = denial(method);
        let unsupported = result.is_none();
        let response = match result {
            Some(result) => json!({"id":id,"result":result}),
            None => {
                json!({"id":id,"error":{"code":-32601,"message":"unsupported unattended server request"}})
            }
        };
        write_frame(&mut self.stdin, &response).await?;
        if unsupported {
            bail!("Codex app-server requested an unsupported interactive capability");
        }
        Ok(())
    }
}

fn notification_turn_id(params: &Value) -> Option<&str> {
    params
        .get("turnId")
        .and_then(Value::as_str)
        .or_else(|| params.pointer("/turn/id").and_then(Value::as_str))
}

fn validate_thread_security(thread: &Value) -> Result<()> {
    if thread.get("approvalPolicy").and_then(Value::as_str) != Some("never")
        || thread.pointer("/thread/ephemeral").and_then(Value::as_bool) != Some(true)
        || thread.pointer("/sandbox/type").and_then(Value::as_str) != Some("readOnly")
        || thread
            .pointer("/sandbox/networkAccess")
            .and_then(Value::as_bool)
            != Some(false)
    {
        bail!(
            "Codex app-server did not confirm the required ephemeral, never-approval, read-only, network-disabled thread policy"
        );
    }
    Ok(())
}

fn normalize(method: &str, params: &Value) -> Option<Value> {
    let kind = match method {
        "thread/started" => "thread.started",
        "turn/started" => "turn.started",
        "turn/completed" => "turn.completed",
        "item/started" => "item.started",
        "item/completed" => "item.completed",
        "error" => {
            return Some(
                json!({"type":"error", "message":params.pointer("/error/message").and_then(Value::as_str).unwrap_or("Codex app-server error")}),
            );
        }
        _ => return None,
    };
    let mut event = json!({"type":kind});
    if let Some(item) = params.get("item") {
        event["item"] = item.clone();
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .map(|value| match value {
                "agentMessage" => "agent_message",
                "commandExecution" => "command_execution",
                "mcpToolCall" => "mcp_tool_call",
                "dynamicToolCall" => "dynamic_tool_call",
                other => other,
            });
        if let Some(item_type) = item_type {
            event["item"]["type"] = json!(item_type);
        }
    }
    Some(event)
}

fn denial(method: &str) -> Option<Value> {
    Some(match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({"decision":"decline"})
        }
        "item/tool/requestUserInput" => json!({"answers":{}}),
        "mcpServer/elicitation/request" => json!({"action":"decline","content":null,"_meta":null}),
        "item/permissions/requestApproval" => json!({"permissions":{},"scope":"turn"}),
        "applyPatchApproval" | "execCommandApproval" => {
            json!({"decision":{"denied":{"rejection":"unattended read-only diagnostic"}}})
        }
        _ => return None,
    })
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    tokio::time::timeout(RPC_DEADLINE, async {
        writer.write_all(&bytes).await?;
        writer.flush().await
    })
    .await
    .context("Codex app-server input write deadline exceeded")?
    .context("failed writing Codex app-server protocol")
}

async fn stdout_pump<R: AsyncBufRead + Unpin>(
    mut reader: R,
    sender: mpsc::Sender<Frame>,
    budget: Arc<EventBudget>,
) -> Result<String> {
    loop {
        let frame = match read_bounded_line(&mut reader, budget.max_line_bytes).await? {
            Some(BoundedLine::Line(line)) => Frame::Line(line),
            Some(BoundedLine::Oversized) => Frame::Oversized,
            None => Frame::Eof,
        };
        let last = matches!(frame, Frame::Eof | Frame::Oversized);
        if sender.send(frame).await.is_err() || last {
            return Ok(String::new());
        }
    }
}

fn error_summary(value: &Value) -> String {
    let text = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no error detail");
    truncate_utf8(text, MAX_LOG_LINE_BYTES).to_owned()
}

#[cfg(test)]
mod tests;
