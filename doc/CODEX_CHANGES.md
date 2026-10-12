# Codexからの変更内容

記録日: 2026-10-13

## 比較元

OpenAIのCodexの次のコミットに保存されたコードと比較しました。

- Repository: https://github.com/openai/codex
- Commit: `068c49f075cf287a1fe7d1ee36cf005efac922e7`
- Commit date: 2026-08-24
- Commit title: Update the frameless realtime default model (#40321)
- Commit URL: https://github.com/openai/codex/commit/068c49f075cf287a1fe7d1ee36cf005efac922e7

現在の家庭用エージェントの作業中コードではなく、Gitのコミットに保存された
元のコードを比較元にしています。LFとCRLFの違いは内容変更と区別しています。

## 既存ファイルの変更

既存ファイル18件に内容変更がありました。各ファイルの冒頭にも、Tege2Dev用に
変更したこと、比較元コミット、この記録の参照先を追記しています。
以下は注記を追加する前に確認した実装上の差分です。

| ファイル | 確認した変更内容 |
| --- | --- |
| `codex-rs/Cargo.toml` | `voiceagent-server`をworkspaceメンバーに追加。 |
| `codex-rs/Cargo.lock` | `codex-voiceagent-server`パッケージとその依存関係を追加。 |
| `codex-rs/codex-api/src/common.rs` | Responses APIリクエストに任意の`max_output_tokens`と`temperature`を追加。 |
| `codex-rs/codex-api/src/endpoint/responses_websocket.rs` | WebSocket関連のテスト用リクエストに追加フィールドを設定。 |
| `codex-rs/codex-api/src/sse/responses.rs` | アイドルタイムアウトが0の場合、SSE待機時間に制限を設けない。 |
| `codex-rs/codex-api/tests/clients.rs` | APIクライアントのテスト用リクエストを追加フィールドに対応。 |
| `codex-rs/core-api/src/lib.rs` | `McpServerConfig`をCore APIから公開。 |
| `codex-rs/core/src/client.rs` | 非OpenAIプロバイダー向けにテキストのsystem/developer指示をトップレベルのinstructionsへ統合。出力上限とtemperatureを環境変数から送信し、接続再利用の比較条件にも追加。 |
| `codex-rs/core/src/client_common_tests.rs` | 共通テストのリクエストに追加フィールドを設定。 |
| `codex-rs/core/src/client_tests.rs` | 指示ロールの統合と、画像を含む指示を保持する動作のテストを追加。 |
| `codex-rs/core/src/codex_delegate.rs` | ターン開始オプションに`disable_tools: false`を設定。 |
| `codex-rs/core/src/guardian/review_session.rs` | レビュー用ターンに`disable_tools: false`を設定。 |
| `codex-rs/core/src/session/turn_input.rs` | `disable_tools`を受け取り、該当ターンでツールを無効化する状態を設定。 |
| `codex-rs/core/src/tools/router.rs` | 名前空間を使わないMCPツール名を、登録済みの正規の名前に解決。 |
| `codex-rs/core/src/tools/spec_plan.rs` | ターン単位のツール無効化、MCPツール定義の平坦化、`lmstudio`プロバイダーでの名前空間無効化を追加。 |
| `codex-rs/ext/guardian-v2/src/async_scorer/sampler.rs` | 評価用リクエストに追加フィールドを設定。 |
| `codex-rs/ext/skills/src/render.rs` | スキル情報用のコンテキスト割当割合を2%から6%へ変更。 |
| `codex-rs/protocol/src/turn_input.rs` | ターン開始オプションに`disable_tools`を追加。 |

元のファイルと配置先が異なる派生ファイルも確認しました。
`runtime/prompts/default.md`は、元の
`codex-rs/protocol/src/prompts/base_instructions/default.md`をコピーしたうえで、
`apply_patch`を使わせる指示とそのツール名への参照を削除・言い換えています。
このファイルにも変更の注記を追加しました。

## 追加したファイル

元のCodexにはなかったファイルは、初回比較で27件でした。
この変更記録自体も、今回追加したファイルです。

| ファイル・フォルダ | 内容 |
| --- | --- |
| `codex-rs/voiceagent-server/Cargo.toml` | 開発エージェントのサーバーパッケージ定義。 |
| `codex-rs/voiceagent-server/src/config.rs` | Coreの設定構成。 |
| `codex-rs/voiceagent-server/src/harness.rs` | セッション、プロジェクト、Core呼び出しの管理。 |
| `codex-rs/voiceagent-server/src/history.rs` | 会話履歴の読み出し。 |
| `codex-rs/voiceagent-server/src/llm_health.rs` | LLM接続先のTCPポート監視。 |
| `codex-rs/voiceagent-server/src/main.rs` | チャット、ストリーミング、WebSocket、履歴等のAPI。 |
| `codex-rs/voiceagent-server/src/manual_compaction.rs` | 手動コンテキスト圧縮。 |
| `codex-rs/voiceagent-server/src/project_rename.rs` | プロジェクト名変更。 |
| `codex-rs/voiceagent-server/src/project_rename_tests.rs` | プロジェクト名変更のテスト。 |
| `runtime/web/index.html` | 開発用Web画面。 |
| `runtime/web/llm-settings.html` | LLM設定画面。 |
| `runtime/web/server.py` | PythonのWebサーバー。 |
| `runtime/web/llm_settings.py` | 外部LLM設定の保存と読み込み。 |
| `runtime/characters/development/AGENTS.md` | 開発用指示。 |
| `runtime/prompts/default.md` | Codexの基本指示をコピーし、特定の編集ツールに関する指示を調整したもの。 |
| `cli/tege2dev` | ターミナルチャットクライアント。 |
| `scripts/entrypoint` | CoreとWebの起動管理。 |
| `Dockerfile`、`compose.yaml` | コンテナのビルドと起動構成。 |
| `.env.example` | 個人の実設定を含まない設定例。 |
| `.gitignore`、`.dockerignore`、`.gitattributes` | 公開対象、ビルド対象、改行等の設定。 |
| `README.md` | 汎用の利用説明。 |
| `projects/.gitkeep`、`secrets/.gitkeep`、`runtime/skills/.gitkeep` | 空の初期ディレクトリ。 |

## 保持したものと確認範囲

- 元のLICENSEとNOTICEは変更せず保持しています。
- 初回の比較では削除はありませんでした。その後、現在のコンテナビルドでは使用しない `codex-rs/vendor/bubblewrap/` の同梱ソース50ファイルを公開用から除外しました。
- コマンド実行用の `bwrap` は引き続きDockerfileでパッケージをインストールします。
- 元のCodexにある同梱bubblewrapの単独ビルド・Bazelビルド定義は、この公開用コンテナの対応範囲ではありません。
- 比較対象はこの公開用フォルダであり、別環境の実行中データではありません。
- この記録とファイルの注記は変更内容の記録です。同梱する全コードの個別ライセンス確認を完了したという意味ではありません。
