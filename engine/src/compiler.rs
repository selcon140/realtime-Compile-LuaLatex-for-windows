use crate::parser::TexBlock;
use crate::synctex::{LineMapping, PdfSyncTarget, SourceMap, SyncTexIndex};
use crate::worker_pool::LuaTexWorkerPool;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct EngineState {
    pub synctex_index: Mutex<Option<SyncTexIndex>>,
    pub work_dir: PathBuf,
    pub preview_dir: PathBuf,
    pub preview_seq: AtomicU64,
    pub worker_pool: Arc<LuaTexWorkerPool>,
}

#[derive(Serialize)]
pub struct CompileResponse {
    pub status: String,
    pub pdf_path: Option<String>,
    pub error_blocks: Vec<TexBlock>,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub resolved_labels_count: usize,
    pub elapsed_ms: u128,
    pub mode: String,
    pub error_detail: Option<String>,
}

impl EngineState {
    pub fn new() -> Self {
        let work_dir = std::env::temp_dir().join("realtime_tex_vscode_engine");
        let preview_dir = work_dir.join("preview_out");
        if preview_dir.exists() {
            let _ = fs::remove_dir_all(&preview_dir);
        }
        fs::create_dir_all(&preview_dir).unwrap();
        let worker_pool = LuaTexWorkerPool::new(&work_dir);

        Self {
            synctex_index: Mutex::new(None),
            work_dir,
            preview_dir,
            preview_seq: AtomicU64::new(1),
            worker_pool,
        }
    }

    pub async fn shutdown(&self, remove_all_cache: bool) {
        self.worker_pool.shutdown().await;
        if self.preview_dir.exists() {
            let _ = fs::remove_dir_all(&self.preview_dir);
        }
        if remove_all_cache && self.work_dir.exists() {
            let _ = fs::remove_dir_all(&self.work_dir);
        }
    }

    pub fn sync_to_pdf(&self, editor_line: usize) -> Option<PdfSyncTarget> {
        let guard = self.synctex_index.lock().unwrap();
        guard.as_ref()?.lookup_editor_line(editor_line)
    }

    fn write_preview_pdf(&self, bytes: &[u8]) -> Result<String, String> {
        let seq = self.preview_seq.fetch_add(1, Ordering::Relaxed);
        let path = self.preview_dir.join(format!("preview_{}.pdf", seq));
        fs::write(&path, bytes).map_err(|e| e.to_string())?;

        if seq > 3 {
            let old = self.preview_dir.join(format!("preview_{}.pdf", seq - 3));
            let _ = fs::remove_file(old);
        }
        Ok(path.to_string_lossy().into_owned())
    }

    pub async fn compile_incremental(
        &self,
        source: &str,
        document_dir: Option<&Path>,
    ) -> CompileResponse {
        let start = std::time::Instant::now();

        let (Some((begin_start, begin_end)), true) = (
            find_begin_document(source),
            source.contains("\\end{document}"),
        ) else {
            return CompileResponse {
                status: "fallback_error".into(),
                pdf_path: None,
                error_blocks: vec![],
                cache_hits: 0,
                cache_misses: 0,
                resolved_labels_count: 0,
                elapsed_ms: start.elapsed().as_millis(),
                mode: "syntax_check".into(),
                error_detail: Some(
                    "\\begin{document} ... \\end{document} が未完結です".into(),
                ),
            };
        };

        let raw_preamble = &source[..begin_start];
        let raw_body_inner = &source[begin_end..];
        let preamble_hash = compute_sha256(raw_preamble);

        let _ = self
            .worker_pool
            .sync_preamble(raw_preamble, &preamble_hash, document_dir)
            .await;

        let job_result = self
            .worker_pool
            .run_job(&preamble_hash, raw_body_inner, source, document_dir)
            .await;

        match job_result {
            Ok(job_out) => {
                if let Some(raw_sync) = job_out.synctex_raw {
                    let prefix_lines = source[..begin_end].bytes().filter(|&b| b == b'\n').count();
                    let body_lines = raw_body_inner.lines().count().max(1);
                    let total_lines = source.lines().count().max(1);

                    let source_map = if job_out.used_prewarm {
                        SourceMap {
                            mappings: vec![LineMapping {
                                orig_start: prefix_lines + 1,
                                orig_end: total_lines,
                                gen_start: 1,
                                gen_end: body_lines,
                                is_independent: false,
                            }],
                        }
                    } else {
                        SourceMap::identity(total_lines)
                    };

                    let index = SyncTexIndex::parse_raw(&raw_sync, source_map);
                    *self.synctex_index.lock().unwrap() = Some(index);
                }

                let mode_str = if job_out.used_prewarm {
                    "warm_hit"
                } else {
                    "cold_start"
                };

                match self.write_preview_pdf(&job_out.pdf_bytes) {
                    Ok(pdf_path) => CompileResponse {
                        status: "updated".into(),
                        pdf_path: Some(pdf_path),
                        error_blocks: vec![],
                        cache_hits: usize::from(job_out.used_prewarm),
                        cache_misses: usize::from(!job_out.used_prewarm),
                        resolved_labels_count: 0,
                        elapsed_ms: start.elapsed().as_millis(),
                        mode: mode_str.into(),
                        error_detail: None,
                    },
                    Err(e) => CompileResponse {
                        status: "compile_error".into(),
                        pdf_path: None,
                        error_blocks: vec![],
                        cache_hits: 0,
                        cache_misses: 0,
                        resolved_labels_count: 0,
                        elapsed_ms: start.elapsed().as_millis(),
                        mode: mode_str.into(),
                        error_detail: Some(e),
                    },
                }
            }
            Err(err_msg) => CompileResponse {
                status: "compile_error".into(),
                pdf_path: None,
                error_blocks: vec![],
                cache_hits: 0,
                cache_misses: 0,
                resolved_labels_count: 0,
                elapsed_ms: start.elapsed().as_millis(),
                mode: "error".into(),
                error_detail: Some(err_msg),
            },
        }
    }
}

/// コメント行を避けて \begin{document} の開始・終了バイト位置を返す
fn find_begin_document(source: &str) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') {
        let code_part = strip_line_comment(line);
        if let Some(pos) = code_part.find("\\begin{document}") {
            let start = offset + pos;
            let end = start + "\\begin{document}".len();
            return Some((start, end));
        }
        offset += line.len();
    }
    None
}

fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'%' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

fn compute_sha256(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}
