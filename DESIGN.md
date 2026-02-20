# tmuxide - Design Document

zide の tmux 移植版。各 tmux window の左 pane にサイドバーとして常駐する TUI アプリ。

## Why

- zellij のバグが多い（パーミッションダイアログ、キー入力の不具合、IME 問題）
- zellij のユーザー層がニッチ → tmux の方が圧倒的に広い
- zellij WASM プラグインの制約（描画API が貧弱、ratatui 使えない）
- tmux には既存のウィンドウ一覧常駐 UI がない（sesh 等はポップアップ型）

## Architecture

### 全体像

```
┌──────────┬──────────────────────────────┐
│ tmuxide  │                              │
│ sidebar  │    user's working pane       │
│ (25 col) │                              │
│          │                              │
│  proj-a  │                              │
│  ├ edit  │                              │
│  └ term  │                              │
│  proj-b  │                              │
│  └ main  │                              │
└──────────┴──────────────────────────────┘
```

各 window の左 pane に tmuxide インスタンスが常駐。
サイドバーは `tmux split-window -hb -l 25` で作成。

### TEA (The Elm Architecture)

zide と同じアーキテクチャを採用。

```
tmux event (control mode) ─┐
                            ├→ Msg → update(model, msg) → (Model, Vec<Cmd>)
crossterm key event ────────┘                                     │
                                                          execute_commands()
                                                                  │
                                                     ┌────────────┤
                                                     ▼            ▼
                                              render(model)   tmux CLI
```

- `update()` は純粋関数
- 副作用は `Cmd` 値で表現 → `execute_commands()` でまとめて実行

### tmux 通信: control mode

**control mode (`tmux -CC`) を採用。** リアルタイムにイベントを受信できる。

```
┌─────────────┐     stdin      ┌─────────┐
│  tmuxide     │ ──────────→   │  tmux   │
│  (TUI app)   │ ←────────── │ -CC     │
│              │    stdout     │ attach  │
└─────────────┘   (events)    └─────────┘
```

- `tmux -CC attach -t <session>` を子プロセスとして起動
- stdin にコマンド送信（`select-window -t :N` 等）
- stdout からイベント受信（`%window-add`, `%window-close`, `%window-renamed` 等）
- 非同期 I/O (tokio) でイベントループと統合

#### control mode イベント（主要）

| イベント | 発火タイミング |
|----------|----------------|
| `%window-add @id` | ウィンドウ追加 |
| `%window-close @id` | ウィンドウ削除 |
| `%window-renamed @id name` | ウィンドウリネーム |
| `%session-changed $id name` | セッション切り替え |
| `%begin ... %end` | コマンド応答の開始/終了 |

#### IPC 層の抽象化

将来的な切り替え余地を残すため、tmux 通信は trait で抽象化する。

```rust
trait TmuxConnection {
    async fn send_command(&mut self, cmd: &str) -> Result<String>;
    fn events(&self) -> impl Stream<Item = TmuxEvent>;
}
```

### イベントループ

```rust
loop {
    tokio::select! {
        // crossterm のキー/マウスイベント
        event = crossterm_events.next() => { ... }
        // control mode からの tmux イベント
        event = tmux_control.next_event() => {
            match event {
                TmuxEvent::WindowAdd(id) => { /* Msg::WindowAdded */ }
                TmuxEvent::WindowClose(id) => { /* Msg::WindowClosed */ }
                TmuxEvent::WindowRenamed(id, name) => { /* Msg::WindowRenamed */ }
                // ...
            }
        }
    }
}
```

## tmux CLI マッピング

### zide Cmd → tmux コマンド

| zide Cmd | tmux コマンド | 備考 |
|----------|---------------|------|
| `GoToTab { tab_name }` | `select-window -t '{name}'` | window 名で切り替え |
| `FocusNextPane` | `select-pane -t +1` | 隣の pane にフォーカス |
| `NewTabWithLayout` | `new-window \; split-window -hb -l 25 'tmuxide'` | 下記参照 |
| `RenameTab` | `rename-window -t :N 'name'` | |
| `CloseTab` | `kill-window -t :N` | |
| `CloseFocusedPane` | `kill-pane` | |

### 削除される zide 機能

| 機能 | 理由 |
|------|------|
| Permission 管理 | tmux に権限モデルなし |
| `tabs_snapshot` | control mode でリアルタイム取得 |
| `RenameTerminalPane` | tmux に pane 名の概念なし |
| `InterceptInput` / `ClearIntercepts` | pane フォーカスで自然に制御 |
| KDL レイアウト生成 | tmux はシェルコマンドで制御 |

### 新規 window 作成フロー

```bash
# 基本
tmux new-window -n 'window-name'
tmux split-window -hb -l 25 -t '{window-name}' 'tmuxide'

# フォルダ内に作成する場合
tmux new-window -n 'proj:tab3'
tmux split-window -hb -l 25 -t '{proj:tab3}' 'tmuxide'
```

## Core Features

### P0 (MVP)

1. **ツリー表示**
   - 現在のセッション内のウィンドウをツリーで一覧表示
   - `:` 区切りによるフォルダグルーピング（zide 互換）
   - フォルダの展開/折りたたみ

2. **プレビュー**
   - ツリー上でカーソルを移動すると、対応するウィンドウが右ペインに表示される
   - `select-window` でウィンドウを切り替えることでプレビューを実現
   - 実質的に「選択 = プレビュー」（zide と同じ体験）

