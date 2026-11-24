mod compiler;
#[allow(dead_code)]
mod parser;
mod synctex;
mod worker_pool;
#[allow(dead_code)]
mod xref;

use compiler::{CompileResponse, EngineState};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use synctex::PdfSyncTarget;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Notify};

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Request {
    /// 1文字目の入力開始（0ms）で届く先行ステージング要求
    #[serde(rename = "prepare")]
    Prepare {
        source: String,
        #[serde(default)]
        line: usize,
        document_dir: Option<String>,
    },
    #[serde(rename = "compile")]
    Compile {
        id: u64,
        source: String,
        #[serde(default)]
        line: usize,
        #[serde(default)]
        draft: bool,
        document_dir: Option<String>,
    },
    #[serde(rename = "sync")]
    Sync { id: u64, line: usize },
    #[serde(rename = "shutdown")]
    Shutdown { id: u64 },
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum Response {
    #[serde(rename = "compile_result")]
    CompileResult {
        id: u64,
        #[serde(flatten)]
        result: CompileResponse,
    },
    #[serde(rename = "sync_result")]
    SyncResult {
        id: u64,
        target: Option<PdfSyncTarget>,
    },
    #[serde(rename = "shutdown_ok")]
    ShutdownOk { id: u64 },
    #[serde(rename = "error")]
    Error { id: u64, message: String },
}

struct QueuedCompile {
    id: u64,
    source: String,
    line: usize,
    draft: bool,
    doc_dir: Option<PathBuf>,
    queued_at: std::time::Instant,
}

#[tokio::main]
async fn main() {
    let engine = Arc::new(EngineState::new());
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let pending_job: Arc<Mutex<Option<QueuedCompile>>> = Arc::new(Mutex::new(None));
    let job_notify = Arc::new(Notify::new());
    let is_shutting_down = Arc::new( std::sync::atomic::AtomicBool::new(false) );

    // 実行中ワーカーを無駄に kill せず、完了次第最新の1件だけを連続処理するワーカータスク
    {
        let engine_bg = Arc::clone(&engine);
        let stdout_bg = Arc::clone(&stdout);
        let pending_bg = Arc::clone(&pending_job);
        let notify_bg = Arc::clone(&job_notify);
        let shutdown_bg = Arc::clone(&is_shutting_down);

        tokio::spawn(async move {
            loop {
                if shutdown_bg.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let next_job = { pending_bg.lock().await.take() };
                let Some(job) = next_job else {
                    notify_bg.notified().await;
                    continue;
                };

               let queue_ms = job.queued_at.elapsed().as_millis();
let kind = if job.draft { "draft" } else { "full" };

eprintln!(
    "[tex:start] id={} kind={} queue={}ms",
    job.id, kind, queue_ms
);

let result = engine_bg
    .compile_incremental(
        &job.source,
        job.line,
        job.draft,
        job.doc_dir.as_deref(),
    )
    .await;

eprintln!(
    "[tex:done] id={} kind={} mode={} queue={}ms engine={}ms",
    job.id, kind, result.mode, queue_ms, result.elapsed_ms
);

let resp = Response::CompileResult { id: job.id, result };
write_response(&stdout_bg, &resp).await;
            }
        });
    }

    eprintln!("[realtime-tex-server] ready on stdio (2-stage prefix streaming enabled)");

    let stdin = io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    while let Ok(Some(line_str)) = lines.next_line().await {
        let trimmed = line_str.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = Response::Error {
                    id: 0,
                    message: format!("Invalid JSON request: {}", e),
                };
                write_response(&stdout, &err_resp).await;
                continue;
            }
        };

        match req {
            Request::Prepare {
                source,
                line,
                document_dir,
            } => {
                let engine_clone = Arc::clone(&engine);
                let doc_dir = document_dir.filter(|s| !s.is_empty()).map(PathBuf::from);
                tokio::spawn(async move {
                    engine_clone
                        .prepare_for_typing(&source, line, doc_dir.as_deref())
                        .await;
                });
            }
            Request::Compile {
                id,
                source,
                line,
                draft,
                document_dir,
            } => {
                let doc_dir = document_dir.filter(|s| !s.is_empty()).map(PathBuf::from);
                let replaced = pending_job.lock().await.replace(QueuedCompile {
    id,
    source,
    line,
    draft,
    doc_dir,
    queued_at: std::time::Instant::now(), // 追加
});

                // まだ未着手だった古い待機リクエストがあれば即座に cancelled を返して Promise を解放
                if let Some(old) = replaced {
                    let cancelled = Response::CompileResult {
                        id: old.id,
                        result: CompileResponse {
                            status: "cancelled".into(),
                            pdf_path: None,
                            error_blocks: vec![],
                            cache_hits: 0,
                            cache_misses: 0,
                            resolved_labels_count: 0,
                            elapsed_ms: 0,
                            mode: "cancelled".into(),
                            error_detail: None,
                        },
                    };
                    write_response(&stdout, &cancelled).await;
                }

                job_notify.notify_one();
            }
            Request::Sync { id, line } => {
                let target = engine.sync_to_pdf(line);
                let resp = Response::SyncResult { id, target };
                write_response(&stdout, &resp).await;
            }
            Request::Shutdown { id } => {
                is_shutting_down.store(true, std::sync::atomic::Ordering::SeqCst);
                job_notify.notify_waiters();
                engine.shutdown(false).await;
                let resp = Response::ShutdownOk { id };
                write_response(&stdout, &resp).await;
                break;
            }
        }
    }

    is_shutting_down.store(true, std::sync::atomic::Ordering::SeqCst);
    engine.shutdown(false).await;
}

async fn write_response(stdout: &Arc<Mutex<io::Stdout>>, resp: &Response) {
    if let Ok(mut json) = serde_json::to_string(resp) {
        json.push('\n');
        let mut out = stdout.lock().await;
        let _ = out.write_all(json.as_bytes()).await;
        let _ = out.flush().await;
    }
}
