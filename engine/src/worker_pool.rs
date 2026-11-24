use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const POOL_SIZE: usize = 3;
const PREWARM_MARKER: &str = "@@PREWARM_READY@@";
const PREFIX_MARKER: &str = "@@PREFIX_READY@@";
const UNCOMPRESSED_PDF_CMD: &str =
    "\\pdfvariable compresslevel=0 \\pdfvariable objcompresslevel=0\n";

pub const DRAFT_DEFERRED: &str = "__realtime_tex_draft_deferred__";

pub struct JobOutput {
    pub pdf_bytes: Vec<u8>,
    pub synctex_raw: Option<String>,
    pub mode_tag: &'static str,
}

struct SlotDirGuard(PathBuf);

impl Drop for SlotDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, PartialEq, Eq)]
struct WarmKey {
    preamble_hash: String,
    document_dir: Option<PathBuf>,
}

#[derive(Clone)]
struct PoolConfig {
    key: WarmKey,
    preamble: String,
}

/// 第1ゲート（\begin{document} + フォント読込直後）で待機中のワーカー
struct PrewarmedWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    slot_dir: PathBuf,
    key: WarmKey,
}

/// 第2ゲート（カーソル直前の prefix.tex まで組版完了）へ進行中または待機中のワーカー
struct StagedWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    slot_dir: PathBuf,
    key: WarmKey,
}

struct StagedEntry {
    key: WarmKey,
    prefix_hash: String,
    handle: JoinHandle<Option<StagedWorker>>,
}

pub struct LuaTexWorkerPool {
    slots_root: PathBuf,
    aux_dir: PathBuf,
    config: Mutex<Option<PoolConfig>>,
    idle_workers: Mutex<VecDeque<PrewarmedWorker>>,
    staged: Mutex<Option<StagedEntry>>,
    slot_counter: AtomicUsize,
    is_shutting_down: AtomicBool,
    replenish_notify: Arc<Notify>,
}

impl LuaTexWorkerPool {
    pub fn new(work_dir: &Path) -> Arc<Self> {
        let slots_root = work_dir.join("worker_slots");
        let aux_dir = work_dir.join("aux_cache");
        if slots_root.exists() {
            let _ = fs::remove_dir_all(&slots_root);
        }
        fs::create_dir_all(&slots_root).unwrap();
        fs::create_dir_all(&aux_dir).unwrap();

        let pool = Arc::new(Self {
            slots_root,
            aux_dir,
            config: Mutex::new(None),
            idle_workers: Mutex::new(VecDeque::new()),
            staged: Mutex::new(None),
            slot_counter: AtomicUsize::new(0),
            is_shutting_down: AtomicBool::new(false),
            replenish_notify: Arc::new(Notify::new()),
        });

        tokio::spawn(Arc::clone(&pool).replenish_loop());
        pool
    }

    pub async fn shutdown(&self) {
        self.is_shutting_down.store(true, Ordering::SeqCst);
        self.replenish_notify.notify_waiters();

        if let Some(entry) = self.staged.lock().await.take() {
            cleanup_staged_entry(entry).await;
        }

        let workers: Vec<PrewarmedWorker> = {
            let mut pool = self.idle_workers.lock().await;
            pool.drain(..).collect()
        };
        for mut w in workers {
            let _ = w.child.kill().await;
            let _ = fs::remove_dir_all(&w.slot_dir);
        }

        if self.slots_root.exists() {
            let _ = fs::remove_dir_all(&self.slots_root);
        }
    }

    pub async fn sync_preamble(
        self: &Arc<Self>,
        preamble: &str,
        preamble_hash: &str,
        document_dir: Option<&Path>,
    ) -> Result<(), String> {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return Err("Worker pool is shutting down".into());
        }

        let new_key = WarmKey {
            preamble_hash: preamble_hash.to_string(),
            document_dir: document_dir.map(Path::to_path_buf),
        };

