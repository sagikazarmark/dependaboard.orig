# Dependaboard

A focused operational dashboard for open Dependabot pull requests. Dependaboard combines a Dioxus fullstack UI, a libSQL read model, GitHub App authentication, and Restate workflows for durable merge, rebase, branch update, webhook, and reconciliation processing.

This README is the operator's and user's document: how to run the system, what each control does and says, and the [*Live Acceptance*](#live-acceptance) checklist. The architecture — the Restate entities and their contracts, the storage trait and schema, the error taxonomy, and what is deliberately out of the MVP — is [`spec.md`](spec.md); where a section below names a mechanism, the contract behind it is there. A change to what a user can observe edits this file; a change to a contract edits the spec ([ADR-0001](docs/adr/0001-spec-owns-contracts-readme-owns-behaviour.md)).

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

### 7. Create the user PAT (optional)

GitHub ignores `@dependabot rebase` commands authored by a GitHub App, so rebases require a real user's fine-grained personal access token. Merge and update branch run under the App's identity and need no token: a merge-only deployment can skip this step. The Restate service then starts with `rebase disabled` in its log, the dashboard withholds **Request rebase** and says why, and a rebase that reaches the service anyway is rejected per pull request rather than attempted.

Open **Settings > Developer settings > Personal access tokens > Fine-grained tokens > Generate new token** and configure:

- **Resource owner:** the user or organization that owns the installed repositories.
- **Repository access:** the same repositories selected for the GitHub App.
- **Pull requests:** read and write.
- **Expiration:** a short period appropriate for local development.

The token's user must have write access to those repositories. An organization may require administrator approval or SSO authorization before the token can access its repositories. Use that user's GitHub login as `DASHBOARD_USERNAME`; the application scopes the environment PAT to that authenticated dashboard identity.

Record the token once as `GITHUB_USER_PAT`. It remains in process memory and is never placed in Restate inputs or state. Leave the variable unset or empty to run without one.

### 8. Configure the environment

Create separate ignored environment files for the two host processes and restrict them:

```sh
cp .env.restate.example .env.restate
cp .env.web.example .env.web
chmod 600 .env.restate .env.web
```

Edit both files. Set the same installation ID, dashboard username, libSQL URL, and Restate ingress URL in each. Put GitHub App credentials and the user PAT (if any) only in `.env.restate`; put the webhook secret and dashboard password only in `.env.web`. This prevents the Dioxus process from inheriting the PAT or App private key.

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

The public tunnel now forwards GitHub requests to the same web server. The dashboard and generated server functions require Basic Auth. Behind that, every `POST` — which is every server function, reads included, since a server function is always a `POST` and a `GET` cannot change anything — is refused with `403` when the browser says it came from another site: a `Sec-Fetch-Site` of anything but `same-origin` or `none`, or, where the browser sends no fetch metadata, an `Origin` whose host and port are not the `Host` the request arrived with (`Origin: null` included; the scheme is not compared). A request carrying neither header — `curl`, a script — is admitted, by design: the policy exists to stop a page on another site spending the credentials the browser holds for the dashboard, and a caller that presents the credentials itself is not what it guards against. `/api/webhooks/github` is outside both layers and instead requires GitHub's HMAC signature.

Browsers send fetch metadata only to an HTTPS or loopback address, so under the quickstart — `127.0.0.1` and the tunnel — every request is decided by `Sec-Fetch-Site`, and a dashboard served over plain HTTP on any other host is decided by the `Origin`-against-`Host` fallback instead. That fallback needs the `Host` the browser sent, and `dx serve` does not pass it: the dev server is a proxy in front of the real server and rewrites `Host` to the inner server's own `127.0.0.1:<port>`. The fallback is therefore not exercised under the quickstart, and a browser that reached `dx serve` without fetch metadata would have the dashboard's own requests refused. A reverse proxy in front of a deployment must pass the browser's `Host` through for the fallback to hold; *Troubleshooting* has the `curl` that tells what a proxy passes.

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

**A server function returns 403 `cross-site request refused`**

The auth edge refused a `POST` the browser said came from another site, or whose `Origin` did not match the `Host` the server saw (step 11). Each refusal is logged at `warn` with the method, path, `Sec-Fetch-Site`, and `Origin`. A refusal with `site=None` and an `origin` naming the address you opened the dashboard at means the fallback comparison fired and the server saw a different `Host` than the browser sent: a proxy in front of it rewrote the header. `dx serve` does this — it sets `Host` to the inner server's `127.0.0.1:<port>`; verified with `dx` 0.7.10 by the `curl` below, refused through `dx serve` and admitted against the inner port — so a browser that reaches `dx serve` without fetch metadata has every server function refused, while one that sends `Sec-Fetch-Site` is decided by it and not affected. To find out what a proxy passes, POST a server function through it as a browser without fetch metadata would, with the `Origin` the browser would send; the path carries a hash, and with `RUST_LOG='info'` as `.env.web.example` sets it the server logs each at startup as `Registering: POST /api/<name><hash>`:

```sh
set -a; . ./.env.web; set +a
curl -s -o /dev/null -w '%{http_code}\n' \
  -u "$DASHBOARD_USERNAME:$DASHBOARD_PASSWORD" \
  -X POST -H 'Content-Type: application/json' -d '{}' \
  -H 'Origin: http://127.0.0.1:8081' \
  http://127.0.0.1:8081/api/load_signed_in_user<hash>
```

`200` means the server saw the `Host` the browser sent; `403` means the proxy rewrote it. Through `dx serve` it is `403`. Against the inner server's own port — logged at startup as `dependaboard web listening address=127.0.0.1:<port>` — with both the URL and the `Origin` changed to it, or through a reverse proxy that passes `Host` through, it is `200`.

**The dashboard is empty**

Confirm the App is installed on the expected repositories, `GITHUB_INSTALLATION_ID` matches that installation, and an installed repository has an open PR authored by `dependabot[bot]`. Check the Restate UI and the Restate-service terminal for reconciliation failures.

**Merge is rejected**

Confirm the repository allows at least one of squash, merge, and rebase, required checks and branch protection permit the merge, and the GitHub App still has Pull requests and Contents write access. The per-repository method is resolved at repository sync, so after changing a repository's merge settings or `GITHUB_MERGE_METHOD`, run **Sync** (or wait for the next reconciliation) before merging. A rejection that reads "head moved" is not a configuration problem: the pull request was pushed to after the table showed it, and **Retry rejected** in the batch drawer sends it again with its current head.

**Rebase comments return 403**

Confirm the fine-grained PAT has Pull requests read/write access, its user can write to the repository, and `DASHBOARD_USERNAME` is that user's GitHub login.

**Request rebase is disabled**

The Restate service has no `GITHUB_USER_PAT`, and says so at startup with `rebase disabled`. The dashboard asks the service what it can do when the page opens (`DashboardIngress/capabilities`) and withholds **Request rebase** with the reason; a rebase submitted regardless is rejected per pull request with "no GitHub user token is configured for @dependabot commands", and **Retry rejected** leaves those targets out. Set the PAT as in step 7 and restart the service; **Update branch** is the App-identity alternative meanwhile.

**Update branch is rejected**

Update branch merges the base into the pull request's head branch under the App's identity, so it needs the same Contents write access as merge and a head branch the App may push to; a stale head SHA or a pull request GitHub reports as not mergeable is rejected rather than retried on its own — the batch drawer's **Retry rejected** sends the rejected ones again with their current heads. Note that Dependabot stops rebasing a pull request once another commit lands on it, so prefer **Request rebase** when a PAT is configured.

**Restate fails after a Compose recreation**

The Compose file fixes `RESTATE_NODE_NAME=dependaboard` so the current project's named volume remains valid across container recreation. If the volume came from an older configuration with a different node name, reset it once with `docker compose down -v`.

## Architecture

- `apps/web`: Dioxus web UI, authenticated server functions, and the signed GitHub webhook route.
- `apps/restate-service`: Restate virtual objects and workflows. GitHub and libSQL side effects are journaled with `ctx.run`.
- `crates/core`: shared domain contracts, dependency parsing, check rollups, and GitHub error classification. Dependabot metadata parsing is behind the opt-in `dependabot-metadata` feature so the browser bundle's dependency graph stays free of `regex`, `semver`, and `serde_norway`; only `crates/github` enables it.
- `crates/github`: GitHub App JWT/token handling, canonical PR reads, merge calls, and idempotent user-authored Dependabot commands. A pull request snapshot is one GraphQL query (it was five or more REST requests: pull request, head commit, paginated check runs, paginated check suites, combined status); only a commit with more than a hundred check contexts or suites costs a further request per extra page. Mutations and installation listing stay on REST.
- `crates/store`: local or remote libSQL projection behind two traits, `ProjectionWriter` and `ProjectionReader`, one per process (spec §4): the pull requests and repositories the dashboard queries, the batches it lists for audit — the finished ones for good, the running ones while they run — and the retirement outbox — the pull requests a reconciliation prune has removed whose Restate objects have not yet been told, written in the prune's own transaction so a step whose result Restate lost is still made good by the next drain.

Restate owns in-flight truth and retries. libSQL is the cross-PR query model used by the dashboard. The browser never calls Restate directly.

### Live refresh

The read model changes behind the dashboard's back — webhooks land, the hourly sweep runs — and the dashboard follows without a hand on it. The store keeps a `projection_revision` counter that SQLite triggers move on every insert, update, or delete of a pull request or repository row (a cascade included), so "has anything changed?" is one cheap read rather than a comparison of the rows. While the tab is showing, the dashboard polls that revision every ten seconds and reloads the rows and facets only when it has moved; a hidden tab polls nothing and catches up the moment it shows again. The relative times tick once a minute against the same clock.

The global **Sync** button is one-way — Restate takes the request and the sweep runs on its own — so the glyph spins from the click until the sweep reaches the pull requests. The projection's counter cannot say when that is: the sweep writes every repository row before it fetches a single pull request, and each of those writes moves it. So `projection_revision` carries a second counter, `pull_requests`, that only the pull request triggers move, and the glyph follows that one: it stops the first time the poll sees it move, which is within a second of the first pull request row landing — refreshed, added, or removed. The rows reloading for any other reason (a repository the sweep wrote first, a retry after a failed read) leaves the glyph turning; a pull request written by anything else in the meantime — the drawer's own sync, a batch landing a merge — stops it, since the counter cannot say who wrote. The poll runs every second while a sync is being followed and gives up after a minute, reloading once, if no pull request row is reached — nothing reached the store, or the installation has no pull requests to reach.

The detail drawer follows the page rather than keeping the row it opened with: each time the rows land, the open pull request's row is taken from them if the page shows it, so a head that moved is the head the drawer would submit. A row the page does not show — the page is one slice of one filter — is asked about; a pull request the read model no longer has is said to be no longer open, with every action withheld, until a page shows it again. The sidebar's facets take the summary's place as it stands — still being read, failed with a **Retry**, or the counts — rather than show zeros; and while the summary is not in hand, the merge confirmation says why it cannot name each repository's merge method and withholds the confirm.

The same poll is the dashboard's word on its line to the read model, so a viewer who is not clicking anything is told when the rows have stopped being kept current. Three polls in a row that get no revision — a dead server, a proxy answering for it, a store the server cannot reach; the poll cannot tell these apart, and either way the rows are not being refreshed — put a banner over the page naming the age of the last successful refresh, and turn the footer's dot red; the polls go on, and the first answer after any miss clears the banner and reloads the rows, since a read that failed in the gap is not retried by anything else. A 401 is told apart from all of that: the server was reached and refused the credentials the browser holds, as after a password rotation under an open tab, so one is enough, the banner says to reload the page to sign in again, and every failed call says the same rather than "could not be reached". It also ends the polling — the revision's, a followed batch's, the wait on a pull request's own **Sync** from its drawer, and the clock the relative times tick against — rather than ask again every ten seconds: each 401 carries the server's `WWW-Authenticate: Basic` challenge, which the browser turns into its own credential prompt, so a poll that went on would prompt every ten seconds, and cancelling one prompt would buy ten seconds. The page stands as it was left, with the banner over it, until the reload the banner asks for, which signs in again and starts everything over. Actions are not withheld while the line is down — each fails with its own message if it cannot get through, and a merge is guarded by the head SHA it was confirmed with. Once the banner says the page is signed out, though, what would poll is not started: a bulk action's submission, a pull request's **Sync** from its drawer, and **Retry rejected** each fail with that message without asking, since the answer would be another 401 and another prompt. The footer also names who is signed in, as the server saw the credentials, and that is the requester a batch is recorded under. It is the deployment's identity, not a person's: the dashboard has one set of credentials, so the only name the server ever admits — and the only one a batch is ever recorded under — is `DASHBOARD_USERNAME`, or `dependaboard` when that is unset. Step 7 suggests a GitHub login for it, the PAT user's, which is why it can read like a person's name; but two people who share the credentials record the same name, and the "by" on a batch says which deployment asked, not who was at the keyboard. Per-person identity is outside the MVP by design (`spec.md` §6c and §7).

### Selection and the batch limit

A row's box selects it; the box in the table header selects or clears every row on the page, and shows a mixed mark while only some are selected. **Select all N matching** in the result bar selects everything the filter matches, pages beyond the current one included: the server resolves the filter to its newest rows, up to the batch limit of 100 pull requests, and returns the total alongside, so when a filter matches more than one batch takes, the action bar says how many it took and why. The selection belongs to the filter, not the page: **Load next** and the browser's back and forward keep it, changing a filter drops it. A selection that outgrows the limit — rows picked one by one on later pages can do that — has its actions withheld until it is trimmed, rather than being refused by the server after confirmation. Queuing a bulk action takes its rows, and only its rows, out of the selection once Restate has the batch: one pull request merged from its drawer leaves the rest of the selection standing for the next batch, and a submission Restate never took leaves the whole selection standing to try again.

A selected pull request can leave the dashboard between the selection and the click — merged by hand, or superseded by Dependabot. The server leaves it out of the batch rather than refuse the batch over it: the rest run, and a toast names each pull request left out with its repository, in the same words as the retry's notice below. A submission none of whose pull requests are still in the dashboard is refused as a whole, before anything reaches Restate.

The confirmation dialog counts the pull requests and the repositories they span for every action. For a merge it also counts the rows whose last known check rollup is not green, and says that the count is the read model's last word, not a gate: the dashboard does not re-verify checks before merging (see `spec.md`), so required checks are enforced by branch protection or not at all.

### Retrying a batch's rejected targets

A batch that finishes with rejected pull requests — a head that moved between the table and the click, a merge GitHub would not do, an identity it would not let — offers **Retry rejected** in its drawer. The dashboard syncs each rejected pull request again, waits for the refreshed rows, and queues them as a new batch of the same kind carrying their current head SHAs; the new batch takes the drawer over as any queued one does, and its record names the batch it retries, so the two are linked each way under **Batches**. Only rejections a fresh attempt can cure go again: a moved head, and a mergeability GitHub judges anew each time. A pull request GitHub refused the configured identity, or whose repository disallows the merge method, would be rejected the same way whatever its head; it is left out, as is one rejected as no longer open or gone by the time it is refreshed, and one whose refresh failed. A toast names each one and why, and the rest go on. A batch whose rejections are all of those kinds has nothing to retry and offers none. One failure is not a pull request's alone: the server refusing the credentials — a 401 to any target's sync or to a poll after it, as after a password rotation — stops the retry. The other targets are not asked after, since each ask would be refused the same with a credential prompt for it, nothing is queued, a toast says the retry was given up because you are no longer signed in, and the banner goes up; nor is a retry started from a page the banner is already over.

### Following a batch

A queued batch is followed from the pill over the page and the drawer it opens, which poll the workflow's progress once a second. The batch id goes into the URL as `batch`, beside the open pull request's `pr`, so a reload — or a link handed to a colleague — reopens the drawer on the batch and picks its progress up where Restate has it. The parameter rides along rather than being a move of its own: following a batch changes the address in place, back and forward keep following it, and the address is set right to say so. The URL's word on the batch is taken once, when the page opens; an old entry that names a batch since replaced or given up does not take it up again. One batch is followed at a time — queuing another, or following one from **Batches**, ends the follow before it.

A batch followed by id alone — from a link, or from **Batches** — is looked up in the projection before Restate is asked, since the projection holds every finished batch for good and lists every running one, while Restate keeps a batch's progress for seven days. A batch the projection holds finished opens in the drawer from its record at once, with every target and verdict, exactly as a batch found finished through Restate would — **Retry rejected** included where a rejection can be cured — and is not polled: there is nothing left to follow. One the projection lists as running opens the drawer on what the listing knows — "Listed as running: merge over 12 pull requests, by alice, started 3m ago" — and polls Restate for its progress, for as long as it takes: the workflow wrote the listing itself, so Restate has the batch. Only an id the projection has never heard of, running or finished, is left to Restate to vouch for; the drawer says so — a batch just queued may not be listed yet — and the follow gives it up once Restate has answered thirty times over that it has no progress for it. A projection that could not be read is not the end of it either: the fault is passed on in the drawer and Restate is asked as before. The drawer says which of these the follow is on while there is no progress to show, and the pill names the action once the listing has said what it is.

The dashboard never declares a batch lost. A batch is durable in Restate, and one target can legitimately sit for hours inside GitHub's budgets — each attempt at a call gets thirty minutes of transient retries, and a step makes up to four attempts around three rate-limit waits of up to an hour, for the guard read and again for the mutation — so any timeout the dashboard could pick would either fire inside the budget or so late it said nothing. Instead, once the progress has stood unchanged for a minute the pill says so beside its count (`merge: 47/100 · no progress 3m`) and its dot stops throbbing, and the drawer says for how long and what the dashboard can tell of why. While Restate has not spoken for a batch the dashboard submitted, the wait is on Restate to start it, and the drawer says so ("Waiting on Restate to start the batch, 3m after it took it"). Once it has, the dashboard cannot tell a call being retried inside its budgets from a service that has died — the workflow publishes progress only as verdicts land, so every kind of stall looks the same from the dashboard — so it does not name GitHub as the cause: "No progress for 47m. Whether a call is being retried or the service is down, the dashboard cannot tell; one call may take hours inside its budgets, and the batch is durable in Restate for all of them." A poll the server did not answer is passed on in the drawer over the progress last heard, and the next poll is taken; the follow goes on for as long as Restate answers. It ends only when the batch completes, when Restate would not take the submission, when a batch followed by id alone — from a link — that the projection has never heard of has been answered thirty times over that Restate has no progress for it either, which is a stale or foreign link, and is said to be — or when the server refuses the credentials the poll carries, since asking again every second would only be refused again, with a credential prompt each time. The batch carries on in Restate without being asked after; the pill says `· signed out` beside the count last heard and stops throbbing, the drawer says the dashboard has stopped asking and that a reload picks the batch up again, and the reload does, from the batch id in the URL. A submission the server refuses the credentials of is given up at once as not submitted, for that reason, rather than tried five times.

A batch that completes is announced in a toast. One whose every target succeeded is announced as complete and nothing more, and the toast goes on its own; one with a rejected or a failed target is announced with the whole tally, in the drawer's words — "Batch complete: 95 succeeded, 5 rejected, 0 failed. Open the batch for their reasons." — as a warning that stays until it is dismissed. A rejection is not a failure: GitHub said no to the request as sent, and the toast does not call it one. Nor is it a success, and a batch rejected whole — a rebase submitted while the deployment had no user token — is announced by the same rule as one rejected in part, with a tally that says so, rather than the dashboard guessing at a shared cause from the counts.

### Recent batches

Restate keeps a batch's progress for seven days after it finishes and then clears it. The batch itself is not lost: the workflow's last step writes the finished batch to libSQL — the kind, who asked for it (the deployment's `DASHBOARD_USERNAME`, not a person; see *Live refresh*), the batch it retries if it was queued from one's **Retry rejected** (`retried_from`), when it started and finished, the tally, and every target's verdict with the pull request named and linked, the head it was sent against (`head_sha`), and for a merge that landed the commit it made (`merge_sha`, inside the outcome) — once, inside `ctx.run`, against a store write that keeps the first record for a batch id. The head says what a merge was a merge *of*, which for a squash or rebase merge the merge commit cannot; the commit says what it made, and GitHub names it on the merged pull request as well as in its answer to the merge, so a merge found already done on a replay names it too. A branch update records no new head: GitHub finishes the update after it has answered, and the head is learned by the sync that follows. Rows from before these were kept read as unknown. Nothing later would redo that write, so the step is never given up: every store failure is retried, the ones the store calls terminal too, and a store that will not take the record — away, out of disk, or a migration behind — stalls the workflow rather than losing the row. The stall is visible: the invocation retries in the Restate UI, and once the record has been pending for half a minute every failed attempt is in the service log at `warn`, with the batch id, how long it has been pending, and the failure — marked as terminal-class when the store did not expect it to clear on its own, so you know to look at the store rather than wait it out. Put the store right and the record lands on the next attempt. The workflow's first step lists the batch as running in the same projection, with what was asked, by whom, which batch it retries if any, since when, and over how many pull requests; the finished record's write takes that listing away in the same transaction. Both carry the installation the service serves (`installation_id`), stamped by the workflow rather than taken from the request. The listing is a convenience, not the batch's truth: the store is given fifteen seconds to take it, and a batch whose listing the store would not take runs unlisted, with the refusal in the service log. A workflow that ends without a finished batch — cancelled, or failed past what a target's own verdict can carry — takes its listing away on the way out. The one exception is a batch cancelled while stalled on the record: every target has settled and the merges stand on GitHub, so its running row is kept as the last evidence of it rather than the batch vanishing from both lists.

**Batches** in the top bar opens a drawer listing the running batches first, each with **Follow**, which makes it the batch the pill and drawer follow — so a tab that lost a batch, or never had it, finds it here — then the twenty most recently finished, newest first, with **Show older** taking the list further back a page at a time. Each finished batch folds to a headline and opens to its targets. The headline shows the action, the tally, how many pull requests, who asked, how long ago it finished, and the batch id — the one a `?batch=<id>` link carries, so a link is matched to an entry without opening each, with **copy** beside it to make a link the other way. A batch queued from another's **Retry rejected** says which it retries, and that batch says which retry it (`retries <id>`, `retried as <id>, <id>`), running or finished, each as a link that opens the other entry — so what became of a pull request one batch rejected can be followed to the batch that merged it, and back. The age is a floor, and two batches a fortnight and three weeks old both read `2w`; hovering the age shows when the batch started and finished to the second, in UTC (`started 2026-09-07T10:41:03Z · finished 2026-09-07T10:43:17Z`), and the opened entry shows the same line over its targets. Each target shows its pull request as `owner/repo#number`, linked to GitHub, with the pull request's title beside it — the dependency and the versions — and the reason for every rejection or failure; a merged one shows the commit it made as a short SHA linked to the commit on GitHub, the whole SHA on hover. A running batch shows its id and the instant behind its age the same way, and its targets, in the progress drawer, read the same, the commit included the moment a merge lands. The list is read from the projection, not Restate, so a batch is still there long after its workflow has been forgotten, and after the pull requests it merged have left the table. It is the configured installation's list: two deployments sharing one libSQL URL each see only the batches their own workflow listed and recorded, and a `?batch=<id>` link to the other's batch is given up as a stale or foreign one, since the projection answers for it as for a batch it has never heard of. Batches from before the installation was kept were attributed at migration time through their targets' repositories; one whose repositories had all been purged by then is shown to no deployment. The drawer loads when it opens; a batch starting or finishing while it is showing is there the next time it does.

### Restate ingress visibility

Every Restate handler is reachable through the ingress unless marked private, and `BulkAction.run` can merge pull requests, so only the entry points the web app and the bootstrap need are public. Everything else is `ingress_private`, reachable only from another handler.

| Public (ingress-reachable) | Private (Restate-internal only) |
|---|---|
| `WebhookIngress.dispatch` — verified GitHub deliveries, forwarded by the web edge | `PullRequest.sync`, `.closed`, `.merge`, `.command`, `.update_branch` |
| `DashboardIngress.sync_installation`, `.sync_pull_request` — the dashboard's **Sync** buttons; `.capabilities` — what the service can do, read once per page | `InstallationSync.*` |
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

3. Run `cargo nextest run -p dependaboard-store`. A test fails if the files in `migrations/` and the registry disagree or versions are not contiguous, and the runner tests apply the whole registry to a fresh database and to one that predates versioning.

Never edit a migration that has shipped; add a new one instead. `0001_initial.sql` alone must stay idempotent, because databases created before versioning already contain its tables and adopt it as a no-op on their first start.

## Configuration Reference

| Variable | Used by | Purpose |
|---|---|---|
| `GITHUB_APP_ID` | Restate service | Numeric GitHub App ID |
| `GITHUB_INSTALLATION_ID` | Both | Installation to reconcile; required at startup. The web app refuses every request that names a pull request of another installation by key — a per-PR sync, a batch target, and the drawer's reads of a row and its durable state alike — and reads batches only within it: **Batches** lists the installation's, and a link to another's batch is answered as one the projection has never heard of. The Restate service stamps it on every batch its workflow lists and records |
| `GITHUB_PRIVATE_KEY` / `GITHUB_PRIVATE_KEY_PATH` | Restate service | RS256 App private key value or absolute PEM path |
| `GITHUB_USER_PAT` | Restate service | Optional user identity for `@dependabot rebase` comments; unset or empty disables **Request rebase**, logged at startup, while merge and update branch run as the App |
| `GITHUB_MERGE_METHOD` | Restate service | Preferred merge method: `merge`, `squash` (default), or `rebase`; a repository that disallows it is merged with the first allowed of squash, merge, rebase |
| `GITHUB_WEBHOOK_SECRET` | Web app | HMAC-SHA256 webhook verification; required at startup |
| `GITHUB_API_URL` | Restate service | GitHub REST API root, default `https://api.github.com`. The GraphQL endpoint is derived from it: `/graphql` under that root, or `/api/graphql` when the root is GitHub Enterprise's `/api/v3` |
| `DASHBOARD_USERNAME` | Both | Basic Auth username and PAT identity, default `dependaboard`; the one name the dashboard admits, and the requester every batch is recorded under |
| `DASHBOARD_PASSWORD` | Web app | Required Basic Auth password |
| `LIBSQL_URL` | Both | Local path or remote `libsql://` URL |
| `LIBSQL_AUTH_TOKEN` | Both | Remote libSQL/Turso token; empty locally |
| `RESTATE_INGRESS_URL` | Both | Restate HTTP ingress root |
| `RESTATE_AUTH_TOKEN` / `RESTATE_API_KEY` | Both | Optional bearer token for Restate Cloud ingress |
| `RESTATE_SERVICE_ADDRESS` | Restate service | SDK endpoint bind address; quickstart uses loopback or Docker's host gateway |
| `SYNC_DEBOUNCE_SECONDS` | Restate service | Leading/trailing per-PR webhook debounce, default 20; an unparsable value is warned about at startup and the default used |
| `RECONCILE_INTERVAL_SECONDS` | Restate service | Installation sweep interval, default 3600; an unparsable value is warned about at startup and the default used |
| `RUST_LOG` | Both | Rust tracing filter; the Restate service logs one line per handler invocation at `info` (`debug` for the polled `status`/`progress` reads and the once-per-page `capabilities` read) |

For production, point both binaries at the same remote libSQL database, expose only the web application publicly, deploy the Restate endpoint where Restate can reach it, and use authenticated Restate Cloud ingress.

## Development Verification

```sh
cargo fmt --all -- --check
cargo nextest run --workspace --no-default-features --features dependaboard-web/server
cargo check -p dependaboard-web --target wasm32-unknown-unknown
cargo clippy --workspace --all-targets --no-default-features --features dependaboard-web/server -- -D warnings
cargo deny check
npm run css:build
docker compose config --quiet
dx build --package dependaboard-web --platform web
```

The test and clippy lines carry one feature set, so neither builds the dependency tree a second time for another. The web crate's default `web` feature is the browser bundle; its server, its API and most of its UI tests are compiled only under `server`, and `--no-default-features` changes nothing for the other members, none of which defines a default.

The tests run under [cargo-nextest](https://nexte.st), which `devenv shell` provides (elsewhere, `cargo install cargo-nextest --locked`, or a pre-built binary from the same site). Each test runs in a process of its own, so a leaked global or a racing `env::set_var` fails one named test instead of racing the rest silently. `.config/nextest.toml` adds a slow-test limit — a test still running after 30s is reported, and terminated after a second period, so a hang is a failure with a name rather than a hung run — and one retry, scoped to the single store test that meets a real SQLite file lock from a second connection; the file names the test and says why, and nothing else is retried. `cargo test` over the same flags is the fallback: it must also pass, it is what CI runs today, and it is the only runner for doctests, of which there are none. `-P ci` selects the CI profile, which turns fail-fast off and writes a JUnit report to `target/nextest/ci/junit.xml`.

The suite is named by module, so a filterset (`-E`, see `cargo nextest help filterset`) selects a group of tests by what it touches, without renaming anything:

| Group | Touches | Filterset |
| --- | --- | --- |
| Pure | Nothing: core, the GitHub client's own logic, the Restate service against in-memory fixtures | `package(dependaboard-core) + (package(dependaboard-github) & kind(lib)) + package(dependaboard-restate)` |
| Store | SQLite on a temporary directory | `package(dependaboard-store)` |
| HTTP mock | A wiremock server on loopback, in the GitHub client's integration tests | `package(dependaboard-github) & kind(test)` |
| VirtualDom | Dioxus' VirtualDom, in the web UI | `package(dependaboard-web) & test(/^ui::/)` |
| Sockets | The web server and its API on loopback | `package(dependaboard-web) & (test(/^api::/) + test(/^server::/))` |

The five are a partition: together they are the whole suite, and no test is in two.

CI runs these checks through Dagger, from `.dagger/modules/ci`: a module of this repository that calls the Rust module's checks — and, for clippy, its container — with the arguments above, so `dagger check` at a checkout is what CI runs on a pull request. A pull request, and a push to `main`, run `dagger check`: `ci:fmt` (the first line), `ci:test` (the second line's flags, run by `cargo test` — every test, in one build), `ci:clippy` (the fourth), `ci:doc` (`cargo doc --no-deps` over the same feature set, with rustdoc denying warnings) and `ci:audit` (`cargo audit` of `Cargo.lock` against the RustSec database). A push to any other branch runs the fast path, `dagger api call ci quick`: `cargo test -p dependaboard-core -p dependaboard-store -p dependaboard-github -p dependaboard-restate`, the tests of the four members that do not pull in dioxus — most of what the full build compiles is dioxus and what sits under it. In both, cargo-chef compiles the dependencies from the manifests alone — one layer for the tests' build, one in check mode that clippy and rustdoc share — so a source edit recompiles the workspace crates only. The wasm check, `cargo deny`, the stylesheet, the Compose file and the `dx` build are not yet run by CI; nor is nextest, whose `ci` profile is there for the day the Dagger module runs it.

Work is tracked in GitHub Issues, one issue per commit. The commit's subject reads as the behaviour it delivers, and its body carries `Closes #N`, naming the issue that asked for it, so a review can trace a change back to its ticket and the ticket closes when the commit lands. `docs/agents/issue-tracker.md` has the rest of the convention.

## Live Acceptance

Automated tests do not possess GitHub credentials. Before relying on a deployment, run this checklist against a disposable repository covered by the configured installation:

1. Confirm signed `pull_request` (`opened`, `reopened`, `synchronize`, `edited`, `labeled`, `unlabeled`, or `closed`), `check_run` (`created` or `completed`), `check_suite` (`completed`), and `status` deliveries return `200` and update the projection.
2. Confirm malformed or incorrectly signed webhook deliveries return `400` or `401`.
3. Suspend and unsuspend the installation, wait beyond `RECONCILE_INTERVAL_SECONDS`, and confirm only one reconciliation chain remains active.
4. Delete the installation and confirm its projected repositories and pull requests are purged, and that `PullRequest/status` for one of them returns no state.
5. Close a Dependabot pull request while the Restate service is down, bring it back, wait for the next `RepoSync/reconcile`, and confirm the row is gone, `PullRequest/status` returns no state, and the detail drawer reports the pull request as no longer open.
6. Queue a merge and confirm GitHub records the App installation as actor and uses `GITHUB_MERGE_METHOD`. In a repository that disallows that method, confirm the dialog names the repository with the method it will use, and that the merge succeeds with it. Confirm a batch whose every pull request merged is announced with a plain `Batch complete` that goes on its own; then queue a merge that includes a pull request branch protection will not let merge, and confirm the toast is a warning that stays, reading `Batch complete: N succeeded, 1 rejected, 0 failed. Open the batch for their reasons.`, rather than a success.
7. Select several pull requests, close one of them on GitHub, and confirm the batch before the table has caught up. Confirm the rest run, a toast names the closed pull request with its repository as left out of the batch, the pill and drawer count only the pull requests that ran, and the selection is empty once the batch is queued. Then stop the Restate service and confirm a batch again: confirm the toast says it was not submitted and the selection is still standing.
8. Queue a rebase and confirm the PAT user authors one marked, attributed `@dependabot rebase` comment.
9. Start the Restate service without `GITHUB_USER_PAT` and confirm it logs `rebase disabled`, that **Request rebase** is disabled with its reason in the action bar and the drawer while **Update branch** and **Merge** are not, and that a `BulkAction/run` rebase sent to the ingress directly settles every target as rejected without a comment appearing on GitHub.
10. Queue a branch update and confirm GitHub records the App installation as the author of the merge commit on the head branch, and that the row's head SHA and checks refresh without a manual sync.
11. Stop the Restate service during an in-flight batch. After a minute, with the drawer closed, confirm the pill reads `· no progress 1m` beside its count and its dot has stopped throbbing; open the drawer and confirm its note says how long the batch has stood and that the dashboard cannot tell why, without naming GitHub or calling the batch lost. Bring the service back and confirm target progress resumes and the pill throbs again. Reload the dashboard during an in-flight batch and confirm the progress drawer reopens on it; open **Batches** in a second tab and confirm the batch is listed as running with **Follow**, and that it moves to the finished list once it has run.
12. Once a batch has finished, open **Batches** and confirm it is listed with its counts, requester, and id, that hovering its age shows when it started and finished in UTC and the opened entry repeats the line, and that each target shows its pull request's title beside a working link; press **copy** and confirm the id is on the clipboard and the entry has not folded or unfolded. Confirm it is still listed after `BulkAction/progress` for its id has stopped returning state. Then paste its `?batch=<id>` link into a fresh tab and confirm the drawer opens on the record at once, with every target's verdict and title, without waiting on Restate; paste a link with a well-formed id no batch has and confirm the drawer says the projection has no such batch and is asking Restate, and that after thirty seconds a toast says no batch by that id was found anywhere.
13. Queue a merge, push to one of its pull requests before it runs so that target is rejected as `head moved`, and press **Retry rejected**. Confirm the progress drawer shows the merged target's commit as a short SHA the moment it lands and that the link opens that commit on GitHub. Open **Batches** and confirm the retry's entry reads `retries <id>` with the first batch's id, that the first batch's entry reads `retried as <id>` with the retry's — while the retry is still running and once it has finished — and that clicking either link opens the other batch in the drawer. In `batch_targets`, confirm the retried target's row carries the head that was pushed as `head_sha` and its `outcome` carries the merge commit as `merge_sha`, and that a row from before this release reads back with neither and still lists.
14. Run two deployments — two web apps and two Restate services, each pair with its own `GITHUB_INSTALLATION_ID` — against one `LIBSQL_URL`, and queue a batch from each. Confirm **Batches** in either lists only its own batch, running and once finished, and that in `batches` and `running_batches` each row's `installation_id` is the installation whose service ran it. Paste the other deployment's `?batch=<id>` link into this one and confirm the drawer says the projection has no such batch and is asking Restate, and that after thirty seconds a toast says no batch by that id was found anywhere — the same as for a stale link. Against a database from before this release, confirm a batch whose repositories are still projected is listed under its installation, and one whose repositories had all been purged is listed under neither.
15. Make the projection store refuse writes as a batch's last target settles: with the default embedded store, make `data/dependaboard.db` and its directory read-only just before the last merge lands; with a remote `LIBSQL_URL`, stop the server. Confirm in the Restate UI that `BulkAction/run` is retrying its `record-batch` step rather than failed, that `BulkAction/progress` reports the batch complete, and that after half a minute the service log carries a `warn` line per attempt naming the batch id and how long the record has been pending. Restore the store and confirm the batch moves from the running to the finished list in **Batches**, with its counts and every target's verdict.
16. With a remote `LIBSQL_URL`, stop the store's server, then push to a Dependabot pull request's branch so a `pull_request` webhook is delivered while it is down. Confirm in the Restate UI that `PullRequest/sync` is retrying its `upsert-pr-projection` step rather than failed, with `transient projection-store failure` as its last cause. Bring the server back after a few seconds and confirm the invocation succeeds on its own, and that the row's head SHA and checks on the dashboard catch up without a manual sync or waiting for the next `RepoSync/reconcile`.
17. With the dashboard open, change `DASHBOARD_PASSWORD` and restart the web app. Cancel the browser's credential prompt once and confirm, over the next two minutes, that the banner reads `You are no longer signed in`, that no further credential prompt appears, and that the footer's `last event` time stands where it was. Repeat with a batch in flight and its drawer open: confirm the pill reads `· signed out` beside its count and has stopped throbbing, and the drawer says the dashboard has stopped asking after the batch and that a reload picks it up. Repeat with a pull request's drawer open, pressing its **Sync** and rotating the password while the button reads `Syncing...`: confirm one credential prompt at most, a toast that the sync was queued but completion could not be confirmed because you are no longer signed in, and the banner. Repeat once more with a finished batch that rejected several pull requests, pressing **Retry rejected** and rotating the password while the targets are being refreshed: confirm one credential prompt at most, that no retry batch appears in **Batches** or in the drawer, a toast that the retry was given up because you are no longer signed in, and the banner. With the banner up, press the drawer's **Sync** and **Retry rejected** again and confirm each fails with the same message and no prompt. Reload, sign in with the new password, and confirm the banner is gone, the rows refresh, and the drawer reopens on the batch.
18. Inspect Restate inputs, journals, and object state and confirm the PAT value is absent.

## License

Licensed under either Apache-2.0 or MIT, at your option.
