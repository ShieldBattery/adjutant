use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::*;
use crate::config::Config;
use crate::store::{NewRun, RunKind, Store};

struct Harness {
    _directory: tempfile::TempDir,
    runner: Arc<CodexRunner>,
    run_id: Uuid,
}

async fn harness(timeout: Duration, events: usize, bytes: usize) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("runs.sqlite3"))
        .await
        .unwrap();
    let run = NewRun::new(
        RunKind::StaffRequest,
        "test".to_owned(),
        "test".to_owned(),
        None,
        1,
        2,
        3,
    );
    store.create_run(&run).await.unwrap();
    store.mark_running(run.id).await.unwrap();
    let config = Arc::new(Config {
        discord_token: "test".to_owned(),
        discord_guild_id: 1,
        discord_bug_report_channel_id: 2,
        discord_bug_report_webhook_id: 3,
        discord_request_channel_id: 4,
        discord_output_channel_id: 4,
        discord_mention_role_id: None,
        discord_allowed_role_ids: HashSet::new(),
        shieldbattery_public_url: "https://shieldbattery.invalid".parse().unwrap(),
        shieldbattery_internal_url: None,
        codex_bin: "codex".to_owned(),
        codex_home: PathBuf::from("codex-home"),
        codex_profile: None,
        codex_model: None,
        shieldbattery_source_dir: PathBuf::from("shieldbattery"),
        codex_env_passthrough: Vec::new(),
        max_concurrent_jobs: 1,
        max_queued_jobs: 1,
        max_download_bytes: 1,
        max_game_artifacts: 1,
        max_game_artifact_bytes: 1,
        max_game_evidence_bytes: 1,
        max_archive_files: 1,
        max_expanded_bytes: 1,
        max_codex_events: events,
        max_codex_event_bytes: bytes,
        max_codex_event_line_bytes: 4096,
        job_timeout: timeout,
        database_path: directory.path().join("runs.sqlite3"),
        ui_bind: "127.0.0.1:0".parse().unwrap(),
        ui_base_url: None,
        run_retention_days: 1,
    });
    Harness {
        _directory: directory,
        runner: Arc::new(CodexRunner::new(config, store)),
        run_id: run.id,
    }
}

async fn outgoing(reader: &mut BufReader<tokio::io::DuplexStream>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}
async fn incoming(sender: &mpsc::Sender<Frame>, value: Value) {
    sender.send(Frame::Line(value.to_string())).await.unwrap();
}
fn thread(id: &str) -> Value {
    json!({"thread":{"id":id,"ephemeral":true},"approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}})
}
fn turn(id: &str) -> Value {
    json!({"turn":{"id":id,"status":"inProgress","error":null}})
}
fn completion(thread: &str, turn: &str) -> Value {
    json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":"completed","error":null}}})
}

fn start(
    h: &Harness,
) -> (
    tokio::task::JoinHandle<anyhow::Result<String>>,
    BufReader<tokio::io::DuplexStream>,
    mpsc::Sender<Frame>,
    Uuid,
) {
    let (client, peer) = tokio::io::duplex(32 * 1024);
    let (tx, mut rx) = mpsc::channel(8);
    let runner = Arc::clone(&h.runner);
    let run_id = h.run_id;
    let conversation = Uuid::now_v7();
    let task = tokio::spawn(async move {
        let cwd = PathBuf::from(".");
        Session::new(
            &runner,
            run_id,
            client,
            &mut rx,
            Arc::new(EventBudget::new(&runner.config)),
        )
        .investigate(conversation, &cwd, "prompt")
        .await
    });
    (task, BufReader::new(peer), tx, conversation)
}

async fn establish(reader: &mut BufReader<tokio::io::DuplexStream>, tx: &mpsc::Sender<Frame>) {
    assert_eq!(outgoing(reader).await["method"], "initialize");
    incoming(tx, json!({"id":1,"result":{}})).await;
    assert_eq!(outgoing(reader).await["method"], "initialized");
    assert_eq!(outgoing(reader).await["method"], "thread/start");
    incoming(tx, json!({"id":2,"result":thread("thread-1")})).await;
    assert_eq!(outgoing(reader).await["method"], "turn/start");
}

#[tokio::test]
async fn accepts_final_answer_and_ignores_commentary_progress() {
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let (task, mut reader, tx, _) = start(&h);
    establish(&mut reader, &tx).await;
    incoming(&tx, json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"agentMessage","phase":"commentary","text":"ADJUTANT_PROGRESS: checking"}}})).await;
    incoming(&tx, json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"agentMessage","phase":"final_answer","text":"final diagnosis"}}})).await;
    incoming(&tx, json!({"id":3,"result":turn("turn-1")})).await;
    incoming(&tx, completion("thread-1", "turn-1")).await;
    assert_eq!(task.await.unwrap().unwrap(), "final diagnosis");
}