        {
            let guard = self.config.lock().await;
            if let Some(ref cfg) = *guard {
                if cfg.key == new_key {
                    return Ok(());
                }
            }
        }

        if let Some(old_staged) = self.staged.lock().await.take() {
            tokio::spawn(cleanup_staged_entry(old_staged));
        }

        let stale: Vec<PrewarmedWorker> = {
            let mut pool = self.idle_workers.lock().await;
            pool.drain(..).collect()
        };
        for mut w in stale {
            let _ = w.child.kill().await;
            let _ = fs::remove_dir_all(&w.slot_dir);
        }

        {
            let mut guard = self.config.lock().await;
            *guard = Some(PoolConfig {
                key: new_key,
                preamble: preamble.to_string(),
            });
        }

        self.replenish_notify.notify_one();
        Ok(())
    }

    /// ユーザーが1文字目を入力した瞬間に呼ばれ、カーソル直前までの prefix.tex を先行組版して待機させる
    pub async fn stage_prefix(
        self: &Arc<Self>,
        preamble_hash: &str,
        prefix_content: &str,
        prefix_hash: &str,
        document_dir: Option<&Path>,
    ) {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return;
        }

        let target_key = WarmKey {
            preamble_hash: preamble_hash.to_string(),
            document_dir: document_dir.map(Path::to_path_buf),
        };

        {
            let staged_guard = self.staged.lock().await;
            if let Some(ref existing) = *staged_guard {
                if existing.key == target_key && existing.prefix_hash == prefix_hash {
                    // 同じ prefix で既に先行実行中または待機完了しているためそのまま活かす
                    return;
                }
            }
        }

        let Some(worker) = self.pop_alive_warm_worker(&target_key).await else {
            self.replenish_notify.notify_one();
            return;
        };
        self.replenish_notify.notify_one();

        let prefix_owned = prefix_content.to_string();
        let key_clone = target_key.clone();
        let handle = tokio::spawn(async move {
            advance_to_prefix_gate(worker, &prefix_owned, key_clone).await
        });

        let prev = self.staged.lock().await.replace(StagedEntry {
            key: target_key,
            prefix_hash: prefix_hash.to_string(),
            handle,
        });
        if let Some(old_entry) = prev {
            tokio::spawn(cleanup_staged_entry(old_entry));
        }
    }

    pub async fn run_job(
        self: &Arc<Self>,
        preamble_hash: &str,
        prefix_content: &str,
        prefix_hash: &str,
        suffix_content: &str,
        full_source: &str,
        is_full_document: bool,
        allow_cold_start: bool,
        document_dir: Option<&Path>,
    ) -> Result<JobOutput, String> {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return Err("Worker pool is shutting down".into());
        }

        let target_key = WarmKey {
            preamble_hash: preamble_hash.to_string(),
            document_dir: document_dir.map(Path::to_path_buf),
        };

        // 1. 入力開始時から先行組版していた StagedWorker があれば最優先で使用（ヘッドスタート活用）
        let matching_staged = {
            let mut staged_guard = self.staged.lock().await;
            if staged_guard
                .as_ref()
                .map(|s| s.key == target_key && s.prefix_hash == prefix_hash)
                .unwrap_or(false)
            {
                staged_guard.take()
            } else {
                None
            }
        };

        if let Some(entry) = matching_staged {
            if let Ok(Some(mut sw)) = entry.handle.await {
                if matches!(sw.child.try_wait(), Ok(None)) {
                    // 次の打鍵に備えて裏で即座に次のワーカーも先行ステージング開始
                    self.spawn_next_stage_in_bg(
                        preamble_hash,
                        prefix_content,
                        prefix_hash,
                        document_dir,
                    );

                    let res = self
                        .finish_from_prefix_gate(&mut sw, suffix_content, preamble_hash, is_full_document, "staged_hit")
                        .await;
                    let _ = fs::remove_dir_all(&sw.slot_dir);
                    return res;
                } else {
                    let _ = sw.child.kill().await;
                    let _ = fs::remove_dir_all(&sw.slot_dir);
                }
            }
        }

        // 2. 第1ゲート待機中の PrewarmedWorker があればそれを使用
        let mut available_worker = self.pop_alive_warm_worker(&target_key).await;

