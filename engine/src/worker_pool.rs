use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

const POOL_SIZE: usize = 2;
const DEFERRED_PACKAGES: &[&str] = &["hyperref", "bookmark", "cleveref", "preview"];
const PREWARM_MARKER: &str = "@@PREWARM_READY@@";

pub struct JobOutput {
    pub pdf_bytes: Vec<u8>,
    pub synctex_gz: Option<Vec<u8>>,
}

/// タスクがキャンセル（abort）されても一時スロットディレクトリを確実に削除する RAII ガード
struct SlotDirGuard(PathBuf);

impl Drop for SlotDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn split_preamble_for_format(preamble: &str) -> (String, String) {
    let mut dumpable = String::new();
    let mut deferred = String::new();

    for line in preamble.lines() {
        let is_deferred = DEFERRED_PACKAGES
            .iter()
            .any(|pkg| line.contains(&format!("{{{}}}", pkg)));
        if is_deferred {
            deferred.push_str(line);
            deferred.push('\n');
        } else {
            dumpable.push_str(line);
            dumpable.push('\n');
        }
    }
    (dumpable, deferred)
}

#[derive(Clone, PartialEq, Eq)]
struct WarmKey {
    preamble_hash: String,
    document_dir: Option<PathBuf>,
}

#[derive(Clone)]
struct PoolConfig {
    key: WarmKey,
    dumpable_preamble: String,
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
}

impl LuaTexWorkerPool {
    pub fn new(work_dir: &Path) -> Arc<Self> {
        let slots_root = work_dir.join("worker_slots");
        if slots_root.exists() {
            let _ = fs::remove_dir_all(&slots_root);
        }
        fs::create_dir_all(&slots_root).unwrap();

        Arc::new(Self {
            slots_root,
            config: Mutex::new(None),
            idle_workers: Mutex::new(VecDeque::new()),
            slot_counter: AtomicUsize::new(0),
            is_shutting_down: AtomicBool::new(false),
        })
    }

    pub async fn shutdown(&self) {
        self.is_shutting_down.store(true, Ordering::SeqCst);

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

    /// プリアンブルまたは作業ディレクトリが変わったら待機ワーカーを入れ替えて先行起動
    pub async fn sync_preamble(
        self: &Arc<Self>,
        dumpable_preamble: &str,
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

        // 古い待機ワーカーを破棄
        let stale: Vec<PrewarmedWorker> = {
            let mut pool = self.idle_workers.lock().await;
            pool.drain(..).collect()
        };
        for mut w in stale {
            let _ = w.child.kill().await;
            let _ = fs::remove_dir_all(&w.slot_dir);
        }

        let new_cfg = PoolConfig {
            key: new_key,
            dumpable_preamble: dumpable_preamble.to_string(),
        };
        {
            let mut guard = self.config.lock().await;
            *guard = Some(new_cfg.clone());
        }

        // バックグラウンドでメモリ常駐ワーカーを先行起動
        for _ in 0..POOL_SIZE {
            let pool_clone = Arc::clone(self);
            let cfg_clone = new_cfg.clone();
            tokio::spawn(async move {
                pool_clone.spawn_and_enqueue_prewarmed(cfg_clone).await;
            });
        }

        Ok(())
    }

    /// プリウォーム済みワーカーがあれば即座に使い、なければ直接起動で実行する
    pub async fn run_job(
        self: &Arc<Self>,
        preamble_hash: &str,
        job_body: &str,
        document_dir: Option<&Path>,
    ) -> Result<JobOutput, String> {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return Err("Worker pool is shutting down".into());
        }

        let target_key = WarmKey {
            preamble_hash: preamble_hash.to_string(),
            document_dir: document_dir.map(Path::to_path_buf),
        };

        let cfg = {
            let guard = self.config.lock().await;
            guard.clone().ok_or("Pool config not initialized")?
        };

        // 生存しているプリウォーム済みワーカーをプールから1つ取り出す
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

        // 消費した分を裏で即座に1プロセス補充
        if warm_worker.is_some() {
            let pool_clone = Arc::clone(self);
            let cfg_clone = cfg.clone();
            tokio::spawn(async move {
                pool_clone.spawn_and_enqueue_prewarmed(cfg_clone).await;
            });
        }

        if let Some(mut worker) = warm_worker {
            let _guard = SlotDirGuard(worker.slot_dir.clone());
            let tex_path = worker.slot_dir.join("job.tex");
            let pdf_path = worker.slot_dir.join("out.pdf");
            let synctex_path = worker.slot_dir.join("out.synctex.gz");

            fs::write(&tex_path, job_body).map_err(|e| e.to_string())?;

            // メモリ上に展開済みのワーカーへ本文の \input 命令を流し込んで stdin を閉じる
            if worker.stdin.write_all(b"\\input{job.tex}\n").await.is_ok()
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
                        let synctex_gz = fs::read(&synctex_path).ok();
                        return Ok(JobOutput {
                            pdf_bytes,
                            synctex_gz,
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

        // プールが空だった場合（初回起動直後など）は直接実行
        let full_source = format!("{}\n{}", cfg.dumpable_preamble, job_body);
        self.run_raw_source(&full_source, document_dir).await
    }

    /// 生ソース直接コンパイル（フォールバック用・ローカル画像対応）
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
        let synctex_path = slot_dir.join("out.synctex.gz");
        fs::write(&tex_path, full_source).map_err(|e| e.to_string())?;

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
            "-synctex=1",
            "job.tex",
        ]);

        let output = timeout(Duration::from_secs(120), cmd.output())
            .await
            .map_err(|_| "lualatex timed out (120s)".to_string())?
            .map_err(|e| format!("Failed to spawn lualatex: {}", e))?;

        if output.status.success() && pdf_path.exists() {
            let pdf_bytes = fs::read(&pdf_path).map_err(|e| e.to_string())?;
            let synctex_gz = fs::read(&synctex_path).ok();
            Ok(JobOutput {
                pdf_bytes,
                synctex_gz,
            })
        } else {
            let log = String::from_utf8_lossy(&output.stdout).into_owned();
            Err(extract_tex_error_summary(&log))
        }
    }

