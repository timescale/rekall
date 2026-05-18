use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProgressEvent {
    PhaseStart {
        phase: String,
        detail: String,
    },
    PhaseEnd {
        phase: String,
        elapsed_secs: f64,
    },
    LoadProgress {
        loaded: u64,
        total: u64,
        rows_per_sec: f64,
    },
    IndexBuildStart {
        index_name: String,
    },
    IndexBuildEnd {
        index_name: String,
        elapsed_secs: f64,
    },
    SearchStart {
        index_name: String,
        params: serde_json::Value,
    },
    SearchEnd {
        index_name: String,
        recall: f64,
        qps: f64,
    },
    Message {
        text: String,
    },
}

pub type ProgressTx = mpsc::UnboundedSender<ProgressEvent>;
pub type ProgressRx = mpsc::UnboundedReceiver<ProgressEvent>;

pub fn progress_channel() -> (ProgressTx, ProgressRx) {
    mpsc::unbounded_channel()
}