// 全文同期に限り、補充中のワーカーがまもなく使える可能性を考えて少し待つ。
// ドラフトは allow_cold_start == false なので、この待機には入らない。
if available_worker.is_none() && allow_cold_start {
    use tokio::time::{sleep_until, Duration, Instant};

    const FULL_WARM_GRACE: Duration = Duration::from_millis(2500);
    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    self.replenish_notify.notify_one();

    let started = Instant::now();
    let deadline = started + FULL_WARM_GRACE;

    while Instant::now() < deadline {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return Err("Worker pool is shutting down".into());
        }

        sleep_until((Instant::now() + POLL_INTERVAL).min(deadline)).await;

        available_worker = self.pop_alive_warm_worker(&target_key).await;
        if available_worker.is_some() {
            break;
        }
    }

    eprintln!(
        "[tex:warm-grace] kind=full waited={}ms acquired={}",
        started.elapsed().as_millis(),
        available_worker.is_some()
    );
}

if let Some(worker) = available_worker {
            self.replenish_notify.notify_one();
            self.spawn_next_stage_in_bg(preamble_hash, prefix_content, prefix_hash, document_dir);

            if let Some(mut sw) = advance_to_prefix_gate(worker, prefix_content, target_key).await {
                let res = self
                    .finish_from_prefix_gate(&mut sw, suffix_content, preamble_hash, is_full_document, "warm_hit")
                    .await;
                let _ = fs::remove_dir_all(&sw.slot_dir);
                return res;
            }
        }

        self.replenish_notify.notify_one();

// 入力途中の版のために、新しいLuaLaTeXをゼロから起動しない。
// 待機ワーカーの補充はバックグラウンドで続ける。
if !allow_cold_start {
    return Err(DRAFT_DEFERRED.to_owned());
}

