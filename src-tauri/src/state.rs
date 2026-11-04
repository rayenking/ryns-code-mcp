use reqwest::Client;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::AppHandle;
use tokio::sync::{watch, Mutex, RwLock};

use crate::client::build_http_client;
use crate::models::{AuditRecord, PendingRequest, SafetySettings};
use crate::server_process::ServerProcessManager;
use crate::sse::SseWorker;

pub struct AppState {
    pub config_dir: Option<PathBuf>,
    pub settings: Arc<RwLock<SafetySettings>>,
    pub client: Client,
    pub pending: Arc<RwLock<Vec<PendingRequest>>>,
    pub audit: Arc<RwLock<Vec<AuditRecord>>>,
    pub sse_cancel_tx: Arc<Mutex<Option<watch::Sender<bool>>>>,
    pub server_process: Arc<ServerProcessManager>,
}

impl AppState {
    pub fn new(config_dir: Option<PathBuf>, initial_settings: SafetySettings) -> Self {
        Self {
            config_dir,
            settings: Arc::new(RwLock::new(initial_settings)),
            client: build_http_client(),
            pending: Arc::new(RwLock::new(Vec::new())),
            audit: Arc::new(RwLock::new(Vec::new())),
            sse_cancel_tx: Arc::new(Mutex::new(None)),
            server_process: Arc::new(ServerProcessManager::new()),
        }
    }

    pub async fn start_sse_loop(&self, app_handle: AppHandle) {
        let mut lock = self.sse_cancel_tx.lock().await;
        // Signal any existing worker to exit
        if let Some(old_tx) = lock.take() {
            let _ = old_tx.send(true);
        }

        let (tx, rx) = watch::channel(false);
        *lock = Some(tx);

        let worker = SseWorker::new(
            app_handle,
            self.client.clone(),
            self.settings.clone(),
            self.pending.clone(),
        );

        tokio::spawn(async move {
            worker.run_loop(rx).await;
        });
    }
}
