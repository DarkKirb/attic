use std::{
    collections::{HashMap, HashSet},
    fs::{self},
    path::Path,
    sync::Arc,
};

use anyhow::Result;
use attic::{
    cache::CacheName,
    nix_store::{NixStore, StorePath, StorePathHash, ValidPathInfo},
};
use attic_client::{api::ApiClient, cache::CacheRef, config::Config, push::upload_path};
use futures::future::join_all;
use indicatif::MultiProgress;
use nix::{sys::stat::Mode, unistd::mkfifo};
use serde::{Deserialize, Serialize};
use sled::Db;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::unix::pipe,
    sync::{Notify, Semaphore},
    task::JoinSet,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum PathState {
    Queued,
    InProgress,
}

async fn enqueue_path(
    db: Arc<Db>,
    store: Arc<NixStore>,
    path: StorePath,
    notifier: Arc<Notify>,
    api: ApiClient,
) -> Result<()> {
    let mut deps = store
        .compute_fs_closure(path.clone(), false, true, true)
        .await?;
    deps.push(path);

    let mut store_path_map: HashMap<StorePathHash, ValidPathInfo> = {
        let futures = deps
            .iter()
            .map(|path| {
                let store = store.clone();
                let path = path.clone();
                let path_hash = path.to_hash();

                async move {
                    let path_info = store.query_path_info(path).await?;
                    Ok((path_hash, path_info))
                }
            })
            .collect::<Vec<_>>();

        join_all(futures).await.into_iter().collect::<Result<_>>()?
    };

    store_path_map.retain(|_, pi| {
        !pi.sigs
            .iter()
            .any(|sig| sig.starts_with("cache.nixos.org-1:"))
    });

    let missing_path_hashes: HashSet<StorePathHash> = {
        let store_path_hashes = store_path_map.keys().map(|sph| sph.to_owned()).collect();
        let res = api
            .get_missing_paths(&CacheName::new("chir-rs".to_string())?, store_path_hashes)
            .await?;
        res.missing_paths.into_iter().collect()
    };
    store_path_map.retain(|sph, _| missing_path_hashes.contains(sph));

    let queued_ser = bincode::serialize(&PathState::Queued)?;

    for (_, pi) in store_path_map {
        db.compare_and_swap(
            store
                .get_full_path(&pi.path)
                .into_os_string()
                .into_string()
                .unwrap(),
            None as Option<&[u8]>,
            Some(&queued_ser[..]),
        )
        .ok();
    }

    notifier.notify_one();

    Ok(())
}

async fn enqueue_thread(db: Arc<Db>, store: Arc<NixStore>, notifier: Arc<Notify>) -> Result<()> {
    let queue_path = std::env::var("QUEUE_PATH")?;
    let config = Config::load()?;
    let cache_ref = CacheRef::DefaultServer(CacheName::new("chir-rs".to_string())?);
    let (_, server, _) = config.resolve_cache(&cache_ref)?;
    let api = ApiClient::from_server_config(server.clone())?;

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
        let root = match store.follow_store_path(line) {
            Ok(root) => root,
            Err(e) => {
                eprintln!("Error: {}", e);
                continue;
            }
        };
        enqueue_path(
            Arc::clone(&db),
            Arc::clone(&store),
            root,
            Arc::clone(&notifier),
            api.clone(),
        ).await?;
    }
    Ok(())
}

async fn is_valid_path(store: &Arc<NixStore>, path: impl AsRef<Path>) -> bool {
    let store_path = match store.parse_store_path(path) {
        Ok(path) => path,
        Err(_) => {
            return false;
        }
    };

    match store.query_path_info(store_path).await {
        Ok(_) => true,
        Err(_) => false,
    }
}

async fn boot_cleanup(db: &Arc<Db>, store: &Arc<NixStore>) -> Result<()> {
    let busy_ser = bincode::serialize(&PathState::InProgress)?;
    let queued_ser = bincode::serialize(&PathState::Queued)?;
    for kv in db.iter() {
        let (key, value) = kv?;
        if !is_valid_path(
            store,
            String::from_utf8(value.clone().to_vec()).unwrap_or_default(),
        )
        .await
        {
            db.remove(&value)?;
            continue;
        }
        let de_value: PathState = bincode::deserialize(&value)?;
        match de_value {
            PathState::InProgress => {
                db.compare_and_swap(&key, Some(&value), Some(&queued_ser[..]))
                    .ok();
            }
            _ => {}
        }
        db.compare_and_swap(&key, Some(&busy_ser[..]), Some(&queued_ser[..]))
            .ok();
    }
    Ok(())
}

async fn upload_file(
    db: Arc<Db>,
    path: String,
    store: Arc<NixStore>,
    api: ApiClient,
    cache: &CacheName,
    mp: MultiProgress,
    notifier: Arc<Notify>,
) -> Result<()> {
    let busy_ser = bincode::serialize(&PathState::InProgress)?;
    let queued_ser = bincode::serialize(&PathState::Queued)?;
    db.compare_and_swap(path.as_bytes(), Some(&queued_ser), Some(&busy_ser[..]))??;
    let store_path = store.parse_store_path(&path)?;
    let path_info = store.query_path_info(store_path).await?;

    match upload_path(path_info, store, api, cache, mp, false).await {
        Ok(_) => {
            db.remove(path.as_bytes())?;
            Ok(())
        }
        Err(e) => {
            db.insert(path.as_bytes(), &queued_ser[..])?;
            notifier.notify_one();
            Err(e.into())
        }
    }
}

async fn upload_thread(db: Arc<Db>, store: Arc<NixStore>, notifier: Arc<Notify>) -> Result<()> {
    let config = Config::load()?;
    let cache_ref = CacheRef::DefaultServer(CacheName::new("chir-rs".to_string())?);
    let (_, server, cache) = config.resolve_cache(&cache_ref)?;
    let api = ApiClient::from_server_config(server.clone())?;
    let mp = MultiProgress::new();

    notifier.notify_one();
    let upload_semaphore = Arc::new(Semaphore::new(num_cpus::get() * 4));
    loop {
        notifier.notified().await;

        // Loop through all files and figure out which ones need to be uploaded.
        for kv in db.iter() {
            let (key, value) = kv?;
            let de_value: PathState = bincode::deserialize(&value)?;
            if de_value == PathState::Queued {
                let db = Arc::clone(&db);
                let upload_semaphore = Arc::clone(&upload_semaphore);
                let store = Arc::clone(&store);
                let api = api.clone();
                let mp = mp.clone();
                let cache = cache.clone();
                let notifier = Arc::clone(&notifier);

                tokio::spawn(async move {
                    let _sem = upload_semaphore.acquire().await.unwrap();
                    upload_file(
                        db,
                        String::from_utf8(key.clone().to_vec()).unwrap_or_default(),
                        store,
                        api,
                        &cache,
                        mp,
                        notifier,
                    )
                    .await
                    .unwrap();
                });
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let sled_path = std::env::var("SLED_PATH")?;

    let db = Arc::new(sled::open(sled_path)?);

    let store = Arc::new(NixStore::connect()?);

    boot_cleanup(&db, &store).await?;

    let notifier = Arc::new(Notify::new());

    let mut joinset = JoinSet::new();

    joinset.spawn(enqueue_thread(
        Arc::clone(&db),
        Arc::clone(&store),
        Arc::clone(&notifier),
    ));
    joinset.spawn(upload_thread(db, store, notifier));

    while let Some(res) = joinset.join_next().await {
        res??;
    }

    Ok(())
}