// 初回表示・入力停止後の全文同期は、ワーカーがなくても実行する。
self.run_raw_source(full_source, preamble_hash, true, document_dir)
    .await
    }

    fn spawn_next_stage_in_bg(
        self: &Arc<Self>,
        preamble_hash: &str,
        prefix_content: &str,
        prefix_hash: &str,
        document_dir: Option<&Path>,
    ) {
        let pool = Arc::clone(self);
        let p_hash = preamble_hash.to_string();
        let pref = prefix_content.to_string();
        let pref_h = prefix_hash.to_string();
        let doc_dir = document_dir.map(Path::to_path_buf);
        tokio::spawn(async move {
            pool.stage_prefix(&p_hash, &pref, &pref_h, doc_dir.as_deref())
                .await;
        });
    }

    async fn pop_alive_warm_worker(&self, target_key: &WarmKey) -> Option<PrewarmedWorker> {
        let mut pool = self.idle_workers.lock().await;
        while let Some(mut w) = pool.pop_front() {
            let alive = matches!(w.child.try_wait(), Ok(None));
            if &w.key == target_key && alive {
                return Some(w);
            } else {
                let _ = w.child.kill().await;
                let _ = fs::remove_dir_all(&w.slot_dir);
            }
        }
        None
    }

    async fn finish_from_prefix_gate(
        &self,
        sw: &mut StagedWorker,
        suffix_content: &str,
        preamble_hash: &str,
        is_full_document: bool,
        mode_tag: &'static str,
    ) -> Result<JobOutput, String> {
        let suffix_path = sw.slot_dir.join("suffix.tex");
        let pdf_path = sw.slot_dir.join("out.pdf");
        let synctex_path = sw.slot_dir.join("out.synctex");
        let aux_path = sw.slot_dir.join("out.aux");

        let mut suffix_buf = String::with_capacity(suffix_content.len() + 2);
        suffix_buf.push_str(suffix_content);
        if !suffix_buf.ends_with('\n') {
            suffix_buf.push('\n');
        }
        fs::write(&suffix_path, suffix_buf).map_err(|e| e.to_string())?;

        sw.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| e.to_string())?;
        sw.stdin.flush().await.map_err(|e| e.to_string())?;
        sw.stdin.shutdown().await.map_err(|e| e.to_string())?;

        let mut log_buf = String::new();
        let wait_fut = async {
            let _ = sw.stdout.read_to_string(&mut log_buf).await;
            sw.child.wait().await
        };

        match timeout(Duration::from_secs(90), wait_fut).await {
            Ok(Ok(status)) if status.success() && pdf_path.exists() => {
                if is_full_document && aux_path.exists() {
                    let cached_aux = self.aux_file_path(preamble_hash);
                    let _ = fs::copy(&aux_path, cached_aux);
                }
                let pdf_bytes = fs::read(&pdf_path).map_err(|e| e.to_string())?;
                let synctex_raw = fs::read_to_string(&synctex_path).ok();
                Ok(JobOutput {
                    pdf_bytes,
                    synctex_raw,
                    mode_tag,
                })
            }
            Ok(Ok(_)) => Err(extract_tex_error_summary(&log_buf)),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => {
                let _ = sw.child.kill().await;
                Err("lualatex timed out (90s)".into())
            }
        }
    }

    pub async fn run_raw_source(
        self: &Arc<Self>,
        full_source: &str,
        preamble_hash: &str,
        save_aux: bool,
        document_dir: Option<&Path>,
    ) -> Result<JobOutput, String> {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return Err("Worker pool is shutting down".into());
        }

        let slot_id = self.slot_counter.fetch_add(1, Ordering::Relaxed);
        let slot_dir = self.slots_root.join(format!("slot_raw_{}", slot_id));
        fs::create_dir_all(&slot_dir).map_err(|e| e.to_string())?;
        let _guard = SlotDirGuard(slot_dir.clone());

        let cached_aux = self.aux_file_path(preamble_hash);
        if cached_aux.exists() {
            let _ = fs::copy(&cached_aux, slot_dir.join("out.aux"));
        }

        let tex_path = slot_dir.join("job.tex");
        let pdf_path = slot_dir.join("out.pdf");
        let synctex_path = slot_dir.join("out.synctex");
        let aux_path = slot_dir.join("out.aux");

        let patched_source = format!("{}{}", UNCOMPRESSED_PDF_CMD, full_source);
        fs::write(&tex_path, patched_source).map_err(|e| e.to_string())?;

        let mut cmd = Command::new("lualatex");
        cmd.current_dir(&slot_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        apply_texinputs(&mut cmd, document_dir);

        cmd.args([
            "-jobname=out",
            "-interaction=nonstopmode",
            "-halt-on-error",
            "-synctex=-1",
            "job.tex",
        ]);

        let output = timeout(Duration::from_secs(120), cmd.output())
            .await
            .map_err(|_| "lualatex timed out (120s)".to_string())?
            .map_err(|e| format!("Failed to spawn lualatex: {}", e))?;

        if output.status.success() && pdf_path.exists() {
            if save_aux && aux_path.exists() {
                let _ = fs::copy(&aux_path, cached_aux);
            }
            let pdf_bytes = fs::read(&pdf_path).map_err(|e| e.to_string())?;
            let synctex_raw = fs::read_to_string(&synctex_path).ok();
            Ok(JobOutput {
                pdf_bytes,
                synctex_raw,
                mode_tag: "cold_start",
            })
        } else {
            let log = String::from_utf8_lossy(&output.stdout).into_owned();
            Err(extract_tex_error_summary(&log))
        }
    }

    fn aux_file_path(&self, preamble_hash: &str) -> PathBuf {
        self.aux_dir.join(format!("aux_{}.aux", preamble_hash))
    }

    /// 1プロセスずつ静かにバックグラウンド補充し、常に POOL_SIZE 個の待機ワーカーを維持する
    async fn replenish_loop(self: Arc<Self>) {
        loop {
            if self.is_shutting_down.load(Ordering::SeqCst) {
                break;
            }

            let cfg_opt = self.config.lock().await.clone();
            let Some(cfg) = cfg_opt else {
                self.replenish_notify.notified().await;
                continue;
            };

            let current_len = {
                let mut pool = self.idle_workers.lock().await;
                let mut alive_queue = VecDeque::new();
                while let Some(mut w) = pool.pop_front() {
                    if w.key == cfg.key && matches!(w.child.try_wait(), Ok(None)) {
                        alive_queue.push_back(w);
                    } else {
                        let _ = w.child.kill().await;
                        let _ = fs::remove_dir_all(&w.slot_dir);
                    }
                }
                *pool = alive_queue;
                pool.len()
            };

            if current_len >= POOL_SIZE {
                self.replenish_notify.notified().await;
                continue;
            }

            let ok = self.spawn_and_enqueue_prewarmed(cfg, true).await;
            if !ok {
                self.replenish_notify.notified().await;
            } else {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    async fn spawn_and_enqueue_prewarmed(&self, cfg: PoolConfig, use_cached_aux: bool) -> bool {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return false;
        }

        let slot_id = self.slot_counter.fetch_add(1, Ordering::Relaxed);
        let slot_dir = self.slots_root.join(format!("slot_warm_{}", slot_id));
        if fs::create_dir_all(&slot_dir).is_err() {
            return false;
        }

        let cached_aux = self.aux_file_path(&cfg.key.preamble_hash);
        let copied_aux = if use_cached_aux && cached_aux.exists() {
            fs::copy(&cached_aux, slot_dir.join("out.aux")).is_ok()
        } else {
            false
        };

        let init_path = slot_dir.join("preamble_init.tex");
        // 第1ゲート（PREWARM_MARKER）と第2ゲート（PREFIX_MARKER）の2段階パイプライン
        let init_src = format!(
            "{}\n{}\n\\begin{{document}}\n\
             \\begingroup\\setbox0=\\hbox{{a\\ifdefined\\ltjsetparameter あ\\fi$x+1=\\mathbf{{A}}$}}\\endgroup\n\
             \\typeout{{^^J{}}}\n\
             \\errorstopmode\n\
             \\read-1 to \\rtstageA\n\
             \\nonstopmode\n\
             \\input{{prefix.tex}}\n\
             \\typeout{{^^J{}}}\n\
             \\errorstopmode\n\
             \\read-1 to \\rtstageB\n\
             \\nonstopmode\n\
             \\input{{suffix.tex}}\n",
            cfg.preamble, UNCOMPRESSED_PDF_CMD, PREWARM_MARKER, PREFIX_MARKER
        );

        if fs::write(&init_path, init_src).is_err() {
            let _ = fs::remove_dir_all(&slot_dir);
            return false;
        }

        let mut cmd = Command::new("lualatex");
        cmd.current_dir(&slot_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        apply_texinputs(&mut cmd, cfg.key.document_dir.as_deref());

        cmd.args([
            "-jobname=out",
            "-interaction=nonstopmode",
            "-halt-on-error",
            "-synctex=-1",
            "preamble_init.tex",
        ]);

        let Ok(mut child) = cmd.spawn() else {
            let _ = fs::remove_dir_all(&slot_dir);
            return false;
        };

        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill().await;
            let _ = fs::remove_dir_all(&slot_dir);
            return false;
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill().await;
            let _ = fs::remove_dir_all(&slot_dir);
            return false;
        };

        let mut reader = BufReader::new(stdout);
        let ready = wait_for_marker(&mut reader, PREWARM_MARKER, 45).await;

        if !ready || !matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill().await;
            let _ = fs::remove_dir_all(&slot_dir);
            // 古い .aux が原因で失敗した場合は削除してクリーンに再試行
            if copied_aux {
                let _ = fs::remove_file(&cached_aux);
                return Box::pin(self.spawn_and_enqueue_prewarmed(cfg, false)).await;
            }
            return false;
        }

        let still_valid = !self.is_shutting_down.load(Ordering::SeqCst)
            && self
                .config
                .lock()
                .await
                .as_ref()
                .map(|c| c.key == cfg.key)
                .unwrap_or(false);

        if still_valid {
            let mut pool = self.idle_workers.lock().await;
            if pool.len() < POOL_SIZE {
                pool.push_back(PrewarmedWorker {
                    child,
                    stdin,
                    stdout: reader,
                    slot_dir,
                    key: cfg.key,
                });
                return true;
            }
        }

        let _ = child.kill().await;
        let _ = fs::remove_dir_all(&slot_dir);
        false
    }
}

