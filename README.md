# Tege2Dev

OpenAI Codex Coreを利用した、ローカルLLM向けの開発エージェントです。
Web画面またはターミナルから、プロジェクト単位の会話セッションを利用できます。
会話分類ルーターは含みません。

## 必要なもの

- Docker Compose、またはPodmanとComposeプロバイダー
- OpenAI互換APIを提供するLLMサーバー
- 初回ビルド時のネット接続

## 設定

`.env.example`を`.env`へコピーし、API認証キー、LLMのベースURLとモデル名を設定してください。
`host.docker.internal`はコンテナからホスト上のLLMへ接続するための例です。
別のサーバーを使う場合は、そのベースURLを設定してください。

`TEGE2DEV_PORT`はWeb公開ポートです。変更する場合は、公開用リンクに使う
`CODEX_AGENT_PUBLIC_BASE_URL`も合わせて変更してください。

## ビルドと起動

Docker Composeの場合:

```bash
docker compose build
```

```bash
docker compose up -d
```

Podmanの場合:

```bash
podman build --layers=false -t tege2dev:local .
```

```bash
podman compose up -d --no-build
```

標準のWeb接続先は `http://localhost:8093/` です。
LLM設定はWeb画面でも変更できます。変更後はコンテナを再起動してください。

## ターミナル

```bash
docker compose exec tege2dev tege2dev
```

プロジェクトを選択すると、Web画面と同じ会話セッションを利用します。

## 保存領域

- `runtime/`: Webソース、指示文、実行時に作成される設定と会話履歴
- `projects/`: 利用者が作成するプロジェクト
- `secrets/`: 必要に応じて利用者が配置する認証情報

初期状態には個人用スキル、実際の認証キー、会話履歴、プロジェクトを含めていません。
スキルとMCP設定は利用者が追加できます。MCP設定先は `runtime/codex/mcp.json` です。

## ソースとライセンス

`codex-rs/`はCodex由来のRustソースです。workspace定義、依存ソース、テスト、
コンパイル時の資産を保ち、単独でビルドできる構成を維持しています。
ただし、現在のコンテナビルドで使わない同梱bubblewrapのソースは除外しています。
コマンド実行にはDockerfileでインストールした`bwrap`を使います。
この構成はDockerfileのCargoビルドを対象とし、CodexのBazelビルドは対象にしていません。
WebサーバーとCLIはそれぞれ `runtime/web/`、`cli/` にあります。
Codex由来のライセンスと著作権表記は `LICENSE` と `NOTICE` を参照してください。
元のCodexとの変更内容と比較元の版は [変更記録](doc/CODEX_CHANGES.md) に記載しています。
