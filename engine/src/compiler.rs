use crate::parser::{parse_and_split, BlockKind, TexBlock};
use crate::synctex::{LineMapping, PdfSyncTarget, SourceMap, SyncTexIndex};
use crate::worker_pool::{split_preamble_for_format, LuaTexWorkerPool};
use crate::xref::{resolve_document_xref, XrefTable};
use base64::Engine as _;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const MAX_CONCURRENT_WORKERS: usize = 4;

/// 独立ブロック（preview環境）内で figure / table を安全に非フロート化するパッチ
const NON_FLOAT_PATCH: &str = r#"
\makeatletter
\renewenvironment{figure}[1][]{%
  \par\noindent\begin{minipage}{\linewidth}%
  \def\@captype{figure}%
}{%
  \end{minipage}\par
}
\renewenvironment{figure*}[1][]{%
  \par\noindent\begin{minipage}{\linewidth}%
  \def\@captype{figure}%
}{%
  \end{minipage}\par
}
\renewenvironment{table}[1][]{%
  \par\noindent\begin{minipage}{\linewidth}%
  \def\@captype{table}%
}{%
  \end{minipage}\par
}
\renewenvironment{table*}[1][]{%
  \par\noindent\begin{minipage}{\linewidth}%
  \def\@captype{table}%
}{%
  \end{minipage}\par
}
\makeatother
"#;

pub struct EngineState {
    pub last_good_pdf: Mutex<Option<Vec<u8>>>,
    pub synctex_index: Mutex<Option<SyncTexIndex>>,
    pub work_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub semaphore: Arc<Semaphore>,
    pub worker_pool: Arc<LuaTexWorkerPool>,
}

#[derive(Serialize)]
pub struct CompileResponse {
    pub status: String,
    pub pdf_base64: Option<String>,
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
        let cache_dir = work_dir.join("block_cache");
        fs::create_dir_all(&cache_dir).unwrap();
        let worker_pool = LuaTexWorkerPool::new(&work_dir);

