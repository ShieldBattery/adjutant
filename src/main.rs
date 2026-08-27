use std::sync::Arc;

use adjutant::codex::CodexRunner;
use adjutant::config::Config;
use adjutant::discord::{DiscordHandler, gateway_intents};
use adjutant::evidence::EvidenceCollector;
use adjutant::jobs;
use adjutant::store::Store;
use anyhow::{Context, Result, bail};
use serenity::Client;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    initialize_tracing();

    let config = Arc::new(Config::from_env().context("invalid Adjutant configuration")?);
    let store = Store::open(&config.database_path).await?;
    let recovered = store.recover_interrupted_runs().await?;
    if recovered > 0 {
        warn!(recovered, "marked interrupted diagnostic runs as failed");
    }
    let pruned = store.prune(config.run_retention_days).await?;
    if pruned > 0 {
        info!(pruned, "pruned expired diagnostic runs");
    }

    let collector = EvidenceCollector::new(Arc::clone(&config))?;
    let runner = CodexRunner::new(Arc::clone(&config), store.clone());
    let (queue, job_handle) = jobs::start(
        config.max_queued_jobs,
        config.max_concurrent_jobs,
        store.clone(),
        collector,
        runner,
    );
    let handler = DiscordHandler::new(Arc::clone(&config), store.clone(), queue.clone());
    let mut client = Client::builder(&config.discord_token, gateway_intents())
        .event_handler(handler)
        .await
        .context("failed to create Discord client")?;
    let shard_manager = Arc::clone(&client.shard_manager);

    let (ui_shutdown_sender, ui_shutdown_receiver) = oneshot::channel();
    let (service_sender, mut service_receiver) = mpsc::unbounded_channel::<String>();
    let ui_store = store.clone();
    let ui_bind = config.ui_bind;
    let ui_token = config.ui_token.clone();
    let ui_service_sender = service_sender.clone();
    let ui_handle = tokio::spawn(async move {
        let result = adjutant::web::serve(ui_store, ui_bind, ui_token, async {
            let _ = ui_shutdown_receiver.await;
        })
        .await;
        let summary = result.as_ref().map_or_else(
            |error| format!("inspection UI stopped: {error:#}"),
            |()| "inspection UI stopped unexpectedly".to_owned(),
        );
        let _ = ui_service_sender.send(summary);
        result
    });

    let discord_service_sender = service_sender.clone();
    let discord_handle = tokio::spawn(async move {
        let result = client.start().await.context("Discord client stopped");
        let summary = result.as_ref().map_or_else(
            |error| format!("Discord service stopped: {error:#}"),
            |()| "Discord service stopped unexpectedly".to_owned(),
        );
        let _ = discord_service_sender.send(summary);
        result
    });
    drop(service_sender);

    info!(bind = %config.ui_bind, "Adjutant started");
    let service_failure = tokio::select! {
        result = shutdown_signal() => {
            result?;
            info!("shutdown requested");
            None
        }
        failure = service_receiver.recv() => failure,
    };

    shard_manager.shutdown_all().await;
    let _ = ui_shutdown_sender.send(());
    let discord_result = discord_handle.await.context("Discord task panicked")?;
    let ui_result = ui_handle.await.context("inspection UI task panicked")?;
    drop(queue);
    job_handle.await.context("job queue task panicked")?;
    store.close().await;

    if let Some(failure) = service_failure {
        bail!(failure);
    }
    discord_result?;
    ui_result?;
    info!("Adjutant stopped cleanly");
    Ok(())
}

fn initialize_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("adjutant=info,serenity=warn"));
    if std::env::var("LOG_FORMAT").is_ok_and(|value| value.eq_ignore_ascii_case("json")) {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {},
    }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}
