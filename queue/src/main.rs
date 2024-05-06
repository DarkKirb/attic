use std::{
    collections::{HashMap, HashSet},
    fs,
    ops::Deref,
    sync::Arc,
};

use anyhow::Result;
use app_queue::{AppQueue, Job};
use async_trait::async_trait;
use attic::{
    cache::CacheName,
    nix_store::{NixStore, StorePath, StorePathHash, ValidPathInfo},
};
use attic_client::{api::ApiClient, cache::CacheRef, config::Config, push::upload_path};
use futures::future::join_all;
use indicatif::MultiProgress;
use log::{debug, info, warn};
use nix::{sys::stat::Mode, unistd::mkfifo};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::unix::pipe,
};

static NIX_STORE: Lazy<Arc<NixStore>> = Lazy::new(|| Arc::new(NixStore::connect().unwrap()));
static CACHE_NAME: Lazy<CacheName> = Lazy::new(|| CacheName::new("chir-rs".to_string()).unwrap());
static API: Lazy<ApiClient> = Lazy::new(|| {
    let config = Config::load().unwrap();
    let cache_ref = CacheRef::DefaultServer(CACHE_NAME.clone());
    let (_, server, _) = config.resolve_cache(&cache_ref).unwrap();
    ApiClient::from_server_config(server.clone()).unwrap()
});
static MULTI_PROGRESS: Lazy<MultiProgress> = Lazy::new(MultiProgress::new);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct QueuedInput {
    path: StorePath,
}

#[typetag::serde]
#[async_trait]
impl Job for QueuedInput {
    async fn run(&mut self, queue: Arc<AppQueue>) -> Result<()> {
        debug!("Starting to preprocess path: {:?}", self.path);

        let mut deps = NIX_STORE
            .compute_fs_closure(self.path.clone(), false, true, true)
            .await?;
        deps.push(self.path.clone());

        info!("Prepocess: {:?} has {} dependencies", self.path, deps.len());

        let mut store_path_map: HashMap<StorePathHash, ValidPathInfo> = {
            let futures = deps
                .iter()
                .map(|path| {
                    let path = path.clone();
                    let path_hash = path.to_hash();

                    async move {
                        let path_info = NIX_STORE.query_path_info(path).await?;
                        Ok((path_hash, path_info))
                    }
                })
                .collect::<Vec<_>>();

            join_all(futures).await.into_iter().collect::<Result<_>>()?
        };

        info!(
            "Fetched path info for {:?} ({} paths)",
            self.path,
            deps.len()
        );

        store_path_map.retain(|_, pi| {
            !pi.sigs
                .iter()
                .any(|sig| sig.starts_with("cache.nixos.org-1:"))
        });

        info!(
            "Non-upstream deps for {:?}: {}",
            self.path,
            store_path_map.len()
        );

        let missing_path_hashes: HashSet<StorePathHash> = {
            let store_path_hashes = store_path_map.keys().map(|sph| sph.to_owned()).collect();
            let res = API
                .get_missing_paths(&CacheName::new("chir-rs".to_string())?, store_path_hashes)
                .await?;
            res.missing_paths.into_iter().collect()
        };
        store_path_map.retain(|sph, _| missing_path_hashes.contains(sph));

        info!("Pre-processed path: {:?}", self.path);

        for dep in store_path_map.values() {
            let dep = &dep.path;
            let job_id = format!("fetch_path_info:{:?}", dep.as_os_str());
            queue
                .add_unique_job(job_id, Box::new(UploadPath { path: dep.clone() }))
                .await
                .ok();
        }
        Ok(())
    }

    fn is_fatal_error(&self, _: &anyhow::Error) -> bool {
        true
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct UploadPath {
    path: StorePath,
}

#[typetag::serde]
#[async_trait]
impl Job for UploadPath {
    async fn run(&mut self, _: Arc<AppQueue>) -> Result<()> {
        let path_info = match NIX_STORE.query_path_info(self.path.clone()).await {
            Ok(pi) => pi,
            Err(e) => {
                warn!("Path {:?} is not valid: {e:#?}. Skipping.", self.path);
                return Ok(());
            }
        };

        upload_path(
            path_info,
            Arc::clone(&*NIX_STORE),
            API.clone(),
            CACHE_NAME.deref(),
            MULTI_PROGRESS.clone(),
            false,
        )
        .await?;

        Ok(())
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum PathState {
    Queued,
    InProgress,
}

async fn enqueue_thread(queue: Arc<AppQueue>) -> Result<()> {
    let queue_path = std::env::var("QUEUE_PATH")?;

    fs::remove_file(&queue_path).ok();

    mkfifo(
        &queue_path[..],
        Mode::S_IRWXU | Mode::S_IWGRP | Mode::S_IWOTH,
    )?;

    let rx = pipe::OpenOptions::new()
        .read_write(true)
        .open_receiver(&queue_path)?;

    let mut lines = BufReader::new(rx).lines();

    while let Some(line) = lines.next_line().await? {
        info!("Parsed line: {line:?}");
        let root = match NIX_STORE.follow_store_path(line) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("Error: {}", e);
                continue;
            }
        };
        queue.add_job(Box::new(QueuedInput { path: root })).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    info!("Starting attic-queue");

    let db_path = std::env::var("DATABASE_PATH")?;

    let app_queue = AppQueue::new(db_path).await?;
    Arc::clone(&app_queue).run_job_workers_default();

    enqueue_thread(app_queue).await?;

    Ok(())
}