#[tokio::test]
async fn preserves_early_completion_before_turn_start_response() {
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let (task, mut reader, tx, _) = start(&h);
    establish(&mut reader, &tx).await;
    incoming(&tx, json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"agentMessage","phase":"final_answer","text":"early"}}})).await;
    incoming(&tx, completion("thread-1", "turn-1")).await;
    incoming(&tx, json!({"id":3,"result":turn("turn-1")})).await;
    assert_eq!(task.await.unwrap().unwrap(), "early");
}

#[tokio::test]
async fn deadline_covers_idle_startup() {
    let h = harness(Duration::from_millis(30), 100, 100_000).await;
    let (task, _reader, _tx, _) = start(&h);
    assert!(task.await.unwrap().is_err());
}

async fn active(
    h: &Harness,
) -> (
    tokio::task::JoinHandle<anyhow::Result<String>>,
    BufReader<tokio::io::DuplexStream>,
    mpsc::Sender<Frame>,
    Uuid,
) {
    let (task, mut reader, tx, conversation) = start(h);
    establish(&mut reader, &tx).await;
    incoming(&tx, json!({"id":3,"result":turn("turn-1")})).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while h.runner.steering.target(conversation) != Some(h.run_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    (task, reader, tx, conversation)
}

fn update() -> SteeringUpdate {
    SteeringUpdate {
        guild_id: 1,
        channel_id: 2,
        message_id: 91,
        author_id: 4,
        author: "staff".to_owned(),
        text: "actually, this started at 03:00 UTC. focus on reconnects.".to_owned(),
        source_url: "https://discord.invalid/channels/1/2/91".to_owned(),
    }
}

async fn finish(tx: &mpsc::Sender<Frame>, report: &str) {
    incoming(
        tx,
        json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1",
        "item":{"id":"answer","type":"agentMessage","phase":"final_answer","text":report}}}),
    )
    .await;
    incoming(tx, completion("thread-1", "turn-1")).await;
}

async fn submit_update(
    h: &Harness,
    reader: &mut BufReader<tokio::io::DuplexStream>,
) -> tokio::task::JoinHandle<SteerOutcome> {
    let runner = Arc::clone(&h.runner);
    let run = h.run_id;
    let sender = tokio::spawn(async move { runner.steer(run, update()).await });
    let request = tokio::time::timeout(Duration::from_secs(1), outgoing(reader))
        .await
        .unwrap();
    assert_eq!(request["method"], "turn/steer");
    assert_eq!(request["params"]["threadId"], "thread-1");
    assert_eq!(request["params"]["expectedTurnId"], "turn-1");
    assert_eq!(request["params"]["clientUserMessageId"], "discord:91");
    assert_eq!(request["params"]["input"][0]["text"], update().to_prompt());
    assert!(request["params"].get("sandboxPolicy").is_none());
    sender
}

#[tokio::test]
async fn steers_original_attributed_text_and_records_acceptance_in_the_same_run() {
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let (task, mut reader, tx, conversation) = active(&h).await;
    let sender = submit_update(&h, &mut reader).await;
    incoming(&tx, json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1",
        "item":{"type":"agentMessage","phase":"commentary","text":"ADJUTANT_PROGRESS: checked the reconnect path."}}})).await;
    incoming(&tx, json!({"id":4,"result":{"turnId":"turn-1"}})).await;
    assert_eq!(sender.await.unwrap(), SteerOutcome::Accepted);
    finish(&tx, "updated diagnosis").await;
    assert_eq!(task.await.unwrap().unwrap(), "updated diagnosis");
    assert_eq!(h.runner.steering.target(conversation), None);
    let events = h
        .runner
        .store
        .get_events(&h.run_id.to_string())
        .await
        .unwrap();
    let accepted = events
        .iter()
        .filter_map(|e| serde_json::from_str::<Value>(&e.event_json).ok())
        .find(|e| e["type"] == "adjutant.steering" && e["outcome"] == "accepted")
        .unwrap();
    assert_eq!(accepted["update"], serde_json::to_value(update()).unwrap());
    let progress = h
        .runner
        .store
        .get_progress(h.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        progress.note.as_deref(),
        Some("checked the reconnect path.")
    );
}

