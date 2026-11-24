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
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Request {
    #[serde(rename = "compile")]
    Compile {
        id: u64,
        source: String,
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

#[tokio::main]
async fn main() {
    let engine = Arc::new(EngineState::new());
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let stdin = io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    // 進行中のコンパイルタスク（新しい編集が来たら即 abort して差し替える）
    let mut active_compile: Option<(u64, JoinHandle<()>)> = None;

    eprintln!("[realtime-tex-server] ready on stdio (prewarm + preemption enabled)");

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
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
            Request::Compile {
                id,
                source,
                document_dir,
            } => {
                // 進行中の古いコンパイルがあれば即キャンセル（kill_on_drop により lualatex も即終了）
                if let Some((old_id, handle)) = active_compile.take() {
                    if !handle.is_finished() {
                        handle.abort();
                        let cancelled = Response::CompileResult {
                            id: old_id,
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
                }

                let engine_clone = Arc::clone(&engine);
                let stdout_clone = Arc::clone(&stdout);
                let doc_dir = document_dir.filter(|s| !s.is_empty()).map(PathBuf::from);

                let handle = tokio::spawn(async move {
                    let result = engine_clone
                        .compile_incremental(&source, doc_dir.as_deref())
                        .await;
                    let resp = Response::CompileResult { id, result };
                    write_response(&stdout_clone, &resp).await;
                });

                active_compile = Some((id, handle));
            }
            Request::Sync { id, line } => {
                let target = engine.sync_to_pdf(line);
                let resp = Response::SyncResult { id, target };
                write_response(&stdout, &resp).await;
            }
            Request::Shutdown { id } => {
                if let Some((_, handle)) = active_compile.take() {
                    handle.abort();
                }
                engine.shutdown(false).await;
                let resp = Response::ShutdownOk { id };
                write_response(&stdout, &resp).await;
                break;
            }
        }
    }

    if let Some((_, handle)) = active_compile.take() {
        handle.abort();
    }
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
