"use strict";
var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || ("get" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    o[k2] = m[k];
}));
var __setModuleDefault = (this && this.__setModuleDefault) || (Object.create ? (function(o, v) {
    Object.defineProperty(o, "default", { enumerable: true, value: v });
}) : function(o, v) {
    o["default"] = v;
});
var __importStar = (this && this.__importStar) || (function () {
    var ownKeys = function(o) {
        ownKeys = Object.getOwnPropertyNames || function (o) {
            var ar = [];
            for (var k in o) if (Object.prototype.hasOwnProperty.call(o, k)) ar[ar.length] = k;
            return ar;
        };
        return ownKeys(o);
    };
    return function (mod) {
        if (mod && mod.__esModule) return mod;
        var result = {};
        if (mod != null) for (var k = ownKeys(mod), i = 0; i < k.length; i++) if (k[i] !== "default") __createBinding(result, mod, k[i]);
        __setModuleDefault(result, mod);
        return result;
    };
})();
Object.defineProperty(exports, "__esModule", { value: true });
exports.EngineClient = void 0;
const child_process_1 = require("child_process");
const fs = __importStar(require("fs"));
const path = __importStar(require("path"));
const readline = __importStar(require("readline"));
class EngineClient {
    binaryPath;
    proc = null;
    nextId = 1;
    pending = new Map();
    constructor(binaryPath) {
        this.binaryPath = binaryPath;
    }
    static resolveBinaryPath(extensionPath, customPath) {
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
    start() {
        if (this.proc)
            return;
        if (!fs.existsSync(this.binaryPath)) {
            throw new Error(`Rust エンジンが見つかりません: ${this.binaryPath}\nengine フォルダで "cargo build --release" を実行してください。`);
        }
        this.proc = (0, child_process_1.spawn)(this.binaryPath, [], { stdio: ["pipe", "pipe", "pipe"] });
        const rl = readline.createInterface({ input: this.proc.stdout });
        rl.on("line", (line) => {
            try {
                const msg = JSON.parse(line);
                const resolver = this.pending.get(msg.id);
                if (resolver) {
                    this.pending.delete(msg.id);
                    resolver(msg);
                }
            }
            catch {
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
    async compile(source, documentDir) {
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
            return resp;
        }
        return {
            status: "compile_error",
            pdf_path: null,
            error_blocks: [],
            cache_hits: 0,
            cache_misses: 0,
            resolved_labels_count: 0,
            elapsed_ms: 0,
            mode: "error",
            error_detail: resp.message ?? "Unknown engine error",
        };
    }
    async syncToPdf(line) {
        if (!this.proc)
            return null;
        const id = this.nextId++;
        const resp = await this.send(id, { type: "sync", id, line });
        return resp.type === "sync_result" ? resp.target ?? null : null;
    }
    dispose() {
        if (!this.proc)
            return;
        try {
            const id = this.nextId++;
            this.proc.stdin.write(JSON.stringify({ type: "shutdown", id }) + "\n");
        }
        catch {
            // ignore
        }
        this.proc.kill();
        this.proc = null;
    }
    send(id, payload) {
        return new Promise((resolve) => {
            this.pending.set(id, resolve);
            this.proc.stdin.write(JSON.stringify(payload) + "\n");
        });
    }
}
exports.EngineClient = EngineClient;
//# sourceMappingURL=engineClient.js.map