#[tokio::test]
async fn completion_before_steer_success_is_still_accepted_without_a_second_turn() {
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let (task, mut reader, tx, _) = active(&h).await;
    let sender = submit_update(&h, &mut reader).await;
    finish(&tx, "completed with update").await;
    incoming(&tx, json!({"id":4,"result":{"turnId":"turn-1"}})).await;
    assert_eq!(sender.await.unwrap(), SteerOutcome::Accepted);
    assert_eq!(task.await.unwrap().unwrap(), "completed with update");
    let mut remaining = String::new();
    reader.read_to_string(&mut remaining).await.unwrap();
    assert!(remaining.is_empty());
}

#[tokio::test]
async fn explicit_steer_rejection_allows_fallback_without_failing_the_original_run() {
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let (task, mut reader, tx, _) = active(&h).await;
    let sender = submit_update(&h, &mut reader).await;
    incoming(
        &tx,
        json!({"id":4,"error":{"code":-32600,"message":"no active turn to steer"}}),
    )
    .await;
    assert_eq!(sender.await.unwrap(), SteerOutcome::Unavailable);
    finish(&tx, "original diagnosis").await;
    assert_eq!(task.await.unwrap().unwrap(), "original diagnosis");
}

#[tokio::test]
async fn lost_or_mismatched_steer_confirmation_is_uncertain_never_safe_to_resubmit() {
    for response in [
        Frame::Eof,
        Frame::Line(json!({"id":4,"result":{"turnId":"other-turn"}}).to_string()),
    ] {
        let h = harness(Duration::from_secs(2), 100, 100_000).await;
        let (task, mut reader, tx, conversation) = active(&h).await;
        let sender = submit_update(&h, &mut reader).await;
        tx.send(response).await.unwrap();
        assert_eq!(sender.await.unwrap(), SteerOutcome::Uncertain);
        assert!(task.await.unwrap().is_err());
        assert_eq!(h.runner.steering.target(conversation), None);
    }
}

#[tokio::test]
async fn denies_string_id_approval_and_unknown_requests_without_hanging() {
    for (method, supported) in [
        ("item/commandExecution/requestApproval", true),
        ("unrecognized/writeAccess", false),
    ] {
        let h = harness(Duration::from_secs(2), 100, 100_000).await;
        let (task, mut reader, tx, _) = active(&h).await;
        incoming(&tx, json!({"id":"approval-1","method":method,"params":{}})).await;
        let response = outgoing(&mut reader).await;
        assert_eq!(response["id"], "approval-1");
        if supported {
            assert_eq!(response["result"]["decision"], "decline");
            finish(&tx, "read-only diagnosis").await;
            assert_eq!(task.await.unwrap().unwrap(), "read-only diagnosis");
        } else {
            assert_eq!(response["error"]["code"], -32601);
            assert!(task.await.unwrap().is_err());
        }
    }
}

#[tokio::test]
async fn exhausted_event_count_or_bytes_does_not_lose_control_or_final_answer() {
    for (events, bytes) in [(1, 100_000), (100, 1)] {
        let h = harness(Duration::from_secs(2), events, bytes).await;
        let (task, _, tx, _) = active(&h).await;
        finish(&tx, "final survives truncation").await;
        assert_eq!(task.await.unwrap().unwrap(), "final survives truncation");
        let stored = h
            .runner
            .store
            .get_events(&h.run_id.to_string())
            .await
            .unwrap();
        assert!(stored.len() <= events + 1);
        assert!(
            stored
                .iter()
                .any(|event| event.event_json.contains("adjutant.events_truncated"))
        );
    }
}

