use crate::parser::TexBlock;
use crate::synctex::{PdfSyncTarget, SyncTexIndex};
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

struct BodySplit<'a> {
    prefix: &'a str,
    suffix_body: &'a str,
    prefix_start_byte_in_source: usize,
    suffix_start_byte_in_source: usize,
    was_truncated: bool,
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

    /// 1文字目の入力開始（0ms）で呼ばれ、カーソルより上の確定領域を裏で先行組版開始する
    pub async fn prepare_for_typing(
        &self,
        source: &str,
        cursor_line: usize,
        document_dir: Option<&Path>,
    ) {
        let Some((begin_start, begin_end, end_doc_start)) = find_document_span(source) else {
            return;
        };

        let cursor_byte = line_to_byte_offset(source, cursor_line.max(1));
        // プリアンブル編集中は途中入力でワーカーを破棄しないようスキップ
        if cursor_byte < begin_start {
            return;
        }

        let raw_preamble = &source[..begin_start];
        let preamble_hash = compute_sha256(raw_preamble);

        if self
            .worker_pool
            .sync_preamble(raw_preamble, &preamble_hash, document_dir)
            .await
            .is_err()
        {
            return;
        }

        let split = split_body_around_cursor(source, begin_end, end_doc_start, cursor_byte, false);
        let prefix_hash = compute_sha256(split.prefix);

        self.worker_pool
            .stage_prefix(&preamble_hash, split.prefix, &prefix_hash, document_dir)
            .await;
    }

    pub async fn compile_incremental(
        &self,
        source: &str,
        cursor_line: usize,
        allow_draft: bool,
        document_dir: Option<&Path>,
    ) -> CompileResponse {
        let start = std::time::Instant::now();

        let Some((begin_start, begin_end, end_doc_start)) = find_document_span(source) else {
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
        let preamble_hash = compute_sha256(raw_preamble);

        let _ = self
            .worker_pool
            .sync_preamble(raw_preamble, &preamble_hash, document_dir)
            .await;

        let cursor_byte = line_to_byte_offset(source, cursor_line.max(1));
        let split =
            split_body_around_cursor(source, begin_end, end_doc_start, cursor_byte, allow_draft);

        let prefix_hash = compute_sha256(split.prefix);
        let suffix_with_end = format!("{}\n\\end{{document}}\n", split.suffix_body);

        let mut used_truncated = split.was_truncated;
        let mut job_result = self
            .worker_pool
            .run_job(
                &preamble_hash,
                split.prefix,
                &prefix_hash,
                &suffix_with_end,
                source,
                !used_truncated,
                document_dir,
            )
            .await;

        // もし高速ドラフトの部分切断でマクロ等の不整合が起きた場合は、即座に全文 suffix で再実行して誤エラーを防ぐ
        if job_result.is_err() && used_truncated {
            used_truncated = false;
            let full_split =
                split_body_around_cursor(source, begin_end, end_doc_start, cursor_byte, false);
            let full_prefix_hash = compute_sha256(full_split.prefix);
            let full_suffix = format!("{}\n\\end{{document}}\n", full_split.suffix_body);
            job_result = self
                .worker_pool
                .run_job(
                    &preamble_hash,
                    full_split.prefix,
                    &full_prefix_hash,
                    &full_suffix,
                    source,
                    true,
                    document_dir,
                )
                .await;
        }

        match job_result {
            Ok(job_out) => {
                if let Some(raw_sync) = job_out.synctex_raw {
                    let prefix_line_offset = count_newlines(&source[..split.prefix_start_byte_in_source]) as isize;
                    let suffix_line_offset = count_newlines(&source[..split.suffix_start_byte_in_source]) as isize;
                    let index = SyncTexIndex::parse_raw_with_offsets(
                        &raw_sync,
                        &[
                            ("prefix.tex", prefix_line_offset),
                            ("suffix.tex", suffix_line_offset),
                            ("job.tex", -1),
                        ],
                    );
                    *self.synctex_index.lock().unwrap() = Some(index);
                }

                let is_warm = job_out.mode_tag != "cold_start";
                let mode_str = if used_truncated && is_warm {
                    "draft_partial"
                } else {
                    job_out.mode_tag
                };

                match self.write_preview_pdf(&job_out.pdf_bytes) {
                    Ok(pdf_path) => CompileResponse {
                        status: "updated".into(),
                        pdf_path: Some(pdf_path),
                        error_blocks: vec![],
                        cache_hits: usize::from(is_warm),
                        cache_misses: usize::from(!is_warm),
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

    fn write_preview_pdf(&self, bytes: &[u8]) -> Result<String, String> {
        let seq = self.preview_seq.fetch_add(1, Ordering::Relaxed);
        let path = self.preview_dir.join(format!("preview_{}.pdf", seq));
        fs::write(&path, bytes).map_err(|e| e.to_string())?;

        if seq > 4 {
            let old = self.preview_dir.join(format!("preview_{}.pdf", seq - 4));
            let _ = fs::remove_file(old);
        }
        Ok(path.to_string_lossy().into_owned())
    }
}

/// カーソル行の前後にある「環境・波括弧・数式の外側の安全な段落境界」を特定して分割する
fn split_body_around_cursor<'a>(
    source: &'a str,
    begin_end: usize,
    end_doc_start: usize,
    cursor_byte_in_source: usize,
    allow_draft_truncate: bool,
) -> BodySplit<'a> {
    let body_core = &source[begin_end..end_doc_start];
    let cursor_in_body = cursor_byte_in_source
        .saturating_sub(begin_end)
        .min(body_core.len());

    let boundaries = collect_safe_boundaries(body_core);

    // カーソルより少なくとも 200 バイト手前の安全境界を prefix 終端にする（BackSpace 等でも prefix がぶれないようにする）
    let safe_margin_before = cursor_in_body.saturating_sub(200);
    let prefix_end = boundaries
        .iter()
        .copied()
        .filter(|&b| b <= safe_margin_before)
        .last()
        .unwrap_or(0);

    // 入力中（allow_draft_truncate）かつ後ろに重い要素や長文がある場合のみ、カーソル周辺以降を一時スキップ
    let mut suffix_end = body_core.len();
    let mut was_truncated = false;

    if allow_draft_truncate {
        let min_after_cursor = cursor_in_body.saturating_add(1600);
        if let Some(cut) = boundaries
            .iter()
            .copied()
            .find(|&b| b >= min_after_cursor && b < body_core.len())
        {
            let tail = &body_core[cut..];
            let has_heavy_tail = tail.len() > 4000
                || tail.contains("\\begin{tikzpicture}")
                || tail.contains("\\begin{pgfplots}")
                || tail.contains("\\begin{circuitikz}")
                || tail.contains("\\includegraphics");
            if has_heavy_tail {
                suffix_end = cut;
                was_truncated = true;
            }
        }
    }

    BodySplit {
        prefix: &body_core[..prefix_end],
        suffix_body: &body_core[prefix_end..suffix_end],
        prefix_start_byte_in_source: begin_end,
        suffix_start_byte_in_source: begin_end + prefix_end,
        was_truncated,
    }
}

/// 環境ネスト・波括弧・数式モードがすべて閉じている安全な段落境界オフセット一覧を収集
fn collect_safe_boundaries(body_core: &str) -> Vec<usize> {
    let mut boundaries = vec![0usize];
    let mut offset = 0usize;

    let mut env_depth: usize = 0;
    let mut brace_depth: i32 = 0;
    let mut in_display_bracket = false;
    let mut in_double_dollar = false;
    let mut in_inline_dollar = false;
    let mut in_makeatletter = false;

    for line in body_core.split_inclusive('\n') {
        let code = strip_line_comment(line);
        let trimmed = code.trim();

        let clean_before = env_depth == 0
            && brace_depth == 0
            && !in_display_bracket
            && !in_double_dollar
            && !in_inline_dollar
            && !in_makeatletter;

        if clean_before
            && (trimmed.starts_with("\\section")
                || trimmed.starts_with("\\subsection")
                || trimmed.starts_with("\\newpage")
                || trimmed.starts_with("\\clearpage"))
        {
            if boundaries.last().copied() != Some(offset) {
                boundaries.push(offset);
            }
        }

        let bytes = code.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => {
                    i += 1;
                    if i >= bytes.len() {
                        break;
                    }
                    if bytes[i] == b'[' {
                        in_display_bracket = true;
                        i += 1;
                    } else if bytes[i] == b']' {
                        in_display_bracket = false;
                        i += 1;
                    } else if bytes[i].is_ascii_alphabetic() {
                        let cmd_start = i;
                        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                            i += 1;
                        }
                        let cmd = &code[cmd_start..i];
                        match cmd {
                            "begin" => env_depth += 1,
                            "end" => env_depth = env_depth.saturating_sub(1),
                            "makeatletter" => in_makeatletter = true,
                            "makeatother" => in_makeatletter = false,
                            "verb" => {
                                if i < bytes.len() && bytes[i] == b'*' {
                                    i += 1;
                                }
                                if i < bytes.len() {
                                    let delim = bytes[i];
                                    i += 1;
                                    while i < bytes.len() && bytes[i] != delim {
                                        i += 1;
                                    }
                                    if i < bytes.len() {
                                        i += 1;
                                    }
                                }
                            }
                            _ => {}
                        }
                    } else {
                        i += 1;
                    }
                }
                b'{' => {
                    brace_depth += 1;
                    i += 1;
                }
                b'}' => {
                    brace_depth = (brace_depth - 1).max(0);
                    i += 1;
                }
                b'$' => {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                        in_double_dollar = !in_double_dollar;
                        i += 2;
                    } else {
                        in_inline_dollar = !in_inline_dollar;
                        i += 1;
                    }
                }
                _ => {
                    i += 1;
                }
            }
        }

        offset += line.len();

        let clean_after = env_depth == 0
            && brace_depth == 0
            && !in_display_bracket
            && !in_double_dollar
            && !in_inline_dollar
            && !in_makeatletter;

        if clean_after && trimmed.is_empty() {
            if boundaries.last().copied() != Some(offset) {
                boundaries.push(offset);
            }
        }
    }

    boundaries
}

