import { ChildProcessWithoutNullStreams, spawn } from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as readline from "readline";

export interface TexBlock {
  id: number;
  content: string;
  cache_key: string;
  has_error: boolean;
  start_line: number;
  end_line: number;
}

export interface CompileResult {
  status: "updated" | "fallback_error" | "compile_error";
  pdf_base64: string | null;
  error_blocks: TexBlock[];
  cache_hits: number;
  cache_misses: number;
  resolved_labels_count: number;
  elapsed_ms: number;
  mode: string;
  error_detail: string | null;
}

export interface PdfSyncTarget {
  page: number;
  y_pt: number;
}

export class EngineClient {
  private proc: ChildProcessWithoutNullStreams | null = null;
  private nextId = 1;
  private pending = new Map<number, (msg: any) => void>();

  constructor(private readonly binaryPath: string) {}

  static resolveBinaryPath(extensionPath: string, customPath?: string): string {
    if (customPath && fs.existsSync(customPath)) {
      return customPath;
    }
    const exeName = process.platform === "win32" ? "realtime-tex-server.exe" : "realtime-tex-server";
    const candidates = [
      path.resolve(extensionPath, "..", "engine", "target", "release", exeName),
      path.resolve(extensionPath, "..", "engine", "target", "debug", exeName),
      path.resolve(extensionPath, "bin", exeName),
    ];
    for (const c of candidates) {
      if (fs.existsSync(c)) {
        return c;
      }
    }
    return candidates[0];
  }

  start(): void {
    if (this.proc) return;
    if (!fs.existsSync(this.binaryPath)) {
      throw new Error(
        `Rust エンジンが見つかりません: ${this.binaryPath}\nengine フォルダで "cargo build --release" を実行してください。`
      );
    }

    this.proc = spawn(this.binaryPath, [], { stdio: ["pipe", "pipe", "pipe"] });
    const rl = readline.createInterface({ input: this.proc.stdout });

    rl.on("line", (line) => {
      try {
        const msg = JSON.parse(line);
        const resolver = this.pending.get(msg.id);
        if (resolver) {
          this.pending.delete(msg.id);
          resolver(msg);
        }
      } catch {
        // ignore malformed output
      }
    });

    this.proc.stderr.on("data", (chunk) => {
      console.log("[realtime-tex-server]", chunk.toString().trim());
    });

    this.proc.on("exit", () => {
      this.proc = null;
      for (const [, resolve] of this.pending) {
        resolve({ type: "error", message: "Engine process exited unexpectedly" });
      }
      this.pending.clear();
    });
  }

  async compile(source: string, documentDir?: string): Promise<CompileResult> {
    this.start();
    const id = this.nextId++;
    const payload = {
      type: "compile",
      id,
      source,
      document_dir: documentDir ?? null,
    };
    const resp = await this.send(id, payload);
    if (resp.type === "compile_result") {
      return resp as CompileResult;
    }
    return {
      status: "compile_error",
      pdf_base64: null,
      error_blocks: [],
      cache_hits: 0,
      cache_misses: 0,
      resolved_labels_count: 0,
      elapsed_ms: 0,
      mode: "error",
      error_detail: resp.message ?? "Unknown engine error",
    };
  }

  async syncToPdf(line: number): Promise<PdfSyncTarget | null> {
    if (!this.proc) return null;
    const id = this.nextId++;
    const resp = await this.send(id, { type: "sync", id, line });
    return resp.type === "sync_result" ? resp.target ?? null : null;
  }

  dispose(): void {
    if (!this.proc) return;
    try {
      const id = this.nextId++;
      this.proc.stdin.write(JSON.stringify({ type: "shutdown", id }) + "\n");
    } catch {
      // ignore
    }
    this.proc.kill();
    this.proc = null;
  }

  private send(id: number, payload: object): Promise<any> {
    return new Promise((resolve) => {
      this.pending.set(id, resolve);
      this.proc!.stdin.write(JSON.stringify(payload) + "\n");
    });
  }
}
