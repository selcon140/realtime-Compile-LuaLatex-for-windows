# Realtime TeX Preview for VS Code

VS Code 内で動作するリアルタイム LuaLaTeX プレビュー拡張機能です。
Rust 製の独立エンジン（`realtime-tex-server`）をバックグラウンドで起動し、未保存の編集中バッファをリアルタイムにインクリメンタルコンパイルして右ペインの Webview にフリッカーレス描画します。

## 主な機能
- **ローカル画像・外部ファイル対応 (`TEXINPUTS`):** 編集中の `.tex` と同じフォルダ（およびサブフォルダ）にある JPEG / PNG / PDF や `\input` ファイルを、作業フォルダを汚さずに自動解決します。
- **インクリメンタル並列キャッシュ & 自動フォールバック:** `tree-sitter-latex` によるブロック分割・ネスト対応の相互参照解決・非フロート化パッチを備え、複雑な文書は自動的に生コンパイルへフォールバックします。
- **Last-Known-Good & VS Code 診断連携:** 入力途中のエラー時は直前の正常プレビューを維持しつつ、VS Code の「問題」パネルとエディタ上の赤波線にエラーを表示します。
- **SyncTeX 同期:** エディタ上のカーソル移動に連動して PDF プレビューを該当箇所へ自動スクロール＆ハイライトします。

## セットアップ手順

### 1. Rust エンジンのビルド
```powershell
cd engine
cargo build --release
```
`engine/target/release/realtime-tex-server.exe` が生成されます。

### 2. VS Code 拡張機能のビルド
```powershell
cd ../extension
npm install
npm run compile
```

### 3. 起動（拡張機能開発ホストでテスト）
1. VS Code で **`extension` フォルダ**（またはルートフォルダ）を開きます。
   ※ ルートを開いた場合は `.vscode/launch.json` がそのまま使えます。
2. **`F5`** キーを押すと、拡張機能が読み込まれた新しい VS Code ウィンドウが開きます。
3. 任意の `.tex` ファイル（同じフォルダに画像があってもOK）を開き、エディタ右上のプレビューアイコン、または `Ctrl+Shift+P` → **`Realtime TeX: Open Preview`** を実行します。
