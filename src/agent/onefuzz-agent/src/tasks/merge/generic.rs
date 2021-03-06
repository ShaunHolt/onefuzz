// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tasks::{
    config::{CommonConfig, SyncedDir},
    heartbeat::HeartbeatSender,
    utils,
};
use anyhow::Result;
use onefuzz::{expand::Expand, fs::set_executable};
use reqwest::Url;
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use storage_queue::{QueueClient, EMPTY_QUEUE_DELAY};
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct QueueMessage {
    content_length: u32,

    url: Url,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub supervisor_exe: String,
    pub supervisor_options: Vec<String>,
    pub supervisor_env: HashMap<String, String>,
    pub supervisor_input_marker: String,
    pub target_exe: PathBuf,
    pub target_options: Vec<String>,
    pub target_options_merge: bool,
    pub tools: SyncedDir,
    pub input_queue: Url,
    pub inputs: SyncedDir,
    pub unique_inputs: SyncedDir,

    #[serde(flatten)]
    pub common: CommonConfig,
}

pub async fn spawn(config: Arc<Config>) -> Result<()> {
    utils::init_dir(&config.tools.path).await?;
    utils::sync_remote_dir(&config.tools, utils::SyncOperation::Pull).await?;
    set_executable(&config.tools.path).await?;

    utils::init_dir(&config.unique_inputs.path).await?;
    let hb_client = config.common.init_heartbeat();
    loop {
        hb_client.alive();
        let tmp_dir = PathBuf::from("./tmp");
        verbose!("tmp dir reset");
        utils::reset_tmp_dir(&tmp_dir).await?;
        utils::sync_remote_dir(&config.unique_inputs, utils::SyncOperation::Pull).await?;
        let mut queue = QueueClient::new(config.input_queue.clone());
        if let Some(msg) = queue.pop().await? {
            let input_url = match utils::parse_url_data(msg.data()) {
                Ok(url) => url,
                Err(err) => {
                    error!("could not parse input URL from queue message: {}", err);
                    return Ok(());
                }
            };

            if let Err(error) = process_message(config.clone(), &input_url, &tmp_dir).await {
                error!(
                    "failed to process latest message from notification queue: {}",
                    error
                );
            } else {
                verbose!("will delete popped message with id = {}", msg.id());

                queue.delete(msg).await?;

                verbose!(
                    "Attempting to delete {} from the candidate container",
                    input_url.clone()
                );

                if let Err(e) = try_delete_blob(input_url.clone()).await {
                    error!("Failed to delete blob {}", e)
                }
            }
        } else {
            warn!("no new candidate inputs found, sleeping");
            tokio::time::delay_for(EMPTY_QUEUE_DELAY).await;
        }
    }
}

async fn process_message(config: Arc<Config>, input_url: &Url, tmp_dir: &PathBuf) -> Result<()> {
    let input_path = utils::download_input(input_url.clone(), &config.unique_inputs.path).await?;
    info!("downloaded input to {}", input_path.display());

    info!("Merging corpus");
    match merge(&config, tmp_dir).await {
        Ok(_) => {
            // remove the 'queue' folder
            let mut queue_dir = tmp_dir.clone();
            queue_dir.push("queue");
            let _delete_output = tokio::fs::remove_dir_all(queue_dir).await;
            let synced_dir = SyncedDir {
                path: tmp_dir.clone(),
                url: config.unique_inputs.url.clone(),
            };
            utils::sync_remote_dir(&synced_dir, utils::SyncOperation::Push).await?;
        }
        Err(e) => error!("Merge failed : {}", e),
    }
    Ok(())
}

async fn try_delete_blob(input_url: Url) -> Result<()> {
    let http_client = reqwest::Client::new();
    match http_client
        .delete(input_url)
        .send()
        .await?
        .error_for_status()
    {
        Ok(_) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

async fn merge(config: &Config, output_dir: impl AsRef<Path>) -> Result<()> {
    let mut supervisor_args = Expand::new();

    supervisor_args
        .input_marker(&config.supervisor_input_marker)
        .input_corpus(&config.unique_inputs.path)
        .target_options(&config.target_options)
        .supervisor_exe(&config.supervisor_exe)
        .supervisor_options(&config.supervisor_options)
        .generated_inputs(output_dir)
        .target_exe(&config.target_exe);

    if config.target_options_merge {
        supervisor_args.target_options(&config.target_options);
    }

    let supervisor_path = Expand::new()
        .tools_dir(&config.tools.path)
        .evaluate_value(&config.supervisor_exe)?;

    let mut cmd = Command::new(supervisor_path);

    cmd.kill_on_drop(true)
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for (k, v) in &config.supervisor_env {
        cmd.env(k, v);
    }

    for arg in supervisor_args.evaluate(&config.supervisor_options)? {
        cmd.arg(arg);
    }

    if !config.target_options_merge {
        for arg in supervisor_args.evaluate(&config.target_options)? {
            cmd.arg(arg);
        }
    }

    info!("Starting merge '{:?}'", cmd);
    cmd.spawn()?.wait_with_output().await?;
    Ok(())
}
