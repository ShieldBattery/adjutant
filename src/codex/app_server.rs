//! Bounded, private stdio client for the pinned Codex app-server protocol.
//!
//! Only this module constructs requests. Staff text cannot select an RPC method, turn,
//! model, or sandbox. The inspector retains emitted data within the shared output budget.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{Instrument, info};
use uuid::Uuid;

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

enum Frame {
    Line(String),
    Oversized,
    Eof,
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
    let mut session = Session::new(runner, run_id, stdin, &mut frames, budget);
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
        }
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
            "capabilities":{"experimentalApi":false,"requestAttestation":false},
        })).await?.require_success()?;
        write_frame(&mut self.stdin, &json!({"method":"initialized"})).await?;
        let mut params = json!({"cwd":cwd,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true,"serviceName":"adjutant"});
        if let Some(model) = &self.runner.config.codex_model {
            params["model"] = json!(model);
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
                let frame = self
                    .frames
                    .recv()
                    .await
                    .context("Codex app-server stdout closed during RPC")?;
                if let Some((reply_id, reply)) = self.handle_frame(frame).await? {
                    if reply_id != id {
                        bail!("Codex app-server replied to an unexpected request");
                    }
                    return Ok(reply);
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
                self.deny_request(id, method).await?;
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
