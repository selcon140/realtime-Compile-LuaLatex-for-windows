use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;

const DEFERRED_PACKAGES: &[&str] = &["hyperref", "bookmark", "cleveref", "preview"];

pub struct JobOutput {
    pub pdf_bytes: Vec<u8>,
    pub synctex_gz: Option<Vec<u8>>,
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

#[derive(Clone)]
struct FormatState {
    hash: String,
    use_fmt: bool,
    dumpable_preamble: String,
}

pub struct LuaTexWorkerPool {
    fmt_dir: PathBuf,
    slots_root: PathBuf,
    format_state: Mutex<Option<FormatState>>,
    slot_counter: AtomicUsize,
    is_shutting_down: AtomicBool,
}

impl LuaTexWorkerPool {
    pub fn new(work_dir: &Path) -> Arc<Self> {
        let fmt_dir = work_dir.join("formats");
        let slots_root = work_dir.join("worker_slots");

        if slots_root.exists() {
            let _ = fs::remove_dir_all(&slots_root);
        }
        fs::create_dir_all(&fmt_dir).unwrap();
        fs::create_dir_all(&slots_root).unwrap();

        Arc::new(Self {
            fmt_dir,
            slots_root,
            format_state: Mutex::new(None),
            slot_counter: AtomicUsize::new(0),
            is_shutting_down: AtomicBool::new(false),
        })
    }

    pub async fn shutdown(&self) {
        self.is_shutting_down.store(true, Ordering::SeqCst);
        if self.slots_root.exists() {
            let _ = fs::remove_dir_all(&self.slots_root);
        }
    }

    pub async fn sync_format(
        self: &Arc<Self>,
        dumpable_preamble: &str,
        fmt_hash: &str,
    ) -> Result<(), String> {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return Err("Worker pool is shutting down".into());
        }

        {
            let guard = self.format_state.lock().await;
            if let Some(ref st) = *guard {
                if st.hash == fmt_hash {
                    return Ok(());
                }
            }
        }

        let is_luatexja = dumpable_preamble.contains("ltjs")
            || dumpable_preamble.contains("luatexja");

        let use_fmt = if is_luatexja {
            false
        } else {
            let fmt_name = format!("fmt_{}", fmt_hash);
            let fmt_path = self.fmt_dir.join(format!("{}.fmt", fmt_name));
            if fmt_path.exists() {
                true
            } else {
                self.build_format_file(dumpable_preamble, &fmt_name)
                    .await
                    .is_ok()
            }
        };

        let mut guard = self.format_state.lock().await;
        *guard = Some(FormatState {
            hash: fmt_hash.to_string(),
            use_fmt,
            dumpable_preamble: dumpable_preamble.to_string(),
        });

        Ok(())
    }

    /// 分割コンパイル用ジョブ実行（document_dir を TEXINPUTS に渡してローカル画像等を解決）
    pub async fn run_job(
        self: &Arc<Self>,
        fmt_hash: &str,
        job_body: &str,
        document_dir: Option<&Path>,
    ) -> Result<JobOutput, String> {
        if self.is_shutting_down.load(Ordering::SeqCst) {
            return Err("Worker pool is shutting down".into());
        }

        let state = {
            let guard = self.format_state.lock().await;
            guard.clone().ok_or("Format state not initialized")?
        };

        let slot_id = self.slot_counter.fetch_add(1, Ordering::Relaxed);
        let slot_dir = self.slots_root.join(format!("slot_{}", slot_id));
        fs::create_dir_all(&slot_dir).map_err(|e| e.to_string())?;

        let tex_path = slot_dir.join("job.tex");
        let pdf_path = slot_dir.join("out.pdf");
        let synctex_path = slot_dir.join("out.synctex.gz");

        let full_source = if state.use_fmt {
            job_body.to_string()
        } else {
            format!("{}\n{}", state.dumpable_preamble, job_body)
        };
        fs::write(&tex_path, full_source).map_err(|e| e.to_string())?;

        let mut cmd = Command::new("lualatex");
        cmd.current_dir(&slot_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        apply_texinputs(&mut cmd, document_dir);

        if state.use_fmt {
            #[cfg(windows)]
            let texformats_env = format!("{};", self.fmt_dir.display());
            #[cfg(not(windows))]
            let texformats_env = format!("{}:", self.fmt_dir.display());

            cmd.env("TEXFORMATS", texformats_env)
                .arg(format!("-fmt=fmt_{}", fmt_hash));
        }

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

        let result = if output.status.success() && pdf_path.exists() {
            let pdf_bytes = fs::read(&pdf_path).map_err(|e| e.to_string())?;
            let synctex_gz = fs::read(&synctex_path).ok();
            Ok(JobOutput {
                pdf_bytes,
                synctex_gz,
            })
        } else {
            let log = String::from_utf8_lossy(&output.stdout).into_owned();
            Err(extract_tex_error_summary(&log))
        };

        let _ = fs::remove_dir_all(&slot_dir);
        result
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

        let result = if output.status.success() && pdf_path.exists() {
            let pdf_bytes = fs::read(&pdf_path).map_err(|e| e.to_string())?;
            let synctex_gz = fs::read(&synctex_path).ok();
            Ok(JobOutput {
                pdf_bytes,
                synctex_gz,
            })
        } else {
            let log = String::from_utf8_lossy(&output.stdout).into_owned();
            Err(extract_tex_error_summary(&log))
        };

        let _ = fs::remove_dir_all(&slot_dir);
        result
    }

    async fn build_format_file(&self, dumpable_preamble: &str, fmt_name: &str) -> Result<(), String> {
        let src_path = self.fmt_dir.join(format!("{}.tex", fmt_name));
        let dump_source = format!("{}\n\\endofdump\n", dumpable_preamble);
        fs::write(&src_path, dump_source).map_err(|e| e.to_string())?;

        let mut cmd = Command::new("lualatex");
        cmd.current_dir(&self.fmt_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .args([
                "-ini",
                "-interaction=nonstopmode",
                "-halt-on-error",
                &format!("-jobname={}", fmt_name),
                "&lualatex",
                "mylatexformat.ltx",
                &format!("{}.tex", fmt_name),
            ]);

        let output = timeout(Duration::from_secs(20), cmd.output())
            .await
            .map_err(|_| "Format build timed out".to_string())?
            .map_err(|e| e.to_string())?;

        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stdout).into_owned())
        }
    }
}

/// 編集中の .tex ファイルのディレクトリを TEXINPUTS へ設定（JPEG/PNG や \input を自動解決）
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
