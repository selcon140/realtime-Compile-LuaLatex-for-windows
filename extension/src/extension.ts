import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import { CompileResult, EngineClient } from "./engineClient";

let engine: EngineClient | null = null;
let panel: vscode.WebviewPanel | null = null;
let targetDocUri: vscode.Uri | null = null;
let diagnostics: vscode.DiagnosticCollection;

let debounceTimer: NodeJS.Timeout | undefined;
let fullSettleTimer: NodeJS.Timeout | undefined;
let syncTimer: NodeJS.Timeout | undefined;

let latestCompileSeq = 0;
let inFlightCount = 0;
let wasErrorState = false;
let inTypingBurst = false;
let lastPrepareTime = 0;
let lastPreparedLine = 1;
let lastCompileStartTime = 0;

export function activate(context: vscode.ExtensionContext) {
  diagnostics = vscode.languages.createDiagnosticCollection("realtime-tex");
  context.subscriptions.push(diagnostics);

  const openCmd = vscode.commands.registerCommand("realtimeTex.openPreview", () => {
    const editor = vscode.window.activeTextEditor;
    if (!editor || !editor.document.fileName.endsWith(".tex")) {
      vscode.window.showWarningMessage("プレビューする .tex ファイルを開いてから実行してください。");
      return;
    }

    targetDocUri = editor.document.uri;
    const line = editor.selection.active.line + 1;
    ensureEngine(context);
    openOrRevealPanel(context, editor.document);
    triggerCompile(editor.document, line, false);
  });

  const changeListener = vscode.workspace.onDidChangeTextDocument((event) => {
  const doc = event.document;
  if (!engine || !isTarget(doc) || event.contentChanges.length === 0) {
    return;
  }

  const t = nowMs();
  const version = doc.version;
  const mySession = session;
  const line = getCurrentCursorLine(
    doc,
    event.contentChanges[0].range.start.line + 1
  );

  lastEdit = {
    uri: doc.uri.toString(),
    version,
    at: t,
  };

  // 古い版向けのタイマーを先に取り消す
  clearTimeout(debounceTimer);
  clearTimeout(fullSettleTimer);

  const firstInBurst = !inTypingBurst;
  inTypingBurst = true;

  // 先行組版は従来どおり、入力開始時点で発火
  if (
    firstInBurst ||
    Math.abs(line - lastPreparedLine) > 15 ||
    t - lastPrepareTime > 2500
  ) {
    lastPrepareTime = t;
    lastPreparedLine = line;

    const dir = doc.isUntitled ? undefined : path.dirname(doc.uri.fsPath);
    engine.prepare(doc.getText(), line, dir);
    void panel?.webview.postMessage({ type: "staging" });
  }

  const sendDraft = () => {
    if (session !== mySession || !isTarget(doc) || doc.version !== version) {
      return;
    }
    void triggerCompile(doc, line, true);
  };

  // 【重要】即時送信と遅延送信は排他。同じ編集で両方を実行しない
  if (
    inFlightCount === 0 &&
    (firstInBurst || t - lastCompileStartTime >= 850)
  ) {
    sendDraft();
  } else {
    const lastChange = event.contentChanges[event.contentChanges.length - 1].text;
    const boundary = /[\n}\]$。．.、,]$/.test(lastChange);
    debounceTimer = setTimeout(sendDraft, wasErrorState || boundary ? 70 : 180);
  }

  // 全文同期は「実際に部分PDFだった場合」だけ。
  // 700msを過ぎてからドラフトが返る場合は triggerCompile 側が処理する。
  fullSettleTimer = setTimeout(() => {
    if (session !== mySession || !isTarget(doc) || doc.version !== version) {
      return;
    }

    inTypingBurst = false;

    if (partialVersion === version) {
      void triggerCompile(doc, line, false);
    }
  }, 700);
});

  const activeEditorListener = vscode.window.onDidChangeActiveTextEditor((editor) => {
    if (!panel || !editor) return;
    if (editor.document.fileName.endsWith(".tex")) {
      if (targetDocUri?.toString() !== editor.document.uri.toString()) {
        resetPreviewSession();
        targetDocUri = editor.document.uri;
        panel.title = `Preview: ${path.basename(editor.document.fileName)}`;
        const line = editor.selection.active.line + 1;
        triggerCompile(editor.document, line, false);
      }
    }
  });

  const selectionListener = vscode.window.onDidChangeTextEditorSelection((event) => {
    if (!panel || !targetDocUri || !engine) return;
    if (event.textEditor.document.uri.toString() !== targetDocUri.toString()) return;

    if (syncTimer) clearTimeout(syncTimer);
    const line = event.selections[0].active.line + 1;

    // カーソルを大きく移動した時点でも先行ステージングを準備しておく
    if (!inTypingBurst && Math.abs(line - lastPreparedLine) > 20) {
      lastPreparedLine = line;
      lastPrepareTime = Date.now();
      const doc = event.textEditor.document;
      const docDir = doc.isUntitled ? undefined : path.dirname(doc.uri.fsPath);
      engine.prepare(doc.getText(), line, docDir);
    }

    syncTimer = setTimeout(async () => {
      const target = await engine?.syncToPdf(line);
      if (target && panel) {
        panel.webview.postMessage({ type: "sync", target });
      }
    }, 20);
  });

  context.subscriptions.push(openCmd, changeListener, activeEditorListener, selectionListener);
}

