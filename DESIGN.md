# tide - Design Document

tmux の各 window に 25-column の sidebar を常駐させ、window 群をツリーとして扱う TUI アプリ。`name:subname` 形式の命名でフォルダ構造を表現する。

このプロジェクトは、自分の `zide` で試した操作モデルを tmux 向けに再設計したもの。

## Why

- zellij では安定性や入力まわりで気になる点があった
- tmux の方がユーザー層が広く、既存ワークフローに組み込みやすい
- tmux には常駐型の window/sidebar UI が少ない
- ratatui + Rust で、制御と描画を素直に実装できる

## Architecture

### 全体像

```
┌──────────┬──────────────────────────────┐
│ tide     │                              │
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

各 window の左 pane に tide インスタンスが常駐し、`tmux split-window -hb -l 25` で sidebar を作る。

### TEA (The Elm Architecture)

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
- 副作用は `Cmd` 値で表現し、`execute_commands()` でまとめて実行する

### tmux 通信: control mode

**control mode (`tmux -CC`) を採用。** リアルタイムにイベントを受信できる。

```
┌─────────────┐     stdin      ┌─────────┐
│    tide     │ ──────────→   │  tmux   │
│  (TUI app)  │ ←──────────   │ -CC     │
│             │    stdout      │ attach  │
└─────────────┘   (events)     └─────────┘
```

- `tmux -CC attach -t <session>` を子プロセスとして起動
- stdin にコマンド送信（`select-window -t :N` など）
- stdout からイベント受信（`%window-add`, `%window-close`, `%window-renamed` など）
- 非同期 I/O (tokio) でイベントループと統合

#### control mode イベント（主要）

| イベント | 発火タイミング |
|----------|----------------|
| `%window-add @id` | window 追加 |
| `%window-close @id` | window 削除 |
| `%window-renamed @id name` | window rename |
| `%session-changed $id name` | session 切り替え |
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
        event = crossterm_events.next() => { ... }
        event = tmux_control.next_event() => {
            match event {
                TmuxEvent::WindowAdd(id) => { /* Msg::WindowAdded */ }
                TmuxEvent::WindowClose(id) => { /* Msg::WindowClosed */ }
                TmuxEvent::WindowRenamed(id, name) => { /* Msg::WindowRenamed */ }
            }
        }
    }
}
```

## tmux CLI マッピング

| 操作 | tmux コマンド | 備考 |
|------|---------------|------|
| window へ移動 | `select-window -t '{name}'` | window 名で切り替え |
| 隣 pane へ focus | `select-pane -t +1` | 通常は右 pane を想定 |
| 新規 window + sidebar | `new-window \; split-window -hb -l 25 'tide'` | 下記参照 |
| window rename | `rename-window -t :N 'name'` | |
| window close | `kill-window -t :N` | |
| pane close | `kill-pane` | |

### 新規 window 作成フロー

```bash
# 基本
tmux new-window -n 'window-name'
tmux split-window -hb -l 25 -t '{window-name}' 'tide'

# フォルダ内に作成する場合
tmux new-window -n 'proj:tab3'
tmux split-window -hb -l 25 -t '{proj:tab3}' 'tide'
```

## Core Features

### P0 (MVP)

1. **ツリー表示**
   - 現在の session 内の window をツリーで一覧表示
   - `:` 区切りによるフォルダグルーピング
   - フォルダの展開/折りたたみ

2. **プレビュー**
   - ツリー上でカーソルを移動すると、対応する window を右 pane に表示
   - `select-window` による window 切り替えでプレビューを実現

3. **基本操作**
   - `j`/`k`/↑/↓: カーソル移動（+ プレビュー切り替え）
   - `Enter`/`l`/→: フォルダならトグル、window なら選択してフォーカスを外す
   - `h`/←: フォルダ折りたたみ / 親フォルダへ移動
   - `Esc`: サイドバーから右 pane へフォーカスを返す
   - `r`: window rename
   - `x`: window close（確認あり）
   - `c`: 新規 window 作成