3. **基本操作**
   - `j`/`k`/↑/↓: カーソル移動（+ プレビュー切り替え）
   - `Enter`/`l`/→: フォルダならトグル、ウィンドウなら選択してフォーカスを外す
   - `h`/←: フォルダ折りたたみ / 親フォルダに移動
   - `Esc`: サイドバーからフォーカスを右ペインに返す
   - `r`: ウィンドウリネーム
   - `x`: ウィンドウクローズ（確認あり）
   - `c`: 新規ウィンドウ作成

4. **リアルタイム同期**
   - control mode (`tmux -CC attach`) でイベント購読
   - `%window-add`, `%window-close`, `%window-renamed` 等の通知でツリーを自動更新
   - 外部からのウィンドウ操作も即座に反映

### P1

5. **マルチセッション対応**
   - 他セッションのウィンドウもツリーに表示
   - セッション間の切り替え (`switch-client`)

6. **フォルダ付き新規ウィンドウ作成**
   - `C` / `Alt+c`: プロジェクト名入力 → `name:window1` として作成
   - カーソルがフォルダ上なら同フォルダに追加

7. **レイアウト永続化**
   - 起動コマンド一発でサイドバー付きレイアウトを構築
   - `tmuxide attach` 的な CLI インターフェース

### P2

8. **ウィンドウ並び替え**
   - `J`/`K` でウィンドウの順序変更 (`swap-window`)

9. **検索 / フィルタ**
   - `/` でウィンドウ名のインクリメンタルサーチ

10. **カスタムグルーピングルール**
    - `:` 以外の区切り文字対応
    - 設定ファイルでのルール定義

## フォーカス制御

| 操作 | zellij (現状) | tmuxide |
|------|---------------|---------​|
| サイドバーにフォーカス | Ctrl+F でプラグイン pane を選択 | `select-pane -L`（左 pane へ） |
| サイドバーからフォーカスを返す | Esc → `FocusNextPane` | Esc → `select-pane -R`（右 pane へ） |
| キー横取り | `InterceptInput` | 不要（pane フォーカスで自然に制御） |

tmux のキーバインドで `prefix + h` → 左 pane（tmuxide）にフォーカス、を推奨。

## コード構成

```
tmuxide/
├── Cargo.toml
├── src/
│   ├── main.rs          # tokio ランタイム、イベントループ
│   ├── tmux/
│   │   ├── mod.rs       # tmux 通信の公開API
│   │   ├── control.rs   # control mode プロトコル (stdin/stdout)
│   │   └── parser.rs    # control mode レスポンス/イベントパーサー
│   ├── msg.rs           # Msg enum
│   ├── cmd.rs           # Cmd enum（tmux コマンドに対応）
│   ├── model.rs         # Model + 純粋ロジック
│   ├── tree.rs          # TreeNode, FlatItem, グルーピングロジック
│   ├── update.rs        # update() 純粋関数
│   └── view.rs          # ratatui 描画（header + tree + footer）
```

### zide からの流用率

| ファイル | 流用率 | 作業内容 |
|----------|--------|----------|
| `tree.rs` | **100%** | コピーのみ。zellij 依存ゼロ |
| `model.rs` | **~80%** | `SessionInfo`/`TabInfo` を自前型に。ロジックはそのまま |
| `update.rs` | **~60%** | TEA パターン維持。Cmd 差し替え、Permission 系削除 |
| `cmd.rs` | **0%** | 全書き直し（軽量） |
| `msg.rs` | **~70%** | Permission 系削除、WindowAdded/WindowClosed/WindowRenamed 追加 |
| `view.rs` | **0%** | ratatui で全書き直し（構造は同じ: header + tree + footer） |
| `main.rs` | **0%** | 全書き直し（WASM → tokio イベントループ） |
| `tmux/` | **新規** | control mode パーサー + コマンド送信 |

## 自前型

```rust
pub struct WindowInfo {
    pub id: String,       // @id (tmux window_id)
    pub index: usize,
    pub name: String,
    pub active: bool,
}
```

tmux は「セッション > ウィンドウ > ペイン」の階層だが、
P0 では同一セッション内の window だけを扱う。
P1 でマルチセッション対応時に `SessionInfo` を追加。

## Dependencies

```toml
[dependencies]
ratatui = "0.29"
crossterm = "0.28"
tokio = { version = "1", features = ["full"] }
tui-tree-widget = "0.22"
unicode-width = "0.2"
```

## リスク

### 低リスク
- **コア流用**: tree.rs / model.rs のロジックは実証済み
- **tmux CLI の安定性**: 枯れてる
- **描画**: ratatui は成熟したライブラリ

### 中リスク
- **control mode のパース複雑さ**: `%begin`/`%end` ブロックの処理、エラーハンドリング
  - 対策: パーサーを独立モジュールにしてユニットテスト充実させる
- **複数インスタンスの一貫性**: 各 window の tmuxide が独立して control mode 接続
  - 対策: 各インスタンスは自身のイベントストリームで状態更新（競合しない）
- **control mode 接続断**: tmux サーバーが落ちた場合のリカバリ
  - 対策: 再接続ロジック + エラー表示

### 高リスク
- **tmux のエッジケース**: 実装してみないとわからない
  - 例: window 名に特殊文字があると control mode パースが壊れる？
  - 例: 1 セッションに対して複数 control client は問題ない？
  - 対策: 早期に prototype を作って検証

## Open Questions

- control mode の接続が切れた場合のリカバリ戦略
- サイドバー pane 自身が `list-windows` に出るのをどう扱うか
- tmux の最低サポートバージョン（control mode の機能差）
- 1 セッション内の複数 control mode 接続の挙動

## Non-Goals

- ファイルツリー表示（エディタの仕事）
- ターミナルマルチプレクサの再実装（tmux に乗る）
- zellij サポート（tmux 専用）
- GUI / Web UI
