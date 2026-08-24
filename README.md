# Dependaboard

A focused operational dashboard for open Dependabot pull requests. Dependaboard combines a Dioxus fullstack UI, a libSQL read model, GitHub App authentication, and Restate workflows for durable merge, rebase, webhook, and reconciliation processing.

## Architecture

- `apps/web`: Dioxus web UI, server functions, and the signed GitHub webhook route.
- `apps/restate-service`: Restate virtual objects and workflows. All GitHub and libSQL side effects are journaled with `ctx.run`.
- `crates/core`: shared domain contracts, dependency parsing, check rollups, and GitHub error classification.
- `crates/github`: GitHub App JWT/token handling, canonical PR reads, merge calls, and idempotent user-authored Dependabot commands.
- `crates/store`: local or remote libSQL projection behind `PrStore`.

Restate owns in-flight truth and retries. libSQL is the cross-PR query model used by the dashboard.

## Prerequisites

- Rust 1.90 with the `wasm32-unknown-unknown` target
- Node.js and npm
- Dioxus CLI 0.7.10: `cargo install dioxus-cli --version 0.7.10 --locked`
- Docker with Compose for the local Restate server

## GitHub Setup

Create a GitHub App with these repository permissions:

- Pull requests: read and write
- Contents: read and write
- Checks: read
- Commit statuses: read
- Metadata: read

Subscribe it to `pull_request`, `check_suite`, `check_run`, and `status`. Configure its webhook URL as `https://your-host/api/webhooks/github` and use the same secret as `GITHUB_WEBHOOK_SECRET`.

Dependabot ignores commands posted by a GitHub App. `GITHUB_USER_PAT` must therefore be a fine-grained token for a real user with permission to post pull-request comments. Installation tokens are still used for reads and merges.

## Local Development

1. Create local configuration:

   ```sh
   cp .env.example .env
   set -a
   . ./.env
   set +a
   ```

2. Start Restate:

   ```sh
   docker compose up -d
   ```

   The Restate UI is available at <http://localhost:9070> and ingress at <http://localhost:8080>.

3. Start the service endpoint in one terminal:

   ```sh
   cargo run -p dependaboard-restate
   ```

4. Register the endpoint after it is listening:

   ```sh
   docker run --rm --network=host \
     docker.restate.dev/restatedev/restate-cli:1.7.6 \
     deployments register http://host.docker.internal:9080
   ```

   If the CLI container cannot resolve `host.docker.internal`, run `restate deployments register localhost:9080` with a locally installed Restate CLI instead.

5. Build the CSS and start the fullstack app in another terminal:

   ```sh
   npm install
   npm run css:build
   dx serve --package dependaboard-web
   ```

The service retries scheduler startup until Restate has discovered `WebhookIngress`. The first reconciliation then populates `data/dependaboard.db`.

The browser prompts for the single-user credentials configured by `DASHBOARD_USERNAME` and `DASHBOARD_PASSWORD`. The webhook route is outside this Basic Auth layer and is protected independently by its GitHub HMAC signature.

For live CSS changes, run `npm run css:watch` alongside `dx serve`.

## Configuration

| Variable | Used by | Purpose |
|---|---|---|
| `GITHUB_APP_ID` | Restate service | Numeric GitHub App ID |
| `GITHUB_INSTALLATION_ID` | Both | Installation to reconcile and target for manual sync |
| `GITHUB_PRIVATE_KEY` / `GITHUB_PRIVATE_KEY_PATH` | Restate service | RS256 App private key |
| `GITHUB_USER_PAT` | Restate service | User identity for `@dependabot rebase` comments |
| `GITHUB_WEBHOOK_SECRET` | Web app | HMAC-SHA256 webhook verification |
| `DASHBOARD_USERNAME` | Web app | Single-user HTTP Basic Auth username |
| `DASHBOARD_PASSWORD` | Web app | Required single-user HTTP Basic Auth password |
| `LIBSQL_URL` | Both | Local path or remote `libsql://` URL |
| `LIBSQL_AUTH_TOKEN` | Both | Remote libSQL/Turso token; empty locally |
| `RESTATE_INGRESS_URL` | Both | Restate HTTP ingress root |
| `RESTATE_AUTH_TOKEN` | Both | Optional bearer token for Restate Cloud |
| `RESTATE_SERVICE_ADDRESS` | Restate service | SDK endpoint bind address, default `0.0.0.0:9080` |
| `SYNC_DEBOUNCE_SECONDS` | Restate service | Leading/trailing per-PR webhook debounce, default 20 |
| `RECONCILE_INTERVAL_SECONDS` | Restate service | Installation sweep interval, default 3600 |

For production, point both binaries at the same remote libSQL database, expose only the web application publicly, deploy the Restate endpoint where Restate can reach it, and use Restate Cloud ingress credentials.

## Verification

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo check -p dependaboard-web --features server --no-default-features
cargo check -p dependaboard-web
npm run css:build
```

## License

Licensed under either Apache-2.0 or MIT, at your option.