#[tokio::test]
async fn rejects_malformed_oversized_frames_and_mismatched_start_turn_ids() {
    for frame in [Frame::Line("not json".to_owned()), Frame::Oversized] {
        let h = harness(Duration::from_secs(2), 100, 100_000).await;
        let (task, _, tx, _) = active(&h).await;
        tx.send(frame).await.unwrap();
        assert!(task.await.unwrap().is_err());
    }
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let (task, mut reader, tx, _) = start(&h);
    establish(&mut reader, &tx).await;
    incoming(&tx, json!({"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"actual-turn"}}})).await;
    incoming(&tx, json!({"id":3,"result":turn("different-turn")})).await;
    assert!(
        task.await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("different turn")
    );
}

#[tokio::test]
async fn unrelated_turn_output_and_retry_errors_cannot_replace_the_final_answer() {
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let (task, _, tx, _) = active(&h).await;
    incoming(&tx, json!({"method":"error","params":{"threadId":"thread-1","turnId":"turn-1","willRetry":true,"error":{"message":"temporary failure"}}})).await;
    incoming(
        &tx,
        json!({"method":"item/completed","params":{"threadId":"another-thread","turnId":"turn-1",
        "item":{"type":"agentMessage","phase":"final_answer","text":"unrelated answer"}}}),
    )
    .await;
    incoming(&tx, completion("another-thread", "turn-1")).await;
    finish(&tx, "matching answer").await;
    assert_eq!(task.await.unwrap().unwrap(), "matching answer");
}

#[tokio::test]
async fn failed_turn_and_missing_final_answer_are_not_successes() {
    for failure in [true, false] {
        let h = harness(Duration::from_secs(2), 100, 100_000).await;
        let (task, _, tx, _) = active(&h).await;
        let mut done = completion("thread-1", "turn-1");
        if failure {
            done["params"]["turn"]["status"] = json!("failed");
        }
        incoming(&tx, done).await;
        assert!(task.await.unwrap().is_err());
    }
}

#[tokio::test]
async fn effective_policy_cannot_enable_network_writes_approvals_or_persistence() {
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let cmd = command(&h.runner);
    let args = cmd
        .as_std()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(args, ["app-server", "--stdio", "--strict-config"]);
    assert!(
        !cmd.as_std()
            .get_envs()
            .any(|(key, _)| key == "DISCORD_TOKEN")
    );
    assert!(validate_thread_security(&thread("thread-1")).is_ok());
    for pointer in [
        "/sandbox/networkAccess",
        "/sandbox/type",
        "/approvalPolicy",
        "/thread/ephemeral",
    ] {
        let mut policy = thread("thread-1");
        *policy.pointer_mut(pointer).unwrap() = Value::Null;
        assert!(validate_thread_security(&policy).is_err());
    }
}

#[tokio::test]
async fn update_arriving_before_turn_start_confirmation_waits_for_the_confirmed_turn() {
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let (task, mut reader, tx, conversation) = start(&h);
    establish(&mut reader, &tx).await;
    assert_eq!(h.runner.steering.target(conversation), Some(h.run_id));
    incoming(
        &tx,
        json!({"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1"}}}),
    )
    .await;
    let runner = Arc::clone(&h.runner);
    let run_id = h.run_id;
    let sender = tokio::spawn(async move { runner.steer(run_id, update()).await });
    incoming(&tx, json!({"id":3,"result":turn("turn-1")})).await;
    let request = outgoing(&mut reader).await;
    assert_eq!(request["method"], "turn/steer");
    assert_eq!(request["params"]["expectedTurnId"], "turn-1");
    incoming(&tx, json!({"id":4,"result":{"turnId":"turn-1"}})).await;
    assert_eq!(sender.await.unwrap(), SteerOutcome::Accepted);
    finish(&tx, "early correction included").await;
    assert_eq!(task.await.unwrap().unwrap(), "early correction included");
}

#[tokio::test]
async fn cancellation_after_dispatch_unregisters_the_run_without_claiming_non_delivery() {
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let (task, mut reader, _tx, conversation) = active(&h).await;
    let sender = submit_update(&h, &mut reader).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(sender.await.unwrap(), SteerOutcome::Uncertain);
    assert_eq!(h.runner.steering.target(conversation), None);
}

#[tokio::test]
async fn private_tool_audit_preserves_payload_while_public_activity_stays_generic() {
    let h = harness(Duration::from_secs(2), 100, 100_000).await;
    let (task, _, tx, _) = active(&h).await;
    let item = json!({"id":"tool-1","type":"mcpToolCall","server":"shieldbattery_database","tool":"query_database",
        "arguments":{"sql":"SELECT synthetic_private_value"},"result":{"value":"synthetic result"}});
    for method in ["item/started", "item/completed"] {
        incoming(
            &tx,
            json!({"method":method,"params":{"threadId":"thread-1","turnId":"turn-1","item":item}}),
        )
        .await;
    }
    finish(&tx, "summary").await;
    assert_eq!(task.await.unwrap().unwrap(), "summary");
    let audit = h
        .runner
        .store
        .get_events(&h.run_id.to_string())
        .await
        .unwrap();
    assert!(audit.iter().any(
        |event| event.event_json.contains("SELECT synthetic_private_value")
            && event.event_json.contains("synthetic result")
    ));
    let progress = h
        .runner
        .store
        .get_progress(h.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        progress.activity.as_deref(),
        Some("finished a diagnostic check")
    );
}