async fn advance_to_prefix_gate(
    mut worker: PrewarmedWorker,
    prefix_content: &str,
    key: WarmKey,
) -> Option<StagedWorker> {
    let prefix_path = worker.slot_dir.join("prefix.tex");
    let mut buf = String::with_capacity(prefix_content.len() + 2);
    buf.push_str(prefix_content);
    if !buf.ends_with('\n') {
        buf.push('\n');
    }

    if fs::write(&prefix_path, buf).is_err() {
        let _ = worker.child.kill().await;
        let _ = fs::remove_dir_all(&worker.slot_dir);
        return None;
    }

    if worker.stdin.write_all(b"\n").await.is_err() || worker.stdin.flush().await.is_err() {
        let _ = worker.child.kill().await;
        let _ = fs::remove_dir_all(&worker.slot_dir);
        return None;
    }

    let ready = wait_for_marker(&mut worker.stdout, PREFIX_MARKER, 60).await;
    if !ready || !matches!(worker.child.try_wait(), Ok(None)) {
        let _ = worker.child.kill().await;
        let _ = fs::remove_dir_all(&worker.slot_dir);
        return None;
    }

    Some(StagedWorker {
        child: worker.child,
        stdin: worker.stdin,
        stdout: worker.stdout,
        slot_dir: worker.slot_dir,
        key,
    })
}

