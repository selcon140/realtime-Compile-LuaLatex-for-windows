import * as os from "os";
import * as path from "path";
import { randomBytes } from "crypto";
import * as vscode from "vscode";
import { EngineClient } from "./engineClient";
import type { CompileResult } from "./engineClient";

// ─────────────────────────────────────────────────────────────
// 拡張機能側の状態
// ─────────────────────────────────────────────────────────────

let engine: EngineClient | null = null;
let panel: vscode.WebviewPanel | null = null;
let targetDocUri: vscode.Uri | null = null;
let diagnostics: vscode.DiagnosticCollection;

let debounceTimer: NodeJS.Timeout | undefined;
let fullSettleTimer: NodeJS.Timeout | undefined;
let syncTimer: NodeJS.Timeout | undefined;

let session = 0;
let nextCompileSeq = 0;
let latestRequestSeq = 0;
let lastPublishedSeq = 0;
let selectionSeq = 0;

let inFlightCount = 0;
let inTypingBurst = false;
let wasErrorState = false;
let lastPrepareTime = 0;
let lastPreparedLine = 1;
let lastCompileStartTime = 0;

// 文書の版ごとに draft と full を区別する。
// 同じ版で draft → full は許可するが、draft → draft は送らない。
let draftSentVersion = -1;
let fullSentVersion = -1;
let partialVersion = -1;

type EditStamp = {
  uri: string;
  version: number;
  at: number;
};

type PaintTrace = {
  session: number;
  version: number;
  editAt?: number;
  sentAt: number;
  receivedAt: number;
  engineMs: number;
};

type PanelMessage = {
  type: string;
  session: number;
  [key: string]: unknown;
};

let lastEdit: EditStamp | undefined;
const paintTraces = new Map<number, PaintTrace>();

// 初回のPDFがWebviewの起動より先に完成した場合に備える。
let webviewReady = false;
let pendingPanelMessages: PanelMessage[] = [];

const LIVE_INTERVAL_MS = 850;
const SETTLE_MS = 700;

function nowMs(): number {
  return Number(process.hrtime.bigint()) / 1_000_000;
}

function docDir(doc: vscode.TextDocument): string | undefined {
  return doc.isUntitled ? undefined : path.dirname(doc.uri.fsPath);
}

function isTarget(doc: vscode.TextDocument): boolean {
  return !!panel && targetDocUri?.toString() === doc.uri.toString();
}

function resetSession(): void {
  session++;
  selectionSeq++;

  clearTimeout(debounceTimer);
  clearTimeout(fullSettleTimer);
  clearTimeout(syncTimer);

  inFlightCount = 0;
  inTypingBurst = false;
  wasErrorState = false;
  lastPrepareTime = 0;
  lastPreparedLine = 1;
  lastCompileStartTime = 0;

  draftSentVersion = -1;
  fullSentVersion = -1;
  partialVersion = -1;
  latestRequestSeq = 0;
  lastPublishedSeq = 0;
  lastEdit = undefined;

  paintTraces.clear();
  pendingPanelMessages = [];
}

function post(type: string, fields: Record<string, unknown> = {}): void {
  if (!panel) return;

  const message: PanelMessage = { ...fields, type, session };

  if (!webviewReady) {
    // 初期化完了前のカーソル移動・ステータス通知を溜めない。
    // reset と、最後に完成したPDFまたはエラーだけを保持する。
    if (type === "reset") {
      pendingPanelMessages = [message];
    } else if (type === "render" || type === "error") {
      pendingPanelMessages = pendingPanelMessages.filter(
        (m) => m.type !== type
      );
      pendingPanelMessages.push(message);
    }
    return;
  }

  void panel.webview.postMessage(message);
}

function currentLine(doc: vscode.TextDocument, fallback: number): number {
  const editor = vscode.window.activeTextEditor;
  const line =
    editor && editor.document.uri.toString() === doc.uri.toString()
      ? editor.selection.active.line + 1
      : fallback;

  return Math.min(doc.lineCount, Math.max(1, line));
}

function ensureEngine(context: vscode.ExtensionContext): EngineClient {
  if (!engine) {
    const customPath = vscode.workspace
      .getConfiguration("realtimeTex")
      .get<string>("enginePath");

    const binaryPath = EngineClient.resolveBinaryPath(
      context.extensionPath,
      customPath
    );
    engine = new EngineClient(binaryPath);
  }
  return engine;
}