        Self {
            last_good_pdf: Mutex::new(None),
            synctex_index: Mutex::new(None),
            work_dir,
            cache_dir,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_WORKERS)),
            worker_pool,
        }
    }

    pub async fn shutdown(&self, remove_all_cache: bool) {
        self.worker_pool.shutdown().await;
        if remove_all_cache && self.work_dir.exists() {
            let _ = fs::remove_dir_all(&self.work_dir);
        }
    }

    pub fn sync_to_pdf(&self, editor_line: usize) -> Option<PdfSyncTarget> {
        let guard = self.synctex_index.lock().unwrap();
        guard.as_ref()?.lookup_editor_line(editor_line)
    }

    pub async fn compile_incremental(
        &self,
        source: &str,
        document_dir: Option<&Path>,
    ) -> CompileResponse {
        let start = std::time::Instant::now();
        let parsed = parse_and_split(source);

        if parsed.has_fatal_error {
            if source.contains("\\begin{document}") && source.contains("\\end{document}") {
                return self.compile_raw_fallback(source, document_dir, start).await;
            }
            let error_blocks = parsed.blocks.into_iter().filter(|b| b.has_error).collect();
            return CompileResponse {
                status: "fallback_error".into(),
                pdf_base64: None,
                error_blocks,
                cache_hits: 0,
                cache_misses: 0,
                resolved_labels_count: 0,
                elapsed_ms: start.elapsed().as_millis(),
                mode: "syntax_check".into(),
                error_detail: Some(
                    "\\begin{document} ... \\end{document} が未完結です".into(),
                ),
            };
        }

        // 1. 相互参照とカウンタ値を組版から独立して先行解決
        let xref = resolve_document_xref(&parsed.blocks);
        let xref_preload = xref.to_latex_preload();

        // 2. プリアンブル分離とメモリ常駐プリウォームワーカー同期
        let (dumpable_preamble, deferred_preamble) = split_preamble_for_format(&parsed.preamble);
        let fmt_hash = compute_sha256(&dumpable_preamble);

        if self
            .worker_pool
            .sync_preamble(&dumpable_preamble, &fmt_hash, document_dir)
            .await
            .is_err()
        {
            return self.compile_raw_fallback(source, document_dir, start).await;
        }

        let dir_token = document_dir
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        // 3. カウンタ値・参照先番号・作業ディレクトリを織り込んだ実効キャッシュキーで判定
        let mut cache_hits = 0usize;
        let mut effective_keys: Vec<String> = Vec::with_capacity(parsed.blocks.len());
        let mut jobs: Vec<(String, String, String, String)> = Vec::new();
        let mut queued_keys: HashSet<String> = HashSet::new();

        for (block, plan) in parsed.blocks.iter().zip(xref.block_plans.iter()) {
            let eff_key = compute_sha256(&format!(
                "{}|{}|{}|{}",
                block.cache_key,
                plan.start_counters.to_cache_token(),
                xref.dependency_token_for(plan),
                dir_token
            ));

            if let BlockKind::Independent { env_name } = &block.kind {
                if self.cached_pdf_path(&eff_key).exists() {
                    cache_hits += 1;
                } else if queued_keys.insert(eff_key.clone()) {
                    let setcounters = plan.start_counters.to_latex_setcounters();
                    jobs.push((
                        eff_key.clone(),
                        setcounters,
                        block.content.clone(),
                        env_name.clone(),
                    ));
                }
            }
            effective_keys.push(eff_key);
        }
        let cache_misses = jobs.len();

        // 4. キャッシュミスした独立ブロックのみ並列コンパイル
        let mut join_set = JoinSet::new();
        let doc_dir_owned = document_dir.map(Path::to_path_buf);

        for (eff_key, setcounters, block_content, env_name) in jobs {
            let pool = Arc::clone(&self.worker_pool);
            let sem = Arc::clone(&self.semaphore);
            let fmt_h = fmt_hash.clone();
            let deferred = deferred_preamble.clone();
            let preload = xref_preload.clone();
            let final_pdf = self.cached_pdf_path(&eff_key);
            let dir_opt = doc_dir_owned.clone();

            join_set.spawn(async move {
                let _permit = sem.acquire_owned().await.unwrap();

                let wrapped_content = match env_name.as_str() {
                    "axiom" | "definition" | "theorem" | "tcolorbox" | "equation" | "equation*"
                    | "align" | "align*" => {
                        format!(
                            "\\begin{{minipage}}{{\\linewidth}}\n{}\n\\end{{minipage}}",
                            block_content
                        )
                    }
                    _ => block_content,
                };

                let body = format!(
                    "{deferred}\n\
                     \\usepackage[active,tightpage]{{preview}}\n\
                     \\setlength{{\\PreviewBorder}}{{1pt}}\n\
                     {NON_FLOAT_PATCH}\n\
                     {preload}\n\
                     \\begin{{document}}\n\
                     {setcounters}\n\
                     \\begin{{preview}}\n\
                     {wrapped_content}\n\
                     \\end{{preview}}\n\
                     \\end{{document}}\n"
                );
                let job_out = pool.run_job(&fmt_h, &body, dir_opt.as_deref()).await?;
                fs::write(&final_pdf, job_out.pdf_bytes).map_err(|e| e.to_string())
            });
        }

        while let Some(res) = join_set.join_next().await {
            if !matches!(res, Ok(Ok(()))) {
                return self.compile_raw_fallback(source, document_dir, start).await;
            }
        }

        // 5. 親文書の組み立てと SyncTeX インデックス更新
        let (master_body, source_map) = self.build_assembled_body(
            &deferred_preamble,
            &xref_preload,
            &parsed.blocks,
            &effective_keys,
            &xref,
        );

        match self
            .worker_pool
            .run_job(&fmt_hash, &master_body, document_dir)
            .await
        {
            Ok(job_out) => {
                *self.last_good_pdf.lock().unwrap() = Some(job_out.pdf_bytes.clone());
                if let Some(gz) = job_out.synctex_gz {
                    if let Ok(index) = SyncTexIndex::parse_gz(&gz, source_map) {
                        *self.synctex_index.lock().unwrap() = Some(index);
                    }
                }
                let encoded = base64::engine::general_purpose::STANDARD.encode(&job_out.pdf_bytes);
                CompileResponse {
                    status: "updated".into(),
                    pdf_base64: Some(encoded),
                    error_blocks: vec![],
                    cache_hits,
                    cache_misses,
                    resolved_labels_count: xref.labels.len(),
                    elapsed_ms: start.elapsed().as_millis(),
                    mode: "incremental".into(),
                    error_detail: None,
                }
            }
            Err(_) => self.compile_raw_fallback(source, document_dir, start).await,
        }
    }

    /// 分割コンパイル非互換な複雑な文書を、メモリ常駐ワーカー優先で直接コンパイルする
    async fn compile_raw_fallback(
        &self,
        source: &str,
        document_dir: Option<&Path>,
        start: std::time::Instant,
    ) -> CompileResponse {
        let job_result = if let Some(idx) = source.find("\\begin{document}") {
            let raw_preamble = &source[..idx];
            let raw_body = &source[idx..];
            let (dumpable, deferred) = split_preamble_for_format(raw_preamble);
            let p_hash = compute_sha256(&dumpable);

            let warm_res = if self
                .worker_pool
                .sync_preamble(&dumpable, &p_hash, document_dir)
                .await
                .is_ok()
            {
                let body_with_deferred = format!("{}\n{}", deferred, raw_body);
                self.worker_pool
                    .run_job(&p_hash, &body_with_deferred, document_dir)
                    .await
            } else {
                Err("prewarm sync failed".into())
            };

            match warm_res {
                Ok(out) => Ok(out),
                Err(_) => self.worker_pool.run_raw_source(source, document_dir).await,
            }
        } else {
            self.worker_pool.run_raw_source(source, document_dir).await
        };

        match job_result {
            Ok(job_out) => {
                *self.last_good_pdf.lock().unwrap() = Some(job_out.pdf_bytes.clone());
                if let Some(gz) = job_out.synctex_gz {
                    let total_lines = source.lines().count();
                    if let Ok(index) =
                        SyncTexIndex::parse_gz(&gz, SourceMap::identity(total_lines))
                    {
                        *self.synctex_index.lock().unwrap() = Some(index);
                    }
                }
                let encoded = base64::engine::general_purpose::STANDARD.encode(&job_out.pdf_bytes);
                CompileResponse {
                    status: "updated".into(),
                    pdf_base64: Some(encoded),
                    error_blocks: vec![],
                    cache_hits: 0,
                    cache_misses: 0,
                    resolved_labels_count: 0,
                    elapsed_ms: start.elapsed().as_millis(),
                    mode: "raw_warm".into(),
                    error_detail: None,
                }
            }
            Err(err_msg) => CompileResponse {
                status: "compile_error".into(),
                pdf_base64: None,
                error_blocks: vec![],
                cache_hits: 0,
                cache_misses: 0,
                resolved_labels_count: 0,
                elapsed_ms: start.elapsed().as_millis(),
                mode: "raw_fallback".into(),
                error_detail: Some(err_msg),
            },
        }
    }

    fn cached_pdf_path(&self, cache_key: &str) -> PathBuf {
        self.cache_dir.join(format!("{}.pdf", cache_key))
    }

    fn build_assembled_body(
        &self,
        deferred_preamble: &str,
        xref_preload: &str,
        blocks: &[TexBlock],
        effective_keys: &[String],
        xref: &XrefTable,
    ) -> (String, SourceMap) {
        let mut out = String::new();
        let mut current_gen_line: usize = 1;
        let mut mappings = Vec::with_capacity(blocks.len());

        let push_chunk = |buf: &mut String, line_counter: &mut usize, chunk: &str| {
            buf.push_str(chunk);
            *line_counter += chunk.bytes().filter(|&b| b == b'\n').count();
        };

        push_chunk(&mut out, &mut current_gen_line, deferred_preamble);
        push_chunk(&mut out, &mut current_gen_line, "\n");
        push_chunk(&mut out, &mut current_gen_line, xref_preload);
        push_chunk(&mut out, &mut current_gen_line, "\n\\begin{document}\n");

        for ((block, eff_key), plan) in blocks
            .iter()
            .zip(effective_keys.iter())
            .zip(xref.block_plans.iter())
        {
            match &block.kind {
                BlockKind::Independent { .. } => {
                    let pdf_path = self
                        .cached_pdf_path(eff_key)
                        .to_string_lossy()
                        .replace('\\', "/");

                    push_chunk(&mut out, &mut current_gen_line, "\n");
                    let gen_start = current_gen_line;
                    push_chunk(
                        &mut out,
                        &mut current_gen_line,
                        &format!("\\par\\noindent\\includegraphics{{{}}}\\par\n", pdf_path),
                    );
                    let gen_end = current_gen_line.saturating_sub(1);

                    mappings.push(LineMapping {
                        orig_start: block.start_line,
                        orig_end: block.end_line,
                        gen_start,
                        gen_end,
                        is_independent: true,
                    });

                    push_chunk(
                        &mut out,
                        &mut current_gen_line,
                        &plan.end_counters.to_latex_setcounters(),
                    );
                }
                BlockKind::InlineText => {
                    let gen_start = current_gen_line;
                    push_chunk(&mut out, &mut current_gen_line, &block.content);
                    push_chunk(&mut out, &mut current_gen_line, "\n");
                    let gen_end = current_gen_line.saturating_sub(1);

                    mappings.push(LineMapping {
                        orig_start: block.start_line,
                        orig_end: block.end_line,
                        gen_start,
                        gen_end,
                        is_independent: false,
                    });
                }
            }
        }

        push_chunk(&mut out, &mut current_gen_line, "\n\\end{document}\n");
        (out, SourceMap { mappings })
    }
}

fn compute_sha256(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}