async fn cleanup_staged_entry(entry: StagedEntry) {
    entry.handle.abort();
    if let Ok(Some(mut sw)) = entry.handle.await {
        let _ = sw.child.kill().await;
        let _ = fs::remove_dir_all(&sw.slot_dir);
    }
}

async fn wait_for_marker(
    reader: &mut BufReader<ChildStdout>,
    marker: &str,
    timeout_secs: u64,
) -> bool {
    timeout(Duration::from_secs(timeout_secs), async {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => return false,
                Ok(_) => {
                    if line.contains(marker) {
                        return true;
                    }
                }
            }
        }
    })
    .await
    .unwrap_or(false)
}

fn apply_texinputs(cmd: &mut Command, document_dir: Option<&Path>) {
    if let Some(dir) = document_dir {
        let normalized = dir.to_string_lossy().replace('\\', "/");
        #[cfg(windows)]
        let texinputs = format!(".;{0};{0}//;", normalized);
        #[cfg(not(windows))]
        let texinputs = format!(".:{0}:{0}//:", normalized);
        cmd.env("TEXINPUTS", texinputs);
    }
}

fn extract_tex_error_summary(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with('!') {
            let next = lines.get(i + 1).copied().unwrap_or("");
            return format!("{} {}", line, next.trim());
        }
    }
    lines
        .last()
        .copied()
        .unwrap_or("Unknown LuaLaTeX error")
        .to_string()
}