4. **リアルタイム同期**
   - control mode (`tmux -CC attach`) でイベント購読
   - `%window-add`, `%window-close`, `%window-renamed` などの通知でツリーを自動更新
   - 外部からの window 操作も即座に反映

### P1

5. **マルチセッション対応**
   - 他 session の window もツリーに表示
   - session 間の切り替え (`switch-client`)

6. **フォルダ付き新規 window 作成**
   - `C` / `Alt+c`: project 名入力 → `name:window1` として作成
   - カーソルがフォルダ上なら同フォルダに追加

7. **レイアウト永続化**
   - 起動コマンド一発でサイドバー付きレイアウトを構築
   - 将来的には `tide attach` のような CLI も検討する

### P2

8. **window 並び替え**
   - `J`/`K` で window の順序変更 (`swap-window`)

9. **検索 / フィルタ**
   - `/` で window 名のインクリメンタルサーチ

10. **カスタムグルーピングルール**
    - `:` 以外の区切り文字対応
    - 設定ファイルでのルール定義

## フォーカス制御

| 操作 | zellij plugin 風 UI | tide |
|------|---------------------|------|
| サイドバーにフォーカス | plugin pane を選択 | `select-pane -L` |
| サイドバーからフォーカスを返す | pane を抜ける | `select-pane -R` |
| キー横取り | 専用制御が必要 | 不要（pane フォーカスで自然に制御） |

tmux のキーバインドで `prefix + h` から左 pane の tide にフォーカスできるようにしておくと使いやすい。

## コード構成

```
tide/
├── Cargo.toml
├── src/
│   ├── main.rs          # tokio ランタイム、イベントループ
│   ├── tmux/
│   │   ├── mod.rs       # tmux 通信の公開 API
│   │   ├── control.rs   # control mode プロトコル (stdin/stdout)
│   │   └── parser.rs    # control mode レスポンス/イベントパーサー
│   ├── msg.rs           # Msg enum
│   ├── cmd.rs           # Cmd enum（tmux コマンドに対応）
│   ├── model.rs         # Model + 純粋ロジック
│   ├── tree.rs          # TreeNode, FlatItem, グルーピングロジック
│   ├── update.rs        # update() 純粋関数
│   └── view.rs          # ratatui 描画（header + tree + footer）
```

## 自前型

```rust
pub struct WindowInfo {
    pub id: String,       // @id (tmux window_id)
    pub index: usize,
    pub name: String,
    pub active: bool,
}
```

tmux は「session > window > pane」の階層だが、P0 では同一 session 内の window だけを扱う。P1 でマルチセッション対応時に `SessionInfo` を追加する。

## Dependencies

```toml
[dependencies]
ratatui = "0.30"
crossterm = "0.29"
tokio = { version = "1", features = ["full"] }
tui-tree-widget = "0.24"
unicode-width = "0.2.2"
```

## リスク

### 低リスク
- **tmux CLI の安定性**: 基本コマンド群は枯れている
- **描画**: ratatui は成熟したライブラリ

### 中リスク
- **control mode のパース複雑さ**: `%begin`/`%end` ブロック処理、エラーハンドリング
  - 対策: パーサーを独立モジュールにしてユニットテストを厚くする
- **複数インスタンスの一貫性**: 各 window の tide が独立して control mode 接続する
  - 対策: 各インスタンスは自身のイベントストリームで状態更新し、必要時に再同期する
- **control mode 接続断**: tmux サーバーが落ちた場合の復旧
  - 対策: 再接続ロジック + エラー表示

### 高リスク
- **tmux のエッジケース**: 実装しないと見えにくい
  - 例: window 名の特殊文字で control mode パースが壊れないか
  - 例: 1 session に対する複数 control client の挙動
  - 対策: 早めに prototype を作って検証する

## Open Questions

- control mode の接続が切れた場合のリカバリ戦略
- サイドバー pane 自身をどう扱うか
- tmux の最低サポートバージョン
- 1 session 内の複数 control mode 接続の挙動

## Non-Goals

- ファイルツリー表示（エディタの仕事）
- ターミナルマルチプレクサの再実装（tmux に乗る）
- zellij サポート（tmux 専用）
- GUI / Web UI
