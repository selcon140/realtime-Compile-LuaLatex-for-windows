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
use tokio::time::timeout;

const POOL_SIZE: usize = 2;
const PREWARM_MARKER: &str = "@@PREWARM_READY@@";
const UNCOMPRESSED_PDF_CMD: &str =
    "\\pdfvariable compresslevel=0 \\pdfvariable objcompresslevel=0\n";

pub struct JobOutput {
    pub pdf_bytes: Vec<u8>,
    pub synctex_raw: Option<String>,
    pub used_prewarm: bool,
}

struct SlotDirGuard(PathBuf);

impl Drop for SlotDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// フォアグラウンド実行中カウンタの RAII ガード（キャンセル時も確実に解除して裏補充を再開）
struct ForegroundGuard {
    active_fg: Arc<AtomicUsize>,
    fg_done_notify: Arc<Notify>,
}

impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        if self.active_fg.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.fg_done_notify.notify_waiters();
        }
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

struct PrewarmedWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    slot_dir: PathBuf,
    key: WarmKey,
}

pub struct LuaTexWorkerPool {
    slots_root: PathBuf,
    config: Mutex<Option<PoolConfig>>,
    idle_workers: Mutex<VecDeque<PrewarmedWorker>>,
    slot_counter: AtomicUsize,
    is_shutting_down: AtomicBool,
    active_fg: Arc<AtomicUsize>,
    replenish_notify: Arc<Notify>,
    fg_done_notify: Arc<Notify>,
}

impl LuaTexWorkerPool {
    pub fn new(work_dir: &Path) -> Arc<Self> {
        let slots_root = work_dir.join("worker_slots");
        if slots_root.exists() {
            let _ = fs::remove_dir_all(&slots_root);
        }
        fs::create_dir_all(&slots_root).unwrap();

        let pool = Arc::new(Self {
            slots_root,
            config: Mutex::new(None),
            idle_workers: Mutex::new(VecDeque::new()),
            slot_counter: AtomicUsize::new(0),
            is_shutting_down: AtomicBool::new(false),
            active_fg: Arc::new(AtomicUsize::new(0)),
            replenish_notify: Arc::new(Notify::new()),
            fg_done_notify: Arc::new(Notify::new()),
        });

        // 補充プロセスを同時に1つだけ直列実行する専用ループを起動
        tokio::spawn(Arc::clone(&pool).replenish_loop());
        pool
    }

    pub async fn shutdown(&self) {
        self.is_shutting_down.store(true, Ordering::SeqCst);
        self.replenish_notify.notify_waiters();
        self.fg_done_notify.notify_waiters();

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

    /// \begin{document} 通過済みの待機ワーカーがあれば即実行、なければ full_source でコールドスタート
    pub async fn run_job(
        self: &Arc<Self>,
        preamble_hash: &str,
        body_inner: &str,
        full_source: &str,
        document_dir: Option<&Path>,
    ) -> Result<JobOutput, String> {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return Err("Worker pool is shutting down".into());
        }

        // フォアグラウンド実行中フラグを立て、裏の補充と CPU を奪い合わないようにする
        self.active_fg.fetch_add(1, Ordering::SeqCst);
        let _fg_guard = ForegroundGuard {
            active_fg: Arc::clone(&self.active_fg),
            fg_done_notify: Arc::clone(&self.fg_done_notify),
        };

        let target_key = WarmKey {
            preamble_hash: preamble_hash.to_string(),
            document_dir: document_dir.map(Path::to_path_buf),
        };

        let warm_worker = {
            let mut pool = self.idle_workers.lock().await;
            let mut found = None;
            while let Some(mut w) = pool.pop_front() {
                let alive = matches!(w.child.try_wait(), Ok(None));
                if w.key == target_key && alive {
                    found = Some(w);
                    break;
                } else {
                    let _ = w.child.kill().await;
                    let _ = fs::remove_dir_all(&w.slot_dir);
                }
            }
            found
        };

        // 消費したら補充ループへ通知（実際の起動は _fg_guard 解放後に開始される）
        self.replenish_notify.notify_one();

        if let Some(mut worker) = warm_worker {
            let _guard = SlotDirGuard(worker.slot_dir.clone());
            let tex_path = worker.slot_dir.join("job.tex");
            let pdf_path = worker.slot_dir.join("out.pdf");
            let synctex_path = worker.slot_dir.join("out.synctex");

            fs::write(&tex_path, body_inner).map_err(|e| e.to_string())?;

            if worker.stdin.write_all(b"\n").await.is_ok()
                && worker.stdin.flush().await.is_ok()
                && worker.stdin.shutdown().await.is_ok()
            {
                let mut log_buf = String::new();
                let wait_fut = async {
                    let _ = worker.stdout.read_to_string(&mut log_buf).await;
                    worker.child.wait().await
                };

                match timeout(Duration::from_secs(120), wait_fut).await {
                    Ok(Ok(status)) if status.success() && pdf_path.exists() => {
                        let pdf_bytes = fs::read(&pdf_path).map_err(|e| e.to_string())?;
                        let synctex_raw = fs::read_to_string(&synctex_path).ok();
                        return Ok(JobOutput {
                            pdf_bytes,
                            synctex_raw,
                            used_prewarm: true,
                        });
                    }
                    Ok(Ok(_)) => {
                        return Err(extract_tex_error_summary(&log_buf));
                    }
                    Ok(Err(e)) => return Err(e.to_string()),
                    Err(_) => {
                        let _ = worker.child.kill().await;
                        return Err("lualatex timed out (120s)".into());
                    }
                }
            }
        }

        // プールが空の場合はコールドスタートで実行
        self.run_raw_source(full_source, document_dir).await
    }

