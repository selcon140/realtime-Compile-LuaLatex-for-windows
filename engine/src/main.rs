mod compiler;
mod parser;
mod synctex;
mod worker_pool;
mod xref;

use compiler::{CompileResponse, EngineState};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use synctex::PdfSyncTarget;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

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
    let engine = EngineState::new();
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    eprintln!("[realtime-tex-server] ready on stdio");

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
                write_response(&mut stdout, &err_resp).await;
                continue;
            }
        };

        match req {
            Request::Compile {
                id,
                source,
                document_dir,
            } => {
                let doc_dir = document_dir.filter(|s| !s.is_empty()).map(PathBuf::from);
                let result = engine.compile_incremental(&source, doc_dir.as_deref()).await;
                let resp = Response::CompileResult { id, result };
                write_response(&mut stdout, &resp).await;
            }
            Request::Sync { id, line } => {
                let target = engine.sync_to_pdf(line);
                let resp = Response::SyncResult { id, target };
                write_response(&mut stdout, &resp).await;
            }
            Request::Shutdown { id } => {
                engine.shutdown(false).await;
                let resp = Response::ShutdownOk { id };
                write_response(&mut stdout, &resp).await;
                break;
            }
        }
    }

    engine.shutdown(false).await;
}

async fn write_response(stdout: &mut io::Stdout, resp: &Response) {
    if let Ok(mut json) = serde_json::to_string(resp) {
        json.push('\n');
        let _ = stdout.write_all(json.as_bytes()).await;
        let _ = stdout.flush().await;
    }
}
