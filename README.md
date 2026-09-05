# Dependaboard

A focused operational dashboard for open Dependabot pull requests. Dependaboard combines a Dioxus fullstack UI, a libSQL read model, GitHub App authentication, and Restate workflows for durable merge, rebase, branch update, webhook, and reconciliation processing.

## Quickstart

This guide starts the complete application locally. Restate and its CLI run through Docker Compose; the two Dependaboard binaries run on the host. A Tailscale Funnel, Cloudflare Quick Tunnel, or equivalent HTTPS tunnel exposes only the web app to GitHub.

The local ports are intentionally distinct:

| Port | Component | Exposure |
|---|---|---|
| `8080` | Restate ingress | Localhost only |
| `8081` | Dependaboard web app and webhook | Localhost plus HTTPS tunnel |
| `9070` | Restate administration UI/API | Localhost only |
| `9080` | Dependaboard Restate service endpoint | Loopback or Docker host gateway only |

### 1. Install prerequisites

Install these tools before continuing:

- Git and a GitHub account that can create and install a GitHub App.
- Docker Engine or Docker Desktop with Docker Compose v2.
- A POSIX shell on Linux, macOS, or WSL2.
- Rustup. The checked-in `rust-toolchain.toml` installs the pinned Rust toolchain, `rustfmt`, `clippy`, and the `wasm32-unknown-unknown` target automatically.
- Node.js 20 or newer and npm.
- A C/C++ compiler and linker, CMake, pkg-config, curl, and OpenSSL.
- Either `cloudflared`, Tailscale with Funnel enabled, or an equivalent HTTPS tunnel.

For example, install the native build tools on Ubuntu or WSL2 with:

```sh
sudo apt-get update
sudo apt-get install build-essential clang cmake pkg-config libssl-dev curl openssl
```

On macOS, install the Xcode command-line tools, then the remaining packages with Homebrew:

```sh
xcode-select --install
brew install cmake pkg-config curl openssl
```

Install the matching Dioxus and wasm-bindgen CLIs:

```sh
cargo install dioxus-cli --version 0.7.10 --locked
cargo install wasm-bindgen-cli --version 0.2.126 --locked
```