function prepare(doc: vscode.TextDocument, line: number): void {
  if (!engine || !isTarget(doc)) return;

  lastPrepareTime = nowMs();
  lastPreparedLine = line;

  try {
    engine.prepare(doc.getText(), line, docDir(doc));
  } catch (err) {
    // prepare は先行処理。失敗の詳細は通常の compile 要求でも確認できる。
    console.warn("[Realtime TeX] prepare:", err);
  }
}

// ─────────────────────────────────────────────────────────────
// 起動・エディタのイベント
// ─────────────────────────────────────────────────────────────

export function activate(context: vscode.ExtensionContext): void {
  diagnostics = vscode.languages.createDiagnosticCollection("realtime-tex");

  const openCommand = vscode.commands.registerCommand(
    "realtimeTex.openPreview",
    () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor || !/\.tex$/i.test(editor.document.fileName)) {
        void vscode.window.showWarningMessage(
          "プレビューする .tex ファイルを開いてから実行してください。"
        );
        return;
      }

      ensureEngine(context);
      openOrRevealPanel(context, editor.document);
      selectDocument(editor.document, editor.selection.active.line + 1);
    }
  );

  const changeListener = vscode.workspace.onDidChangeTextDocument((event) => {
    const doc = event.document;
    if (!engine || !isTarget(doc) || event.contentChanges.length === 0) {
      return;
    }

    // 計測はgetText()やprepare()を呼ぶ前から始める。
    const at = nowMs();
    const version = doc.version;
    const editSession = session;
    const line = currentLine(doc, event.contentChanges[0].range.start.line + 1);

    lastEdit = { uri: doc.uri.toString(), version, at };

    clearTimeout(debounceTimer);
    clearTimeout(fullSettleTimer);

    const firstInBurst = !inTypingBurst;
    inTypingBurst = true;

    if (
      firstInBurst ||
      Math.abs(line - lastPreparedLine) > 15 ||
      at - lastPrepareTime > 2500
    ) {
      prepare(doc, line);
      post("staging");
    }

    const sendDraft = () => {
      if (
        session !== editSession ||
        !isTarget(doc) ||
        doc.version !== version
      ) {
        return;
      }
      void triggerCompile(doc, line, true);
    };

    // ★ 即時送信とタイマー送信を排他にする。
    // 1文字打って止まった場合、ここから送るドラフトは1件だけ。
    if (
      inFlightCount === 0 &&
      (firstInBurst || at - lastCompileStartTime >= LIVE_INTERVAL_MS)
    ) {
      sendDraft();
    } else {
      const changes = event.contentChanges;
      const text = changes[changes.length - 1].text;
      const boundary = /[\n}\]$。．.、,]$/.test(text);

      debounceTimer = setTimeout(
        sendDraft,
        wasErrorState || boundary ? 70 : 180
      );
    }

    fullSettleTimer = setTimeout(() => {
      if (
        session !== editSession ||
        !isTarget(doc) ||
        doc.version !== version
      ) {
        return;
      }

      inTypingBurst = false;

      // 既に全文だった場合は何もしない。
      if (partialVersion === version) {
        void triggerCompile(doc, line, false);
      }
    }, SETTLE_MS);
  });

  const activeEditorListener = vscode.window.onDidChangeActiveTextEditor(
    (editor) => {
      if (!panel || !editor || !/\.tex$/i.test(editor.document.fileName)) {
        return;
      }
      if (targetDocUri?.toString() === editor.document.uri.toString()) {
        return;
      }

      selectDocument(editor.document, editor.selection.active.line + 1);
    }
  );

  const selectionListener = vscode.window.onDidChangeTextEditorSelection(
    (event) => {
      const doc = event.textEditor.document;
      if (!engine || !isTarget(doc) || !event.selections.length) return;

      clearTimeout(syncTimer);
      const line = event.selections[0].active.line + 1;
      const syncSession = session;
      const ticket = ++selectionSeq;

      if (!inTypingBurst && Math.abs(line - lastPreparedLine) > 20) {
        prepare(doc, line);
      }

      syncTimer = setTimeout(async () => {
        try {
          const target = await engine?.syncToPdf(line);
          if (
            target &&
            ticket === selectionSeq &&
            syncSession === session &&
            isTarget(doc) &&
            lastPublishedSeq !== 0
          ) {
            post("sync", { target, seq: lastPublishedSeq });
          }
        } catch (err) {
          console.warn("[Realtime TeX] SyncTeX:", err);
        }
      }, 20);
    }
  );

  context.subscriptions.push(
    diagnostics,
    openCommand,
    changeListener,
    activeEditorListener,
    selectionListener
  );
}