fn find_document_span(source: &str) -> Option<(usize, usize, usize)> {
    let mut offset = 0usize;
    let mut begin_span = None;
    let mut end_doc_start = None;

    for line in source.split_inclusive('\n') {
        let code_part = strip_line_comment(line);
        if begin_span.is_none() {
            if let Some(pos) = code_part.find("\\begin{document}") {
                let start = offset + pos;
                let end = start + "\\begin{document}".len();
                begin_span = Some((start, end));
            }
        }
        if let Some(pos) = code_part.find("\\end{document}") {
            end_doc_start = Some(offset + pos);
        }
        offset += line.len();
    }

    let (b_start, b_end) = begin_span?;
    let e_start = end_doc_start?;
    if e_start >= b_end {
        Some((b_start, b_end, e_start))
    } else {
        None
    }
}

fn line_to_byte_offset(source: &str, target_line_1based: usize) -> usize {
    if target_line_1based <= 1 {
        return 0;
    }
    let mut current_line = 1usize;
    for (idx, b) in source.bytes().enumerate() {
        if b == b'\n' {
            current_line += 1;
            if current_line == target_line_1based {
                return idx + 1;
            }
        }
    }
    source.len()
}

fn count_newlines(s: &str) -> usize {
    s.bytes().filter(|&b| b == b'\n').count()
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