export function deactivate() {
  engine?.dispose();
  engine = null;
}

function getCurrentCursorLine(doc: vscode.TextDocument, fallbackLine: number): number {
  const editor = vscode.window.activeTextEditor;
  if (editor && editor.document.uri.toString() === doc.uri.toString()) {
    return editor.selection.active.line + 1;
  }
  return fallbackLine;
}

function ensureEngine(context: vscode.ExtensionContext) {
  if (engine) return;
  const customPath = vscode.workspace
    .getConfiguration("realtimeTex")
    .get<string>("enginePath");
  const binPath = EngineClient.resolveBinaryPath(context.extensionPath, customPath);
  engine = new EngineClient(binPath);
}

function openOrRevealPanel(context: vscode.ExtensionContext, doc: vscode.TextDocument) {
  if (panel) {
    panel.reveal(vscode.ViewColumn.Beside, true);
    return;
  }

  const pdfjsBuildDir = vscode.Uri.joinPath(
    context.extensionUri,
    "node_modules",
    "pdfjs-dist",
    "build"
  );
  const previewOutDir = vscode.Uri.file(
    path.join(os.tmpdir(), "realtime_tex_vscode_engine", "preview_out")
  );

  panel = vscode.window.createWebviewPanel(
    "realtimeTexPreview",
    `Preview: ${path.basename(doc.fileName)}`,
    { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
    {
      enableScripts: true,
      retainContextWhenHidden: true,
      localResourceRoots: [pdfjsBuildDir, previewOutDir],
    }
  );

  const pdfjsUri = panel.webview.asWebviewUri(
    vscode.Uri.joinPath(pdfjsBuildDir, "pdf.min.mjs")
  );
  const pdfWorkerUri = panel.webview.asWebviewUri(
    vscode.Uri.joinPath(pdfjsBuildDir, "pdf.worker.min.mjs")
  );

  panel.webview.html = getWebviewHtml(panel.webview, pdfjsUri, pdfWorkerUri);

  panel.onDidDispose(() => {
    panel = null;
    targetDocUri = null;
  });
}

async function triggerCompile(
  doc: vscode.TextDocument,
  line: number,
  draft: boolean
): Promise<void> {
  if (!engine || !panel || !isTarget(doc)) return;

  const version = doc.version;
  const uri = doc.uri.toString();
  const mySession = session;

  // await より前に記録することが重要。
  // 同じ版の全文をすでに要求した後、ドラフトに戻ることも防ぐ。
  if (draft) {
    if (draftSentVersion === version || fullSentVersion === version) return;
    draftSentVersion = version;
  } else {
    if (fullSentVersion === version) return;
    fullSentVersion = version;
  }

  const seq = ++latestCompileSeq;
  const editAt =
    lastEdit?.uri === uri && lastEdit.version === version
      ? lastEdit.at
      : undefined;

  const sentAt = nowMs();
  lastCompileStartTime = sentAt;
  inFlightCount++;

  void panel.webview.postMessage({ type: "compiling" });

  try {
    const dir = doc.isUntitled ? undefined : path.dirname(doc.uri.fsPath);
    const res: CompileResult = await engine.compile(doc.getText(), line, draft, dir);
    const receivedAt = nowMs();

    if (
      res.status === "cancelled" ||
      session !== mySession ||
      !panel ||
      !isTarget(doc)
    ) {
      return;
    }

    // 旧版でも成功した途中経過は表示できるようにする。
    // ただし、さらに新しい結果の表示後に古い結果で巻き戻さない。
    // 現在のRust側は実行1件＋待機最新1件で、結果は実行順に返る。
    const currentVersion = doc.version === version;

    if (res.status === "updated" && res.pdf_path) {
      const isDraft = res.mode === "draft_partial";

      if (currentVersion) {
        wasErrorState = false;
        diagnostics.delete(doc.uri);
        partialVersion = isDraft ? version : -1;
      }

      const pdfUri = panel.webview
        .asWebviewUri(vscode.Uri.file(res.pdf_path))
        .toString();

      paintTraces.set(seq, {
        session: mySession,
        uri,
        version,
        editAt,
        sentAt,
        receivedAt,
        engineMs: res.elapsed_ms,
      });

      // 非表示・描画キャンセルなどで応答が来ない分を無制限に残さない
      while (paintTraces.size > 32) {
        const first = paintTraces.keys().next();
        if (first.done) break;
        paintTraces.delete(first.value);
      }

      void panel.webview.postMessage({
        type: "render",
        result: res,
        pdfUri,
        isDraft,
        seq,
        version,
      });

      // ドラフトの完成が700msタイマーより遅かった場合。
      // 同じ版の full が送信済みなら関数先頭で止まる。
      if (currentVersion && isDraft && !inTypingBurst) {
        void triggerCompile(doc, line, false);
      }
    } else if (currentVersion && seq === latestCompileSeq) {
      wasErrorState = true;
      publishDiagnostics(doc, res);
      void panel.webview.postMessage({ type: "error", result: res });
    }
  } catch (err) {
    if (session === mySession && seq === latestCompileSeq) {
      void vscode.window.showErrorMessage(`Realtime TeX Engine Error: ${String(err)}`);
    }
  } finally {
    if (session === mySession) {
      inFlightCount = Math.max(0, inFlightCount - 1);
    }
  }
}

function publishDiagnostics(doc: vscode.TextDocument, res: CompileResult) {
  const list: vscode.Diagnostic[] = [];

  if (res.error_blocks.length > 0) {
    for (const b of res.error_blocks) {
      const start = Math.max(0, b.start_line - 1);
      const end = Math.max(start, b.end_line - 1);
      const range = new vscode.Range(start, 0, end, 999);
      list.push(
        new vscode.Diagnostic(
          range,
          res.error_detail ?? "LaTeX syntax error",
          vscode.DiagnosticSeverity.Error
        )
      );
    }
  } else if (res.error_detail) {
    const m = res.error_detail.match(/l\.(\d+)/);
    const lineIdx = m ? Math.max(0, parseInt(m[1], 10) - 1) : 0;
    const range = new vscode.Range(lineIdx, 0, lineIdx, 999);
    list.push(
      new vscode.Diagnostic(range, res.error_detail, vscode.DiagnosticSeverity.Error)
    );
  }

  diagnostics.set(doc.uri, list);
}

function getWebviewHtml(
  webview: vscode.Webview,
  pdfjsUri: vscode.Uri,
  pdfWorkerUri: vscode.Uri
): string {
  const csp = [
    `default-src 'none'`,
    `script-src ${webview.cspSource} 'unsafe-inline'`,
    `style-src 'unsafe-inline'`,
    `worker-src ${webview.cspSource} blob:`,
    `connect-src ${webview.cspSource} data: blob:`,
    `img-src ${webview.cspSource} data: blob:`,
  ].join("; ");

  return `<!doctype html>
<html lang="ja">
<head>
  <meta charset="UTF-8" />
  <meta http-equiv="Content-Security-Policy" content="${csp}" />
  <style>
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      height: 100vh;
      display: flex;
      flex-direction: column;
      background: #525659;
      color: #d4d4d4;
      font-family: system-ui, sans-serif;
      overflow: hidden;
    }
    .topbar {
      display: flex;
      align-items: center;
      justify-content: space-between;
      padding: 6px 12px;
      background: #252526;
      border-bottom: 1px solid #3c3c3c;
      font-size: 12px;
      gap: 10px;
    }
    .badge {
      padding: 2px 8px;
      border-radius: 4px;
      font-weight: 600;
      background: #444;
      color: #fff;
    }
    .badge[data-state="ok"] { background: #2e7d32; }
    .badge[data-state="staging"] { background: #0284c7; }
    .badge[data-state="compiling"] { background: #d97706; }
    .badge[data-state="error"] { background: #c62828; }
    .metrics {
      display: flex;
      gap: 12px;
      font-family: Consolas, monospace;
      color: #aaa;
    }
    .metrics strong { color: #fff; }
    .preview-pane {
      flex: 1;
      overflow-y: auto;
      padding: 16px;
      display: flex;
      flex-direction: column;
      align-items: center;
      gap: 14px;
    }
    .pdf-page-wrap { position: relative; }
    canvas {
      background: #fff;
      box-shadow: 0 4px 12px rgba(0,0,0,0.35);
      max-width: 100%;
      height: auto;
      display: block;
    }
    .sync-marker {
      position: absolute;
      left: 0; right: 0; height: 22px;
      background: rgba(59, 130, 246, 0.35);
      border-left: 4px solid #3b82f6;
      pointer-events: none;
      opacity: 0;
    }
    .sync-marker.flash { animation: syncFlash 0.9s ease-out forwards; }
    @keyframes syncFlash { 0% { opacity: 1; } 100% { opacity: 0; } }
    .error-bar {
      display: none;
      padding: 6px 12px;
      background: #3b1212;
      border-top: 1px solid #7f1d1d;
      color: #fecaca;
      font-size: 12px;
      font-family: Consolas, monospace;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .error-bar.visible { display: block; }
  </style>
</head>
<body>
  <header class="topbar">
    <div id="status" class="badge" data-state="ok">Ready</div>
    <div class="metrics">
      <span>Mode: <strong id="m-mode">-</strong></span>
      <span>Hit: <strong id="m-hits">0</strong></span>
      <span>Miss: <strong id="m-misses">0</strong></span>
      <span>Time: <strong id="m-time">0 ms</strong></span>
    </div>
  </header>
  <main id="preview" class="preview-pane"></main>
  <footer id="error-bar" class="error-bar"></footer>

  <script type="module">
    import * as pdfjsLib from "${pdfjsUri}";
    pdfjsLib.GlobalWorkerOptions.workerSrc = "${pdfWorkerUri}";

    const PDF_SCALE = 1.4;
    const statusEl = document.getElementById("status");
    const previewEl = document.getElementById("preview");
    const errorBar = document.getElementById("error-bar");
    const mMode = document.getElementById("m-mode");
    const mHits = document.getElementById("m-hits");
    const mMisses = document.getElementById("m-misses");
    const mTime = document.getElementById("m-time");

    let renderGeneration = 0;

    function ensurePageSlots(numPages, isDraft) {
      const existing = previewEl.querySelectorAll(".pdf-page-wrap");
      for (let i = existing.length; i < numPages; i++) {
        const wrap = document.createElement("div");
        wrap.className = "pdf-page-wrap";
        wrap.dataset.page = String(i + 1);
        const canvas = document.createElement("canvas");
        wrap.appendChild(canvas);
        previewEl.appendChild(wrap);
      }
      // 部分ドラフト更新中は後ろのページCanvasを消さずに維持し、スクロール飛びを防ぐ
      if (!isDraft) {
        for (let i = existing.length - 1; i >= numPages; i--) {
          existing[i].remove();
        }
      }
    }

    function getPrioritizedPages(numPages) {
      const viewTop = previewEl.scrollTop;
      const viewBottom = viewTop + previewEl.clientHeight;
      const wraps = previewEl.querySelectorAll(".pdf-page-wrap");

      const visible = [];
      const others = [];

      for (let p = 1; p <= numPages; p++) {
        const wrap = wraps[p - 1];
        if (!wrap || wrap.offsetHeight === 0) {
          (p <= 2 ? visible : others).push(p);
          continue;
        }
        const top = wrap.offsetTop;
        const bottom = top + wrap.offsetHeight;
        if (bottom >= viewTop - 400 && top <= viewBottom + 400) {
          visible.push(p);
        } else {
          others.push(p);
        }
      }
      return { visible, others };
    }

    async function renderSinglePage(pdf, pageNum, gen) {
      if (gen !== renderGeneration) return;
      const page = await pdf.getPage(pageNum);
      if (gen !== renderGeneration) return;

      const viewport = page.getViewport({ scale: PDF_SCALE });
      const offscreen = document.createElement("canvas");
      offscreen.width = viewport.width;
      offscreen.height = viewport.height;
      const offCtx = offscreen.getContext("2d");

      await page.render({ canvasContext: offCtx, viewport }).promise;
      if (gen !== renderGeneration) return;

      const wrap = previewEl.querySelector('.pdf-page-wrap[data-page="' + pageNum + '"]');
      if (!wrap) return;
      const canvas = wrap.querySelector("canvas");
      if (canvas.width !== viewport.width || canvas.height !== viewport.height) {
        canvas.width = viewport.width;
        canvas.height = viewport.height;
      }
      const ctx = canvas.getContext("2d");
      ctx.drawImage(offscreen, 0, 0);
    }

    async function renderPdfFromUri(pdfUri, isDraft) {
      const gen = ++renderGeneration;
      const resp = await fetch(pdfUri);
      const buf = await resp.arrayBuffer();
      if (gen !== renderGeneration) return;

      const pdf = await pdfjsLib.getDocument({ data: new Uint8Array(buf) }).promise;
      if (gen !== renderGeneration) return;

      ensurePageSlots(pdf.numPages, isDraft);
      const { visible, others } = getPrioritizedPages(pdf.numPages);

      await Promise.all(visible.map((p) => renderSinglePage(pdf, p, gen)));
      if (gen !== renderGeneration) return;

      for (const p of others) {
        if (gen !== renderGeneration) return;
        await new Promise((r) => setTimeout(r, 0));
        await renderSinglePage(pdf, p, gen);
      }
    }

    function scrollToTarget(target) {
      const wrap = previewEl.querySelector('.pdf-page-wrap[data-page="' + target.page + '"]');
      if (!wrap) return;
      const yPx = target.y_pt * PDF_SCALE;
      const desired = Math.max(0, wrap.offsetTop + yPx - previewEl.clientHeight * 0.35);
      const dist = Math.abs(previewEl.scrollTop - desired);

      previewEl.scrollTo({
        top: desired,
        behavior: dist < 240 ? "auto" : "smooth",
      });

      let marker = wrap.querySelector(".sync-marker");
      if (!marker) {
        marker = document.createElement("div");
        marker.className = "sync-marker";
        wrap.appendChild(marker);
      }
      marker.style.top = Math.max(0, yPx - 12) + "px";
      marker.classList.remove("flash");
      void marker.offsetWidth;
      marker.classList.add("flash");
    }

    window.addEventListener("message", async (event) => {
      const msg = event.data;
      if (msg.type === "staging") {
        statusEl.textContent = "Pre-compiling...";
        statusEl.dataset.state = "staging";
      } else if (msg.type === "compiling") {
        statusEl.textContent = "Compiling...";
        statusEl.dataset.state = "compiling";
      } else if (msg.type === "render") {
        const r = msg.result;
        mMode.textContent = r.mode;
        mHits.textContent = String(r.cache_hits);
        mMisses.textContent = String(r.cache_misses);
        mTime.textContent = r.elapsed_ms + " ms";
        errorBar.classList.remove("visible");

        await renderPdfFromUri(msg.pdfUri, Boolean(msg.isDraft));
        statusEl.textContent = msg.isDraft ? "Live Draft" : "Synced";
        statusEl.dataset.state = "ok";
      } else if (msg.type === "error") {
        const r = msg.result;
        mTime.textContent = r.elapsed_ms + " ms";
        statusEl.textContent = "Error (Holding Preview)";
        statusEl.dataset.state = "error";
        errorBar.textContent = r.error_detail || "LuaLaTeX compile error";
        errorBar.classList.add("visible");
      } else if (msg.type === "sync") {
        scrollToTarget(msg.target);
      }
    });
  </script>
</body>
</html>`;
}