export function deactivate(): void {
  resetSession();
  engine?.dispose();
  engine = null;
}

function selectDocument(doc: vscode.TextDocument, line: number): void {
  if (!panel) return;

  resetSession();
  targetDocUri = doc.uri;
  panel.title = `Preview: ${path.basename(doc.fileName)}`;

  // 別文書のPDFや計測値を新しい文書のものとして表示しない。
  post("reset");
  void triggerCompile(doc, line, false);
}

// ─────────────────────────────────────────────────────────────
// コンパイル要求と診断
// ─────────────────────────────────────────────────────────────

async function triggerCompile(
  doc: vscode.TextDocument,
  line: number,
  draft: boolean
): Promise<void> {
  const client = engine;
  if (!client || !panel || !isTarget(doc)) return;

  const version = doc.version;
  const uri = doc.uri.toString();
  const mySession = session;

  // awaitの前に記録し、別のイベント経路からの二重要求も止める。
  if (draft) {
    if (draftSentVersion === version || fullSentVersion === version) return;
    draftSentVersion = version;
  } else {
    if (fullSentVersion === version) return;
    fullSentVersion = version;
  }

  const seq = ++nextCompileSeq;
  latestRequestSeq = seq;

  const editAt =
    lastEdit?.uri === uri && lastEdit.version === version
      ? lastEdit.at
      : undefined;

  const source = doc.getText();
  const sentAt = nowMs();
  lastCompileStartTime = sentAt;
  inFlightCount++;

  post("compiling");

  try {
    const res: CompileResult = await client.compile(
      source,
      line,
      draft,
      docDir(doc)
    );
    const receivedAt = nowMs();

    if (session !== mySession || !panel || !isTarget(doc)) {
  return;
}

const isCurrentVersion = doc.version === version;

if (res.status === "cancelled") {
  if (res.mode === "draft_deferred" && isCurrentVersion) {
    // この版にはまだ新しいPDFがないため、全文同期が必要。
    // 既存の700msタイマーは partialVersion === version を見ている。
    partialVersion = version;

    // 応答が返るより先に入力停止タイマーが発火していた場合。
    if (!inTypingBurst) {
      void triggerCompile(doc, line, false);
    }
  }

  // 古い版の見送りや、キュー内で置き換えられた通常のcancelledは無視。
  // エラー表示は出さず、直前の正常なPDFを維持する。
  return;
}

    if (res.status !== "updated" || !res.pdf_path) {
      // 古い入力の一時的なエラーで現在の診断を上書きしない。
      if (isCurrentVersion && seq === latestRequestSeq) {
        wasErrorState = true;
        publishDiagnostics(doc, res);
        post("error", { result: res });
      }
      return;
    }

    // 成功した途中版は表示するが、表示済みの新しい版には巻き戻さない。
    if (seq <= lastPublishedSeq) return;
    lastPublishedSeq = seq;

    const isPartial = res.mode === "draft_partial";

    if (isCurrentVersion) {
      wasErrorState = false;
      diagnostics.delete(doc.uri);
      partialVersion = isPartial ? version : -1;
    }

    paintTraces.set(seq, {
      session: mySession,
      version,
      editAt,
      sentAt,
      receivedAt,
      engineMs: res.elapsed_ms,
    });

    while (paintTraces.size > 32) {
      const first = paintTraces.keys().next();
      if (first.done) break;
      paintTraces.delete(first.value);
    }

    const pdfUri = panel.webview
      .asWebviewUri(vscode.Uri.file(res.pdf_path))
      .toString();

    post("render", {
      seq,
      version,
      result: res,
      pdfUri,
      isPartial,
      intermediate: !isCurrentVersion,
    });

    // ドラフト完成が700msタイマーより遅くても全文同期を忘れない。
    if (isCurrentVersion && isPartial && !inTypingBurst) {
      void triggerCompile(doc, line, false);
    }
  } catch (err) {
    if (session === mySession && seq === latestRequestSeq) {
      post("engineError", { message: String(err) });
      void vscode.window.showErrorMessage(
        `Realtime TeX Engine Error: ${String(err)}`
      );
    }
  } finally {
    if (session === mySession) {
      inFlightCount = Math.max(0, inFlightCount - 1);
    }
  }
}