    /// バックグラウンドでプリアンブルをメモリ展開し、stdin 待機状態のワーカーを生成してプールへ積む
    async fn spawn_and_enqueue_prewarmed(&self, cfg: PoolConfig) {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return;
        }

        let slot_id = self.slot_counter.fetch_add(1, Ordering::Relaxed);
        let slot_dir = self.slots_root.join(format!("slot_warm_{}", slot_id));
        if fs::create_dir_all(&slot_dir).is_err() {
            return;
        }

        let init_path = slot_dir.join("preamble_init.tex");
        let init_src = format!(
            "{}\n\\typeout{{{}}}\n",
            cfg.dumpable_preamble, PREWARM_MARKER
        );
        if fs::write(&init_path, init_src).is_err() {
            let _ = fs::remove_dir_all(&slot_dir);
            return;
        }

        let mut cmd = Command::new("lualatex");
        cmd.current_dir(&slot_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        apply_texinputs(&mut cmd, cfg.key.document_dir.as_deref());

        // ファイル引数を渡さずに起動することで、stdin がメイン入力ストリームとして待機状態になる
        cmd.args([
            "-jobname=out",
            "-interaction=nonstopmode",
            "-halt-on-error",
            "-synctex=1",
        ]);

        let Ok(mut child) = cmd.spawn() else {
            let _ = fs::remove_dir_all(&slot_dir);
            return;
        };

        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill().await;
            let _ = fs::remove_dir_all(&slot_dir);
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill().await;
            let _ = fs::remove_dir_all(&slot_dir);
            return;
        };

        // プリアンブルを読み込ませてフラッシュ（stdin は閉じない）
        if stdin.write_all(b"\\input{preamble_init.tex}\n").await.is_err()
            || stdin.flush().await.is_err()
        {
            let _ = child.kill().await;
            let _ = fs::remove_dir_all(&slot_dir);
            return;
        }

        // @@PREWARM_READY@@ が出力されるまで待機（メモリ展開完了の確認）
        let mut reader = BufReader::new(stdout);
        let ready = timeout(Duration::from_secs(30), async {
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
            return;
        }

        // 待機中にプリアンブルが変更・終了していなければプールへ登録
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
                return;
            }
        }

        let _ = child.kill().await;
        let _ = fs::remove_dir_all(&slot_dir);
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