If you use [devenv](https://devenv.sh/), `devenv shell` provides Rust, Node.js, npm, Dioxus CLI, and the matching wasm-bindgen CLI instead.

Verify the required tools:

```sh
docker compose version
rustc --version
dx --version
wasm-bindgen --version
node --version
npm --version
cc --version
cmake --version
pkg-config --version
curl --version
openssl version
```

Also run `cloudflared --version` or `tailscale version`, matching the tunnel selected below.

### 2. Install project dependencies

From the repository root:

```sh
npm ci
npm run css:build
cargo check --workspace
```

The application uses embedded local libSQL by default, so no database server is required. Its file will be created at `data/dependaboard.db`.

### 3. Start Restate with Compose

Compose derives its project name and named volume from the checkout directory. If you have multiple checkouts with the same directory name, set a unique `COMPOSE_PROJECT_NAME` in every terminal before running Compose commands.

```sh
docker compose up -d restate
docker compose ps
```

Wait until the Compose-managed CLI can reach Restate:

```sh
until docker compose --profile tools run --rm restate-cli whoami; do
  sleep 2
done
```

Restate persists data in the `restate-data` named volume. Its UI is available at <http://127.0.0.1:9070> and ingress at <http://127.0.0.1:8080>.

### 4. Start an HTTPS tunnel

Dependaboard will run on `127.0.0.1:8081`. Start one tunnel and keep it running for the rest of the setup.

Cloudflare Quick Tunnel:

```sh
cloudflared tunnel --url http://127.0.0.1:8081
```

Tailscale Funnel:

```sh
tailscale funnel 8081
```

Record the public HTTPS origin printed by the tunnel, for example `https://dependaboard.example.ts.net` or `https://random-name.trycloudflare.com`. The GitHub webhook URL will be:

```text
https://YOUR-TUNNEL-HOST/api/webhooks/github
```

An ephemeral Cloudflare Quick Tunnel gets a new hostname after restart. Update the GitHub App webhook URL whenever that hostname changes. Do not tunnel Restate ports `8080`, `9070`, or the service endpoint on `9080`.

### 5. Create the GitHub App

Open GitHub and go to **Settings > Developer settings > GitHub Apps > New GitHub App**. Organization owners can instead create the app under the organization's developer settings.

Set the basic fields:

- **GitHub App name:** any globally unique name, such as `your-login-dependaboard-local`.
- **Homepage URL:** the public tunnel origin.
- **Webhook:** active.
- **Webhook URL:** the tunnel origin plus `/api/webhooks/github`.
- **Webhook content type:** `application/json`. GitHub defaults to `application/x-www-form-urlencoded`, which the receiver rejects with `400`.
- **Webhook secret:** a new random secret. Generate one with `openssl rand -hex 32` or a password manager and retain it for `GITHUB_WEBHOOK_SECRET`.
- **Callback URL, setup URL, device flow:** leave disabled or empty; the MVP does not use OAuth.
- **Where can this GitHub App be installed?:** choose the account scope appropriate for the repositories you will monitor. For local testing, limiting it to your own account is simplest.

Set these **Repository permissions**. The same installation token serves both APIs the service uses: one GraphQL query reads each pull request's snapshot, and REST performs the writes and lists installations and pull requests.

| Permission | Access | Needed for |
|---|---|---|
| Pull requests | Read and write | Reading pull request snapshots (title, state, author, labels, mergeability), listing open Dependabot pull requests, merging, and posting `@dependabot` commands |
| Contents | Read and write | Reading the head commit message that carries Dependabot's update metadata, and updating a branch |
| Checks | Read-only | Check runs and check suites in the snapshot's check rollup |
| Commit statuses | Read-only | Legacy commit statuses in the snapshot's check rollup |
| Metadata | Read-only | Resolving repositories and listing the installation's repositories (granted to every App) |

Subscribe to these repository events:

- Pull request
- Check run
- Check suite
- Status

`installation` and `installation_repositories` events are delivered automatically and do not appear as normal subscription choices.

Create the app, then record the numeric **App ID** shown on its settings page.

Under **Private keys**, generate a private key. Move the downloaded PEM outside the repository and restrict its permissions:

```sh
mkdir -p "$HOME/.config/dependaboard"
mv "/path/to/downloaded-app.private-key.pem" "$HOME/.config/dependaboard/github-app.pem"
chmod 600 "$HOME/.config/dependaboard/github-app.pem"
```

### 6. Install the GitHub App

On the GitHub App settings page, select **Install App**, choose the user or organization, and grant access to either all repositories or selected repositories.

Record the numeric installation ID from the resulting browser URL. It is the final number in URLs shaped like one of these:

```text
https://github.com/settings/installations/12345678
https://github.com/organizations/ORG/settings/installations/12345678
```

The configured installation is the complete scope of one local Dependaboard instance.

Dependaboard can start with an empty dashboard. To exercise it end to end, at least one selected repository must have an open Dependabot version-update pull request. Existing repositories may already have one. For a Cargo repository, a minimal `.github/dependabot.yml` is:

```yaml
version: 2
updates:
  - package-ecosystem: cargo
    directory: /
    schedule:
      interval: weekly
```

Commit that file and use the repository's **Insights > Dependency graph > Dependabot** page to trigger or inspect updates. GitHub creates a pull request only when an eligible dependency update exists.

### 7. Create the user PAT

GitHub ignores `@dependabot rebase` commands authored by a GitHub App, so rebases require a real user's fine-grained personal access token.

Open **Settings > Developer settings > Personal access tokens > Fine-grained tokens > Generate new token** and configure:

- **Resource owner:** the user or organization that owns the installed repositories.
- **Repository access:** the same repositories selected for the GitHub App.
- **Pull requests:** read and write.
- **Expiration:** a short period appropriate for local development.

The token's user must have write access to those repositories. An organization may require administrator approval or SSO authorization before the token can access its repositories. Use that user's GitHub login as `DASHBOARD_USERNAME`; the application scopes the environment PAT to that authenticated dashboard identity.

Record the token once as `GITHUB_USER_PAT`. It remains in process memory and is never placed in Restate inputs or state.

### 8. Configure the environment

Create separate ignored environment files for the two host processes and restrict them:

```sh
cp .env.restate.example .env.restate
cp .env.web.example .env.web
chmod 600 .env.restate .env.web
```

Edit both files. Set the same installation ID, dashboard username, libSQL URL, and Restate ingress URL in each. Put GitHub App credentials and the user PAT only in `.env.restate`; put the webhook secret and dashboard password only in `.env.web`. This prevents the Dioxus process from inheriting the PAT or App private key.

Insert the webhook secret retained in step 5 into `.env.web`. Generate a separate dashboard password and insert it there too:

```sh
openssl rand -hex 24  # DASHBOARD_PASSWORD, not reused anywhere else
```

If you did not retain the webhook secret, generate a replacement with `openssl rand -hex 32`, update both GitHub and `.env.web`, and use the same exact value in both places.

The files are sourced as shell code. Keep values single-quoted as shown, do not add spaces around `=`, and never paste untrusted text into them. The generated hexadecimal secrets and GitHub token are safe inside single quotes.

`GITHUB_MERGE_METHOD` is the preferred merge method: `merge`, `squash`, or `rebase`. Each repository sync reads which methods the repository allows; a repository that disallows the preference is merged with the first allowed of squash, merge, rebase instead, and the confirmation dialog names such repositories with the method they will use. A repository that allows none of the three (or that has not been synced since installation) still gets the preference, and GitHub rejects the merge per PR.

The Restate SDK endpoint is unauthenticated and must not listen on a LAN interface. On Docker Desktop, leave `RESTATE_SERVICE_ADDRESS='127.0.0.1:9080'`. On native Linux, find Docker's host gateway:

```sh
docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}'
```

Replace `127.0.0.1` in `.env.restate` with that address, for example `RESTATE_SERVICE_ADDRESS='172.17.0.1:9080'`. This is the same host gateway reached by `host.docker.internal` in Compose. Do not use `0.0.0.0:9080` unless a host firewall explicitly blocks that port on every non-Docker interface.

### 9. Start the Restate service endpoint

In terminal 1:

```sh
set -a; . ./.env.restate; set +a
cargo run -p dependaboard-restate
```

Leave it running. It listens on the configured loopback or Docker host-gateway address and retries scheduler startup until the endpoint has been registered with Restate.

### 10. Register the endpoint through Compose

In another terminal, after the service endpoint is listening:

```sh
docker compose --profile tools run --rm restate-cli \
  deployments register http://host.docker.internal:9080
```

The CLI shares Restate's network namespace, and Compose maps `host.docker.internal` back to the host service on Linux and Docker Desktop. Re-run registration after changing Restate handler signatures. During local development, append `--force` only when you intentionally want Restate to accept a breaking service definition change.

Confirm that `PullRequest`, `BulkAction`, `InstallationSync`, `RepoSync`, `WebhookIngress`, `DashboardIngress`, and `SchedulerIngress` appear at <http://127.0.0.1:9070>.

### 11. Start the web app

In terminal 2:

```sh
set -a; . ./.env.web; set +a
npm run css:build
dx serve --package dependaboard-web --platform web \
  --addr 127.0.0.1 --port 8081
```

Open <http://127.0.0.1:8081> and sign in with `DASHBOARD_USERNAME` and `DASHBOARD_PASSWORD`.

The startup scheduler performs the first installation reconciliation and creates `data/dependaboard.db`. The dashboard may take a few seconds to populate. If the selected repositories have no open Dependabot pull requests, an empty dashboard is expected.

The public tunnel now forwards GitHub requests to the same web server. The dashboard and generated server functions require Basic Auth; `/api/webhooks/github` is outside that layer and instead requires GitHub's HMAC signature.

For live CSS changes, run `npm run css:watch` in another terminal.

### 12. Verify the local installation

Check the authenticated dashboard locally:

```sh
set -a; . ./.env.web; set +a
curl -fsS -u "$DASHBOARD_USERNAME:$DASHBOARD_PASSWORD" \
  http://127.0.0.1:8081/ >/dev/null
```

Check Restate and its service registration:

```sh
docker compose --profile tools run --rm restate-cli whoami
docker compose --profile tools run --rm restate-cli deployments list
```

In the Restate UI, confirm the deployment lists `PullRequest`, `BulkAction`, `InstallationSync`, `RepoSync`, `WebhookIngress`, `DashboardIngress`, and `SchedulerIngress`. The Restate-service terminal should log `installation scheduler accepted by Restate`. In **Invocations**, confirm a completed `InstallationSync/tick` or `InstallationSync/sync_now` and completed `RepoSync/reconcile` invocations; those establish that reconciliation itself finished.

Use the dashboard's **Sync** button to request another reconciliation; it appears in **Invocations** as `DashboardIngress/sync_installation`. The repository filter should list the repositories selected during App installation, even if none has an open Dependabot PR. Open a pull-request drawer to verify durable status history, then test merge or rebase only on a disposable repository where those actions are safe.

In the GitHub App settings, open **Advanced > Recent deliveries**. Redeliver an `installation`, `installation_repositories`, or subscribed repository event and confirm it receives HTTP `200`. Dependaboard acknowledges GitHub's `ping` event with `204` without routing it. A `401` indicates a webhook-secret mismatch; a `400` indicates a malformed signature, a missing delivery header, or a content type other than `application/json`; a `502` indicates the web app could not enqueue the event into Restate.

Finally, repeat the dashboard curl against the public HTTPS tunnel origin. This confirms the tunnel reaches the web process; do not use `-k` to bypass TLS verification:

```sh
curl -fsS -u "$DASHBOARD_USERNAME:$DASHBOARD_PASSWORD" \
  https://YOUR-TUNNEL-HOST/ >/dev/null
```

### 13. Stop or reset the stack

Stop the Rust and Dioxus processes and the tunnel with `Ctrl-C`, then stop Compose:

```sh
docker compose down
```

That preserves Restate's named volume and the local `data/dependaboard.db`. To delete all local orchestration and projection state and start over:

```sh
docker compose down -v
rm -f data/dependaboard.db
```

`docker compose down -v` removes only the current Compose project's volume. If you set `COMPOSE_PROJECT_NAME`, use the same value for teardown.

To decommission the local installation completely, run the state-deletion commands above, stop the tunnel, uninstall or disable the GitHub App webhook, revoke the fine-grained PAT, and remove `.env.restate`, `.env.web`, and the downloaded App private key. Delete the GitHub App too if it was created only for this checkout.

## Troubleshooting

**Port 8080 is already in use**

Restate owns local port `8080`; run Dioxus on `8081` exactly as shown. Stop any previous Restate or Dioxus process before retrying.

**Restate cannot discover `host.docker.internal:9080`**

Confirm `cargo run -p dependaboard-restate` is still running. On native Linux, verify `RESTATE_SERVICE_ADDRESS` uses the gateway reported by `docker network inspect bridge`; on Docker Desktop, use `127.0.0.1`. The Compose file maps `host.docker.internal` to Docker's host gateway.

**Registration reports an incompatible deployment**

For a deliberate local handler change, repeat the registration command with `--force`. Do not use `--force` against shared or production Restate environments without reviewing the compatibility impact.

**The GitHub webhook returns 401**

Ensure the GitHub App's webhook secret exactly matches `GITHUB_WEBHOOK_SECRET`, then restart `dx serve` after changing `.env.web`. The web app refuses to start when that variable is missing or empty, so a running process always has a secret configured.

**The GitHub webhook returns 400**

Confirm the GitHub App's webhook content type is `application/json`, not GitHub's `application/x-www-form-urlencoded` default. A `400` also covers a signature header that is not `sha256=` plus 64 hexadecimal characters, and a delivery missing `X-GitHub-Delivery` or `X-GitHub-Event`.

**The GitHub webhook cannot connect**

Confirm the tunnel still points to `http://127.0.0.1:8081`. If an ephemeral tunnel hostname changed, update the GitHub App webhook URL and redeliver the event.

**The dashboard is empty**

Confirm the App is installed on the expected repositories, `GITHUB_INSTALLATION_ID` matches that installation, and an installed repository has an open PR authored by `dependabot[bot]`. Check the Restate UI and the Restate-service terminal for reconciliation failures.

**Merge is rejected**

Confirm the repository allows at least one of squash, merge, and rebase, required checks and branch protection permit the merge, and the GitHub App still has Pull requests and Contents write access. The per-repository method is resolved at repository sync, so after changing a repository's merge settings or `GITHUB_MERGE_METHOD`, run **Sync** (or wait for the next reconciliation) before merging.

**Rebase comments return 403**

Confirm the fine-grained PAT has Pull requests read/write access, its user can write to the repository, and `DASHBOARD_USERNAME` is that user's GitHub login.

**Update branch is rejected**

Update branch merges the base into the pull request's head branch under the App's identity, so it needs the same Contents write access as merge and a head branch the App may push to; a stale head SHA or a pull request GitHub reports as not mergeable is rejected rather than retried. Note that Dependabot stops rebasing a pull request once another commit lands on it, so prefer **Request rebase** when a PAT is configured.

**Restate fails after a Compose recreation**

The Compose file fixes `RESTATE_NODE_NAME=dependaboard` so the current project's named volume remains valid across container recreation. If the volume came from an older configuration with a different node name, reset it once with `docker compose down -v`.

## Architecture

- `apps/web`: Dioxus web UI, authenticated server functions, and the signed GitHub webhook route.
- `apps/restate-service`: Restate virtual objects and workflows. GitHub and libSQL side effects are journaled with `ctx.run`.
- `crates/core`: shared domain contracts, dependency parsing, check rollups, and GitHub error classification. Dependabot metadata parsing is behind the opt-in `dependabot-metadata` feature so the browser bundle's dependency graph stays free of `regex`, `semver`, and `serde_yml`; only `crates/github` enables it.
- `crates/github`: GitHub App JWT/token handling, canonical PR reads, merge calls, and idempotent user-authored Dependabot commands. A pull request snapshot is one GraphQL query (it was five or more REST requests: pull request, head commit, paginated check runs, paginated check suites, combined status); only a commit with more than a hundred check contexts or suites costs a further request per extra page. Mutations and installation listing stay on REST.
- `crates/store`: local or remote libSQL projection behind `PrStore`.

Restate owns in-flight truth and retries. libSQL is the cross-PR query model used by the dashboard. The browser never calls Restate directly.

### Live refresh

The read model changes behind the dashboard's back — webhooks land, the hourly sweep runs — and the dashboard follows without a hand on it. The store keeps a `projection_revision` counter that SQLite triggers move on every insert, update, or delete of a pull request or repository row (a cascade included), so "has anything changed?" is one cheap read rather than a comparison of the rows. While the tab is showing, the dashboard polls that revision every ten seconds and reloads the rows and facets only when it has moved; a hidden tab polls nothing and catches up the moment it shows again. The relative times tick once a minute against the same clock.

The global **Sync** button is one-way — Restate takes the request and the sweep runs on its own — so the glyph spins from the click until the rows reload, which the poll brings about within a second of the sweep's first write. The poll runs every second while a sync is being followed and gives up after a minute, reloading once, if nothing reaches the store.

### Selection and the batch limit

A row's box selects it; the box in the table header selects or clears every row on the page, and shows a mixed mark while only some are selected. **Select all N matching** in the result bar selects everything the filter matches, pages beyond the current one included: the server resolves the filter to its newest rows, up to the batch limit of 100 pull requests, and returns the total alongside, so when a filter matches more than one batch takes, the action bar says how many it took and why. The selection belongs to the filter, not the page: **Load next** and the browser's back and forward keep it, changing a filter drops it. A selection that outgrows the limit — rows picked one by one on later pages can do that — has its actions withheld until it is trimmed, rather than being refused by the server after confirmation.

The confirmation dialog counts the pull requests and the repositories they span for every action. For a merge it also counts the rows whose last known check rollup is not green, and says that the count is the read model's last word, not a gate: the dashboard does not re-verify checks before merging (see `spec.md`), so required checks are enforced by branch protection or not at all.

### Restate ingress visibility

Every Restate handler is reachable through the ingress unless marked private, and `BulkAction.run` can merge pull requests, so only the entry points the web app and the bootstrap need are public. Everything else is `ingress_private`, reachable only from another handler.

| Public (ingress-reachable) | Private (Restate-internal only) |
|---|---|
| `WebhookIngress.dispatch` — verified GitHub deliveries, forwarded by the web edge | `PullRequest.sync`, `.closed`, `.merge`, `.command`, `.update_branch` |
| `DashboardIngress.sync_installation`, `.sync_pull_request` — the dashboard's **Sync** buttons | `InstallationSync.*` |
| `BulkAction.run`, `.progress` — batch merges, rebases, and branch updates | `RepoSync.*` |
| `PullRequest.status` — the read the detail drawer polls | |
| `SchedulerIngress.start` — arms the reconcile chain at startup | |

The webhook dispatcher routes only what GitHub sends; the dashboard's manual refreshes have their own service, so a refresh skips the per-PR webhook debounce and records its completion id without the dispatcher knowing about it. Discovery tests in `pull_request.rs` and `dashboard.rs` pin the mixed-visibility `PullRequest` object and the public `DashboardIngress` service; keep this table in step with the `ingress_private` attributes when you change one.

The Restate service stops on `SIGINT` or `SIGTERM`: it closes its listener, gives in-flight invocations up to ten seconds to finish, and leaves anything still running for Restate to retry against the next instance.

`apps/web/src/components` is installed from the [dioxus-daisyui-components](https://github.com/sagikazarmark/dioxus-daisyui-components) registry and is not edited by hand. To update a component, re-run the install against a checkout of the registry:

```sh
cd apps/web
dx components add <name> --path /path/to/dioxus-daisyui-components --force
```

The components emit daisyUI class names only; `apps/web/styles/app.css` scans that directory, so `npm run css:build` picks up new classes.

### Schema migrations

The libSQL schema is versioned. `migrations/` holds one SQL file per version, `crates/store/src/migrations.rs` embeds them at compile time, and every process applies the pending ones when it connects. Each migration runs exactly once, in order, inside its own `BEGIN IMMEDIATE` transaction, and is recorded in the `schema_migrations` table; a current database is only read, and re-running is a no-op. Starting the web app and the Restate service together against a new version is safe: the second waits for the first's write lock (SQLite's busy timeout locally, the server's serialisation on a remote database), then finds the version already recorded.

To add a migration:

1. Create `migrations/NNNN_name.sql` with the next four-digit version. Use plain statements only: no `BEGIN`/`COMMIT`, and no connection settings such as `PRAGMA foreign_keys` (a no-op inside the transaction; `connect` already enables it). SQLite cannot add a constraint or change a collation in place; rebuild the table as `0002_pull_request_constraints.sql` does.
2. Append the entry to `MIGRATIONS` in `crates/store/src/migrations.rs`:

   ```rust
   Migration {
       version: N,
       name: "name",
       sql: include_str!("../../../migrations/NNNN_name.sql"),
   },
   ```

3. Run `cargo test -p dependaboard-store`. A test fails if the files in `migrations/` and the registry disagree or versions are not contiguous, and the runner tests apply the whole registry to a fresh database and to one that predates versioning.

Never edit a migration that has shipped; add a new one instead. `0001_initial.sql` alone must stay idempotent, because databases created before versioning already contain its tables and adopt it as a no-op on their first start.

## Configuration Reference

| Variable | Used by | Purpose |
|---|---|---|
| `GITHUB_APP_ID` | Restate service | Numeric GitHub App ID |
| `GITHUB_INSTALLATION_ID` | Both | Installation to reconcile; required at startup, and the web app refuses a per-PR sync for any other installation |
| `GITHUB_PRIVATE_KEY` / `GITHUB_PRIVATE_KEY_PATH` | Restate service | RS256 App private key value or absolute PEM path |
| `GITHUB_USER_PAT` | Restate service | User identity for `@dependabot rebase` comments |
| `GITHUB_MERGE_METHOD` | Restate service | Preferred merge method: `merge`, `squash` (default), or `rebase`; a repository that disallows it is merged with the first allowed of squash, merge, rebase |
| `GITHUB_WEBHOOK_SECRET` | Web app | HMAC-SHA256 webhook verification; required at startup |
| `GITHUB_API_URL` | Restate service | GitHub REST API root, default `https://api.github.com`. The GraphQL endpoint is derived from it: `/graphql` under that root, or `/api/graphql` when the root is GitHub Enterprise's `/api/v3` |
| `DASHBOARD_USERNAME` | Both | Basic Auth username and PAT identity |
| `DASHBOARD_PASSWORD` | Web app | Required Basic Auth password |
| `LIBSQL_URL` | Both | Local path or remote `libsql://` URL |
| `LIBSQL_AUTH_TOKEN` | Both | Remote libSQL/Turso token; empty locally |
| `RESTATE_INGRESS_URL` | Both | Restate HTTP ingress root |
| `RESTATE_AUTH_TOKEN` / `RESTATE_API_KEY` | Both | Optional bearer token for Restate Cloud ingress |
| `RESTATE_SERVICE_ADDRESS` | Restate service | SDK endpoint bind address; quickstart uses loopback or Docker's host gateway |
| `SYNC_DEBOUNCE_SECONDS` | Restate service | Leading/trailing per-PR webhook debounce, default 20; an unparsable value is warned about at startup and the default used |
| `RECONCILE_INTERVAL_SECONDS` | Restate service | Installation sweep interval, default 3600; an unparsable value is warned about at startup and the default used |
| `RUST_LOG` | Both | Rust tracing filter; the Restate service logs one line per handler invocation at `info` (`debug` for the polled `status`/`progress` reads) |

For production, point both binaries at the same remote libSQL database, expose only the web application publicly, deploy the Restate endpoint where Restate can reach it, and use authenticated Restate Cloud ingress.

## Development Verification

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo test -p dependaboard-core --all-features
cargo test -p dependaboard-web --features server --no-default-features
cargo check -p dependaboard-web --target wasm32-unknown-unknown
cargo clippy --workspace --all-targets -- -D warnings
npm run css:build
docker compose config --quiet
dx build --package dependaboard-web --platform web
```

## Live Acceptance

Automated tests do not possess GitHub credentials. Before relying on a deployment, run this checklist against a disposable repository covered by the configured installation:

1. Confirm signed `pull_request` (`opened`, `reopened`, `synchronize`, `edited`, `labeled`, `unlabeled`, or `closed`), `check_run` (`created` or `completed`), `check_suite` (`completed`), and `status` deliveries return `200` and update the projection.
2. Confirm malformed or incorrectly signed webhook deliveries return `400` or `401`.
3. Suspend and unsuspend the installation, wait beyond `RECONCILE_INTERVAL_SECONDS`, and confirm only one reconciliation chain remains active.
4. Delete the installation and confirm its projected repositories and pull requests are purged, and that `PullRequest/status` for one of them returns no state.
5. Close a Dependabot pull request while the Restate service is down, bring it back, wait for the next `RepoSync/reconcile`, and confirm the row is gone, `PullRequest/status` returns no state, and the detail drawer reports the pull request as no longer open.
6. Queue a merge and confirm GitHub records the App installation as actor and uses `GITHUB_MERGE_METHOD`. In a repository that disallows that method, confirm the dialog names the repository with the method it will use, and that the merge succeeds with it.
7. Queue a rebase and confirm the PAT user authors one marked, attributed `@dependabot rebase` comment.
8. Queue a branch update and confirm GitHub records the App installation as the author of the merge commit on the head branch, and that the row's head SHA and checks refresh without a manual sync.
9. Restart the Restate service during an in-flight batch and confirm target progress resumes.
10. Inspect Restate inputs, journals, and object state and confirm the PAT value is absent.

## License

Licensed under either Apache-2.0 or MIT, at your option.
