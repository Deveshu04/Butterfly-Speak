//! Model manager: catalog ⨯ install state ⨯ RAM verdicts, download
//! orchestration, and switching the active model.

pub mod catalog;
pub mod downloader;
pub mod ram;

use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    #[serde(flatten)]
    pub entry: catalog::ModelEntry,
    pub installed: bool,
    pub selected: bool,
    pub downloading: bool,
    pub verdict: ram::RamVerdict,
}

#[derive(Default)]
pub struct DownloadRegistry {
    active: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl DownloadRegistry {
    pub fn begin(&self, id: &str) -> Option<Arc<AtomicBool>> {
        let mut map = self.active.lock().expect("downloads lock");
        if map.contains_key(id) {
            return None; // already downloading
        }
        let flag = Arc::new(AtomicBool::new(false));
        map.insert(id.to_string(), flag.clone());
        Some(flag)
    }

    pub fn finish(&self, id: &str) {
        self.active.lock().expect("downloads lock").remove(id);
    }

    pub fn cancel(&self, id: &str) {
        if let Some(flag) = self.active.lock().expect("downloads lock").get(id) {
            flag.store(true, Ordering::Relaxed);
        }
    }

    pub fn is_downloading(&self, id: &str) -> bool {
        self.active.lock().expect("downloads lock").contains_key(id)
    }
}

pub fn list(selected_id: &str, downloads: &DownloadRegistry) -> Vec<ModelStatus> {
    catalog::catalog()
        .models
        .iter()
        .map(|entry| ModelStatus {
            entry: entry.clone(),
            installed: downloader::dir_installed(&entry.dir_name),
            selected: entry.id == selected_id,
            downloading: downloads.is_downloading(&entry.id),
            verdict: ram::verdict(entry.est_ram_bytes),
        })
        .collect()
}

/// Support artifacts (the punctuation model) that should exist before
/// dictation works well. Returns jobs still missing.
pub fn missing_support_jobs() -> Vec<downloader::Job> {
    let cat = catalog::catalog();
    let mut jobs = Vec::new();
    if !downloader::dir_installed(&cat.punctuation.dir_name) {
        jobs.push(downloader::Job {
            id: "punctuation".into(),
            url: cat.punctuation.url.clone(),
            sha256: cat.punctuation.sha256.clone(),
            total_bytes: cat.punctuation.disk_bytes,
            kind: downloader::JobKind::Archive {
                dir_name: cat.punctuation.dir_name.clone(),
            },
        });
    }
    jobs
}

pub fn job_for(entry: &catalog::ModelEntry) -> downloader::Job {
    downloader::Job {
        id: entry.id.clone(),
        url: entry.url.clone(),
        sha256: entry.sha256.clone(),
        total_bytes: entry.disk_bytes,
        kind: downloader::JobKind::Archive {
            dir_name: entry.dir_name.clone(),
        },
    }
}