function publishDiagnostics(
  doc: vscode.TextDocument,
  res: CompileResult
): void {
  const list: vscode.Diagnostic[] = [];

  function lineRange(startLine: number, endLine: number): vscode.Range {
    const start = Math.min(doc.lineCount - 1, Math.max(0, startLine - 1));
    const end = Math.min(doc.lineCount - 1, Math.max(start, endLine - 1));
    return new vscode.Range(
      doc.lineAt(start).range.start,
      doc.lineAt(end).range.end
    );
  }

  if (res.error_blocks.length) {
    for (const block of res.error_blocks) {
      list.push(
        new vscode.Diagnostic(
          lineRange(block.start_line, block.end_line),
          res.error_detail ?? "LaTeX syntax error",
          vscode.DiagnosticSeverity.Error
        )
      );
    }
  } else if (res.error_detail) {
    // LuaLaTeXの l.N は分割後のファイルの行番号の場合がある。
    // 元文書への対応が確定しない場合、誤った本文行を指さず先頭に通知する。
    list.push(
      new vscode.Diagnostic(
        new vscode.Range(0, 0, 0, 0),
        res.error_detail,
        vscode.DiagnosticSeverity.Error
      )
    );
  }

  diagnostics.set(doc.uri, list);
}

// ─────────────────────────────────────────────────────────────
// Webviewの作成と、表示完了の計測
// ─────────────────────────────────────────────────────────────