    pub async fn run_raw_source(
        self: &Arc<Self>,
        full_source: &str,
        document_dir: Option<&Path>,
    ) -> Result<JobOutput, String> {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return Err("Worker pool is shutting down".into());
        }

        let slot_id = self.slot_counter.fetch_add(1, Ordering::Relaxed);
        let slot_dir = self.slots_root.join(format!("slot_raw_{}", slot_id));
        fs::create_dir_all(&slot_dir).map_err(|e| e.to_string())?;
        let _guard = SlotDirGuard(slot_dir.clone());

        let tex_path = slot_dir.join("job.tex");
        let pdf_path = slot_dir.join("out.pdf");
        let synctex_path = slot_dir.join("out.synctex");

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
            let pdf_bytes = fs::read(&pdf_path).map_err(|e| e.to_string())?;
            let synctex_raw = fs::read_to_string(&synctex_path).ok();
            Ok(JobOutput {
                pdf_bytes,
                synctex_raw,
                used_prewarm: false,
            })
        } else {
            let log = String::from_utf8_lossy(&output.stdout).into_owned();
            Err(extract_tex_error_summary(&log))
        }
    }

    /// フォアグラウンドが空いているときだけ1プロセスずつ静かに補充するループ
    async fn replenish_loop(self: Arc<Self>) {
        loop {
            if self.is_shutting_down.load(Ordering::SeqCst) {
                break;
            }

            while self.active_fg.load(Ordering::SeqCst) > 0 {
                if self.is_shutting_down.load(Ordering::SeqCst) {
                    return;
                }
                self.fg_done_notify.notified().await;
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

            let ok = self.spawn_and_enqueue_prewarmed(cfg).await;
            if !ok {
                // プリアンブルエラー時などの無限リトライを防止
                self.replenish_notify.notified().await;
            }
        }
    }

    /// \begin{document} 通過 + フォント事前ロード済みの待機ワーカーを生成
    async fn spawn_and_enqueue_prewarmed(&self, cfg: PoolConfig) -> bool {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return false;
        }

        let slot_id = self.slot_counter.fetch_add(1, Ordering::Relaxed);
        let slot_dir = self.slots_root.join(format!("slot_warm_{}", slot_id));
        if fs::create_dir_all(&slot_dir).is_err() {
            return false;
        }

        let init_path = slot_dir.join("preamble_init.tex");
        // \begin{document} の AtBeginDocument フックと和文・数式フォントを事前ロードしきってから待機
        let init_src = format!(
            "{}\n{}\n\\begin{{document}}\n\
             \\begingroup\\setbox0=\\hbox{{a\\ifdefined\\ltjsetparameter あ\\fi$x+1=\\mathbf{{A}}$}}\\endgroup\n\
             \\typeout{{{}}}\n\
             \\errorstopmode\n\
             \\read-1 to \\rttrigger\n\
             \\nonstopmode\n\
             \\input{{job.tex}}\n",
            cfg.preamble, UNCOMPRESSED_PDF_CMD, PREWARM_MARKER
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
        let ready = timeout(Duration::from_secs(45), async {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return false,
                    Ok(_) => {
                        if line.contains(PREWARM_MARKER) {
                            return true;
                        }
                    }
                }
            }
        })
        .await
        .unwrap_or(false);

        if !ready || !matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill().await;
            let _ = fs::remove_dir_all(&slot_dir);
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
