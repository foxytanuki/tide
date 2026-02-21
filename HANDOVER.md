# Handover: tide rename instability (`new:new` -> `dev`) investigation

## Status: ROOT CAUSE IDENTIFIED (2026-02-21)

### 原因

`~/.zshrc` の `precmd` hook が毎プロンプト表示時に `tmux rename-window "$(basename $(pwd))"` を実行していた。

```zsh
# ~/.zshrc:106-110 (削除済み)
_tmux_auto_name() {
    local w=$(tmux display-message -p '#W' 2>/dev/null)
    [[ "$w" != ✓* && "$w" != ✗* ]] && tmux rename-window "$(_tmux_proj)" 2>/dev/null
}
add-zsh-hook precmd _tmux_auto_name
```

- `_tmux_proj()` は `basename "$(pwd)"` を返す → cwd が `~/dev/...` なら `dev` になる
- `allow-rename off` はエスケープシーケンス経由のみブロックし、`tmux rename-window` コマンドは素通り
- tide がいくら `new:new` にリネームしても、次のプロンプト描画で `dev` に上書きされていた

### 対応

- `~/.zshrc` から `_tmux_auto_name` / `run()` ブロック全体を削除済み
- `README.md` に「shell の precmd/preexec で rename-window するな」という注意書きを追加済み

## 残存コード課題（原因とは独立して改善の余地あり）

### 1. `pending_rename` が単一スロット

- `model.rs` の `pending_rename_window_id: Option<String>` は 1 件のみ追跡
- `c` 連打時に最後の 1 つだけが追跡対象になる
- 外部 rename 源が消えた今、実害は大幅に減ったが、将来的に `HashMap` 化すると堅牢になる

### 2. `WindowRenamed` イベントの情報を捨てている

- `main.rs` で `TmuxEvent::WindowRenamed(id, name)` を `Msg::WindowChanged` に潰している
- id/name を活用すれば `ListWindows` 往復なしで即時補正できる
- 外部 rename 源がない限り発火しないので優先度は下がった

## 実施した変更の記録

### execute.rs

- `Cmd::NewWindow`: `new-window -d -n <name> -P -F '#{window_id}'` で window_id 取得、`automatic-rename off` / `allow-rename off` 設定、pending 登録
- `Cmd::RenameWindow`: rename 前後に `automatic-rename off` / `allow-rename off` を設定

### update.rs

- `WindowListLoaded` で pending の保持/破棄ロジックを整理（警告解消）

### .zshrc

- `_tmux_auto_name` / `run()` ブロック削除

### README.md

- 新規作成。shell の `tmux rename-window` hook との非互換を明記