function openOrRevealPanel(
  context: vscode.ExtensionContext,
  doc: vscode.TextDocument
): void {
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

  // engine/src/compiler.rs の preview_dir と一致させる。
  const previewOutDir = vscode.Uri.file(
    path.join(os.tmpdir(), "realtime_tex_vscode_engine", "preview_out")
  );

  const p = vscode.window.createWebviewPanel(
    "realtimeTexPreview",
    `Preview: ${path.basename(doc.fileName)}`,
    { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
    {
      enableScripts: true,
      retainContextWhenHidden: true,
      localResourceRoots: [pdfjsBuildDir, previewOutDir],
    }
  );

  panel = p;
  webviewReady = false;

  p.webview.onDidReceiveMessage((msg) => {
    if (!msg || panel !== p) return;

    if (msg.type === "ready") {
      webviewReady = true;
      const messages = pendingPanelMessages;
      pendingPanelMessages = [];
      for (const m of messages) {
        if (m.session === session) void p.webview.postMessage(m);
      }
      return;
    }

    if (
      msg.type !== "visiblePainted" ||
      typeof msg.seq !== "number" ||
      msg.session !== session
    ) {
      return;
    }

    const trace = paintTraces.get(msg.seq);
    paintTraces.delete(msg.seq);
    if (!trace || trace.session !== session) return;

    // すべて拡張機能ホストの同一時計で引き算する。
    // WebviewとNode.jsのperformance.now()を直接比較しない。
    const paintedAt = nowMs();

    post("latency", {
      seq: msg.seq,
      version: trace.version,
      totalMs:
        trace.editAt === undefined
          ? null
          : Math.round(paintedAt - trace.editAt),
      beforeSendMs:
        trace.editAt === undefined
          ? null
          : Math.round(trace.sentAt - trace.editAt),
      roundTripMs: Math.round(trace.receivedAt - trace.sentAt),
      engineMs: trace.engineMs,
      afterResponseMs: Math.round(paintedAt - trace.receivedAt),
    });
  });

  p.onDidDispose(() => {
    if (panel !== p) return;
    resetSession();
    panel = null;
    targetDocUri = null;
    webviewReady = false;
  });

  const pdfjsUri = p.webview.asWebviewUri(
    vscode.Uri.joinPath(pdfjsBuildDir, "pdf.min.mjs")
  );
  const pdfWorkerUri = p.webview.asWebviewUri(
    vscode.Uri.joinPath(pdfjsBuildDir, "pdf.worker.min.mjs")
  );

  p.webview.html = getWebviewHtml(p.webview, pdfjsUri, pdfWorkerUri);
}

// ─────────────────────────────────────────────────────────────
// WebviewのHTML・PDF.js描画
// ─────────────────────────────────────────────────────────────

function getWebviewHtml(
  webview: vscode.Webview,
  pdfjsUri: vscode.Uri,
  pdfWorkerUri: vscode.Uri
): string {
  const nonce = randomBytes(16).toString("hex");
  const csp = [
    "default-src 'none'",
    `script-src 'nonce-${nonce}' ${webview.cspSource}`,
    "style-src 'unsafe-inline'",
    `worker-src ${webview.cspSource} blob:`,
    `connect-src ${webview.cspSource} blob:`,
    `img-src ${webview.cspSource} data: blob:`,
    `font-src ${webview.cspSource} data: blob:`,
  ].join("; ");

  // String.raw を使うため、内部JSの "\n" はそのまま書ける。
  return String.raw`<!doctype html>
<html lang="ja">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="Content-Security-Policy" content="${csp}">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <style>
    * { box-sizing: border-box; }
    body {
      height: 100vh; margin: 0; overflow: hidden;
      display: flex; flex-direction: column;
      background: #525659; color: #ddd;
      font: 12px system-ui, sans-serif;
    }
    .topbar {
      display: flex; flex-wrap: wrap; align-items: center;
      gap: 6px 14px; padding: 6px 12px;
      background: #252526; border-bottom: 1px solid #404040;
    }
    .badge {
      padding: 2px 8px; border-radius: 4px;
      color: white; background: #555; font-weight: 600;
    }
    .badge[data-state="ok"] { background: #2e7d32; }
    .badge[data-state="staging"] { background: #036b99; }
    .badge[data-state="compiling"] { background: #965900; }
    .badge[data-state="error"] { background: #b32626; }
    .metrics {
      display: flex; flex-wrap: wrap; gap: 4px 12px;
      font-family: Consolas, monospace; color: #bbb;
    }
    .metrics strong { color: white; }
    #latency-detail { cursor: help; }
    #preview {
      flex: 1; min-height: 0; overflow: auto;
      padding: 16px; position: relative;
    }
    .pdf-page-wrap {
      position: relative; width: fit-content; max-width: 100%;
      margin: 0 auto 14px;
    }
    canvas {
      display: block; background: white; max-width: 100%; height: auto;
      box-shadow: 0 3px 10px rgba(0,0,0,.35);
    }
    .sync-marker {
      position: absolute; left: 0; right: 0; height: 20px;
      background: rgba(59,130,246,.3); border-left: 4px solid #398aff;
      opacity: 0; pointer-events: none;
    }
    .sync-marker.flash { animation: flash .85s ease-out; }
    @keyframes flash { from { opacity: 1; } to { opacity: 0; } }
    #error-bar {
      display: none; padding: 6px 12px;
      background: #401919; color: #ffcece;
      white-space: pre-wrap; max-height: 7em; overflow: auto;
    }
    #error-bar.visible { display: block; }
  </style>
</head>
<body>
  <header class="topbar">
    <span id="status" class="badge">Ready</span>
    <div class="metrics">
      <span>Mode: <strong id="m-mode">–</strong></span>
      <span>Hit: <strong id="m-hits">–</strong></span>
      <span>Miss: <strong id="m-misses">–</strong></span>
      <span>Engine: <strong id="m-engine">–</strong></span>
      <span id="latency-detail" title="可視ページの描画完了までを計測">
        入力→表示: <strong id="m-total">–</strong>
      </span>
    </div>
  </header>
  <main id="preview"></main>
  <footer id="error-bar"></footer>

  <script type="module" nonce="${nonce}">
    import * as pdfjsLib from ${JSON.stringify(pdfjsUri.toString())};

    pdfjsLib.GlobalWorkerOptions.workerSrc = ${JSON.stringify(pdfWorkerUri.toString())};

    const api = acquireVsCodeApi();
    const PDF_SCALE = 1.4;
    const byId = (id) => document.getElementById(id);
    const statusEl = byId("status");
    const preview = byId("preview");
    const errorBar = byId("error-bar");
    const modeEl = byId("m-mode");
    const hitsEl = byId("m-hits");
    const missesEl = byId("m-misses");
    const engineEl = byId("m-engine");
    const totalEl = byId("m-total");
    const detailEl = byId("latency-detail");

    let activeSession = -1;
    let lastRenderSeq = -1;
    let generation = 0;
    let uiRevision = 0;
    let fetchController = null;

    function status(text, state) {
      statusEl.textContent = text;
      statusEl.dataset.state = state;
    }

    function showError(message) {
      errorBar.textContent = message;
      errorBar.classList.add("visible");
    }

    function resetView(newSession) {
      activeSession = newSession;
      lastRenderSeq = -1;
      generation++;
      uiRevision++;
      if (fetchController) fetchController.abort();
      fetchController = null;

      preview.replaceChildren();
      modeEl.textContent = hitsEl.textContent = missesEl.textContent = "–";
      engineEl.textContent = totalEl.textContent = "–";
      detailEl.title = "可視ページの描画完了までを計測";
      errorBar.classList.remove("visible");
      status("Ready", "ready");
    }

    function ensurePageSlots(count, partial, initialViewport) {
      while (preview.children.length < count) {
        const wrap = document.createElement("div");
        wrap.className = "pdf-page-wrap";
        wrap.dataset.page = String(preview.children.length + 1);

        // 初回の可視ページ判定に300×150の空Canvasを使わない。
        // 実ページが描けるまでは1ページ目の寸法を仮の外形にする。
        const canvas = document.createElement("canvas");
        canvas.style.width = Math.ceil(initialViewport.width) + "px";
        canvas.style.aspectRatio =
          initialViewport.width + " / " + initialViewport.height;

        wrap.appendChild(canvas);
        preview.appendChild(wrap);
      }

      // 部分ドラフトの場合は旧PDFの後続ページを残す。
      if (!partial) {
        while (preview.children.length > count) {
          preview.lastElementChild.remove();
        }
      }
    }

    function visiblePages(count) {
      const bounds = preview.getBoundingClientRect();
      const visible = [];
      const others = [];

      for (let p = 1; p <= count; p++) {
        const rect = preview.children[p - 1].getBoundingClientRect();
        if (rect.bottom > bounds.top && rect.top < bounds.bottom) {
          visible.push(p);
        } else {
          others.push(p);
        }
      }
      return { visible, others };
    }

    async function renderPage(pdf, pageNo, gen) {
      if (gen !== generation) return;
      const page = await pdf.getPage(pageNo);
      if (gen !== generation) return;

      const viewport = page.getViewport({ scale: PDF_SCALE });
      const back = document.createElement("canvas");
      back.width = Math.ceil(viewport.width);
      back.height = Math.ceil(viewport.height);

      const ctx = back.getContext("2d");
      if (!ctx) throw new Error("描画用Canvasを初期化できません。");

      await page.render({ canvasContext: ctx, viewport }).promise;
      if (gen !== generation) return;

      const wrap = preview.children[pageNo - 1];
      if (!wrap) return;

      // 完成済みCanvasと置換。レンダリング中に白紙にしない。
      const old = wrap.querySelector("canvas");
      if (old) old.replaceWith(back);
      else wrap.prepend(back);

      wrap.dataset.bpHeight = String(viewport.height / PDF_SCALE);
    }

    async function renderPdf(msg, gen, revision) {
      if (fetchController) fetchController.abort();
      const controller = new AbortController();
      fetchController = controller;

      let task;
      try {
        const response = await fetch(msg.pdfUri, { signal: controller.signal });
        if (!response.ok) {
          throw new Error("PDF取得失敗: HTTP " + response.status);
        }

        const bytes = await response.arrayBuffer();
        if (gen !== generation) return;

        task = pdfjsLib.getDocument({ data: new Uint8Array(bytes) });
        const pdf = await task.promise;
        if (gen !== generation) return;

        const first = await pdf.getPage(1);
        if (gen !== generation) return;

        ensurePageSlots(
          pdf.numPages,
          msg.isPartial,
          first.getViewport({ scale: PDF_SCALE })
        );

        const order = visiblePages(pdf.numPages);

        // 可視ページのみを先に描画する。
        await Promise.all(order.visible.map((p) => renderPage(pdf, p, gen)));
        if (gen !== generation) return;

        if (order.visible.length > 0) {
          // 少なくとも1回、ブラウザーに画面更新の機会を渡す。
          // モニターの物理表示時刻ではなく、体感に近い計測点。
          await new Promise((done) =>
            requestAnimationFrame(() => requestAnimationFrame(done))
          );
          if (gen !== generation) return;

          api.postMessage({
            type: "visiblePainted",
            session: msg.session,
            seq: msg.seq
          });
        } else {
          detailEl.title =
            "現在見えている位置は部分ドラフトの範囲外です。全文同期を待っています。";
        }

        if (revision === uiRevision) {
          if (msg.isPartial) status("Live Draft", "ok");
          else if (msg.intermediate) status("途中版", "ok");
          else status("Synced", "ok");
        }

        // この時間は「入力→表示」に含めない。
        for (const pageNo of order.others) {
          if (gen !== generation) return;
          await new Promise((done) => setTimeout(done, 0));
          await renderPage(pdf, pageNo, gen);
        }
      } finally {
        if (fetchController === controller) fetchController = null;
        if (task) await task.destroy();
      }
    }

    function scrollToTarget(target) {
      if (!target || !Number.isInteger(target.page)) return;
      const wrap = preview.children[target.page - 1];
      if (!wrap) return;

      const canvas = wrap.querySelector("canvas");
      const bpHeight = Number(wrap.dataset.bpHeight);
      if (!canvas || !bpHeight) return;

      // CSS縮小後の実寸を使用。狭い右ペインでもマーカーがずれにくい。
      const rect = canvas.getBoundingClientRect();
      const pane = preview.getBoundingClientRect();
      const y = target.y_pt * rect.height / bpHeight;
      const desired = Math.max(
        0,
        preview.scrollTop + rect.top - pane.top + y - preview.clientHeight * 0.35
      );

      preview.scrollTo({
        top: desired,
        behavior: Math.abs(preview.scrollTop - desired) < 240 ? "auto" : "smooth"
      });

      let marker = wrap.querySelector(".sync-marker");
      if (!marker) {
        marker = document.createElement("div");
        marker.className = "sync-marker";
        wrap.appendChild(marker);
      }

      marker.style.top = Math.max(0, y - 10) + "px";
      marker.classList.remove("flash");
      void marker.offsetWidth;
      marker.classList.add("flash");
    }

    window.addEventListener("message", async (event) => {
      const msg = event.data;
      if (!msg || typeof msg.session !== "number") return;
      if (msg.session < activeSession) return;

      if (msg.type === "reset" || msg.session > activeSession) {
        resetView(msg.session);
      }
      if (msg.type === "reset") return;

      if (msg.type === "latency") {
        if (msg.seq !== lastRenderSeq) return;

        totalEl.textContent =
          msg.totalMs == null ? "–" : msg.totalMs + " ms";

        detailEl.title = [
          "文書の版: " + msg.version,
          "入力 → 要求送信: " +
            (msg.beforeSendMs == null ? "–" : msg.beforeSendMs + " ms"),
          "要求送信 → Rust応答: " + msg.roundTripMs + " ms",
          "  うち Engine: " + msg.engineMs + " ms",
          "Rust応答 → 可視ページ表示通知: " + msg.afterResponseMs + " ms"
        ].join("\n");
        return;
      }

      if (msg.type === "sync") {
        if (msg.seq === lastRenderSeq) scrollToTarget(msg.target);
        return;
      }

      if (msg.type === "staging" || msg.type === "compiling") {
        uiRevision++;
        if (msg.type === "staging") status("Pre-compiling...", "staging");
        else status("Compiling...", "compiling");
        return;
      }

      if (msg.type === "error" || msg.type === "engineError") {
        uiRevision++;
        status("Error (Holding Preview)", "error");
        showError(
          msg.message ||
          (msg.result && msg.result.error_detail) ||
          "LuaLaTeX compile error"
        );
        return;
      }

      if (msg.type !== "render" || msg.seq <= lastRenderSeq) return;

      lastRenderSeq = msg.seq;
      const gen = ++generation;
      const revision = ++uiRevision;
      const r = msg.result;

      modeEl.textContent = r.mode;
      hitsEl.textContent = String(r.cache_hits);
      missesEl.textContent = String(r.cache_misses);
      engineEl.textContent = r.elapsed_ms + " ms";
      totalEl.textContent = "–";
      detailEl.title = "この版の可視ページを描画中";
      errorBar.classList.remove("visible");
      status("Rendering...", "compiling");

      try {
        await renderPdf(msg, gen, revision);
      } catch (err) {
        if (gen !== generation || revision !== uiRevision) return;
        status("PDF Render Error", "error");
        showError(String(err));
      }
    });

    // メッセージ受信準備ができてから初回PDFを受け取る。
    api.postMessage({ type: "ready" });
  </script>
</body>
</html>`;
}
