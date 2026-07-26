# Nanocentaur PoC

Nanocentaur is a host-managed agent API built from Axum, Nanocodex, NanoVM,
and SQLite. The model harness lives in the API process. Shell and filesystem
tools execute in a disposable libkrun VM against an explicit host workspace.

```text
HTTP request
    |
    v
Axum handler
    |
    +--> SQLite policy
    |      context key -> principal -> roles/grants + agent config
    |                         |
    |                         `-> secret references + request rules
    |
    `--> bounded mpsc command
             |
             v
         agent actor task
           - Nanocodex handle
           - active turn control
           - explicit queued turns
           - NanoVM lifecycle
             |
             v
         SQLite session actor
           - append-only typed event log
           - turns, inputs, queues
           - idempotency and checkpoints

agent VM -- HTTPS_PROXY + runtime CA --> host MITM proxy --> fixed upstream
             `-- scoped base URL -----> Axum fallback
                                           |
                                           `-> host secret provider
```

Each agent is a spawned actor. Axum handlers exchange typed commands and
one-shot replies with it. The actor owns only live runtime state; SQLite is the
source of truth. One blocking SQLite task receives typed commands over a
bounded channel, keeping database calls and mutable connections out of Axum
and agent tasks.

## Agent API

```text
POST   /v1/agent/new
GET    /v1/agent/:agent_id
DELETE /v1/agent/:agent_id
POST   /v1/agent/:agent_id/evict

POST   /v1/agent/:agent_id/turn
GET    /v1/agent/:agent_id/turn/:turn_id
POST   /v1/agent/:agent_id/turn/:turn_id/cancel

GET    /v1/agent/:agent_id/events

POST   /v1/agent/:agent_id/fork
POST   /v1/agent/:agent_id/turn/:turn_id/fork

POST   /v1/payment-sessions
GET    /health
```

Create or resolve an agent:

```http
POST /v1/agent/new
X-API-Key: ...
Content-Type: application/json

{"context_key":"opaque-client-owned-key"}
```

The context key is optional. When supplied, it is scoped to the authenticated
API client and provides create-or-return behavior. It is also resolved through
the SQLite context-binding table. It does not contain or prove permissions.

```json
{
  "agent_id": "d18a...",
  "created": true,
  "state": "idle"
}
```

Send typed content blocks:

```http
POST /v1/agent/d18a.../turn
X-API-Key: ...
Idempotency-Key: client-message-42
Authorization: Payment ...
Content-Type: application/json

{
  "content": [
    {"type":"text","text":"Inspect the failure"}
  ]
}
```

Delivery follows the Nanocodex state machine:

```text
idle agent    -> prompt() -> {"action":"started", ...}
running agent -> steer()  -> {"action":"steered", ...}
completion race
              -> TurnNotSteerable -> prompt() -> started
```

Queueing is explicit:

```json
{
  "delivery": "enqueue",
  "content": [
    {"type":"text","text":"Do this afterward"}
  ]
}
```

This calls `Nanocodex::prompt` even while a turn is active, using Nanocodex's
own FIFO. A full steering queue returns `409 steer_queue_full`; it never
silently changes the request into a queued turn.

The agent-level SSE stream spans multiple turns:

```http
GET /v1/agent/d18a.../events
Accept: text/event-stream
Last-Event-ID: 42
```

Nanocentaur control events are a typed enum. Native events retain
`nanocodex::AgentEvent`, including its typed `AgentEventKind`. No untyped
`serde_json::Value` crosses the domain boundary.

Forks select a completed turn and copy the durable session history through its
terminal event into a new agent. The source may still be running a later turn.
The child gets a fresh workspace and a newly provisioned VM; VM filesystems are
never copied as part of the fork contract.

## Durability

The durable session is stored in `$STATE_DIRECTORY/sessions.sqlite`:

```text
session_agents
turns
turn_inputs
turn_requests
session_events
session_forks
```

An accepted turn, its first input, idempotency mapping, and accepted event are
committed together before the request returns. Steering inputs, cancellation,
runtime events, outputs, and terminal status are also persisted. Agent SSE
replay reads SQLite, so `Last-Event-ID` works across API process restarts.

If the process exits during a turn, startup appends `turn.interrupted`, moves
the turn back to `queued`, wakes a fresh harness, and retries it under the same
`turn_id`. Explicit queued turns are reconstructed in order. Nanocodex's typed
model checkpoint is stored alongside the completed turn as a derived wake-up
optimization; the event log remains the session record.

NanoVM instances and root filesystems are cattle. Every runtime receives a
fresh rootfs copied or reflinked from the configured template. The host
workspace is an explicit mounted resource rather than a VM snapshot. Forks do
not implicitly copy it; durable outputs should be committed or exported as
artifacts.

## Policy API

The administration surface uses a separate bearer credential:

```text
Authorization: Bearer $NANOCENTAUR_ADMIN_TOKEN
```

```text
POST/GET          /admin/v1/api-clients
GET/PATCH/DELETE  /admin/v1/api-clients/:id
POST              /admin/v1/api-clients/:id/keys
DELETE            /admin/v1/api-clients/:id/keys/:key_id

POST/GET          /admin/v1/principals
GET/PATCH/DELETE  /admin/v1/principals/:id

POST/GET          /admin/v1/context-bindings
GET/PATCH/DELETE  /admin/v1/context-bindings/:id
POST              /admin/v1/context-bindings/resolve

POST/GET          /admin/v1/roles
GET/PATCH/DELETE  /admin/v1/roles/:id

POST/GET          /admin/v1/permissions
GET/DELETE        /admin/v1/permissions/:id

PUT/DELETE        /admin/v1/principals/:principal_id/roles/:role_id
GET               /admin/v1/principals/:principal_id/roles

PUT/DELETE        /admin/v1/roles/:role_id/permissions/:permission_id
GET               /admin/v1/roles/:role_id/permissions

PUT/DELETE        /admin/v1/principals/:principal_id/permissions/:permission_id
GET               /admin/v1/principals/:principal_id/permissions
GET               /admin/v1/principals/:principal_id/effective-permissions

POST/GET          /admin/v1/secrets
GET/PATCH/DELETE  /admin/v1/secrets/:id

PUT/DELETE        /admin/v1/principals/:principal_id/secrets/:secret_id
GET               /admin/v1/principals/:principal_id/secrets

PUT/DELETE        /admin/v1/roles/:role_id/secrets/:secret_id
GET               /admin/v1/roles/:role_id/secrets
GET               /admin/v1/principals/:principal_id/effective-secrets
```

Resolution:

```text
API key + optional context_key
              |
              v
          principal
          /       \
     roles/grants  agent_config snapshot
          |        - instructions
          |        - reasoning effort
          v
 tools / egress / secrets / agent operations
```

Roles contain permissions only. Principal `agent_config` controls behavior.
This avoids ambiguous prompt merging when a principal has multiple roles.
Permissions are resolved live before every operation. Agent configuration is
snapshotted when the agent is created.

## Secrets and authenticated egress

SQLite stores typed secret references, fixed upstream origins, allowed
method/path rules, delivery configuration, and principal/role grants. It never
stores resolved secret material. This is an admin request that configures an
OpenAI credential:

```json
{
  "id": "openai",
  "name": "OpenAI",
  "source": {
    "provider": "environment",
    "key": "OPENAI"
  },
  "upstream": "https://api.openai.com",
  "rules": [
    {
      "methods": ["POST"],
      "path_prefixes": ["/v1/"]
    }
  ],
  "delivery": {
    "type": "inject_header",
    "header": "authorization",
    "prefix": "Bearer "
  },
  "guest": {
    "base_url_env": "OPENAI_BASE_URL"
  }
}
```

The environment provider resolves that reference from
`NANOCENTAUR_SECRET_OPENAI` in the host process. Supplying
`--secret-directory /run/secrets/nanocentaur` also enables the `file` provider,
whose keys are safe relative paths below that directory.

Setting `OP_CONNECT_HOST` and `OP_CONNECT_TOKEN` enables the
`1password_connect` provider, following Iron Proxy's source contract:

```json
{
  "provider": "1password_connect",
  "key": "op://Engineering/OpenAI/api/credential"
}
```

The key is `op://vault/item/[section/]field`; each component may be its
human-readable name or 26-character 1Password ID. NanoCentaur authenticates to
Connect with the server-side token, resolves the field by ID or label, and does
not cache the result, so rotations apply on the next authorized request. The
Connect host, token, resolved item, and actual credential remain host-side.
References with query parameters, generated OTP attributes, and file
attachments are intentionally not supported by this field-value provider.

Setting `OP_SERVICE_ACCOUNT_TOKEN` enables the sibling `1password` provider,
also matching Iron Proxy's provider distinction. NanoCentaur hosts 1Password's
official SDK core WASM in-process through Extism; it does not execute or require
the `op` CLI:

```json
{
  "provider": "1password",
  "key": "op://Engineering/CoinGecko/credential"
}
```

The SDK core is pinned to the version and SHA-256 digest compiled into
NanoCentaur. Its WASM runtime has a 60-second call deadline, a 256 MiB memory
ceiling, no filesystem mounts, and network access only to 1Password's
`.com`, `.ca`, and `.eu` domains. References are validated before invocation
and returned values are size-bounded. Each reusable client owns its mutable
WASM runtime on a dedicated host thread; the server defaults to three clients
created from one compiled module. A coordinator dispatches through a bounded
typed channel without a shared runtime mutex. Distinct references can resolve
in parallel, while identical outstanding references are globally
single-flighted and receive the same in-flight result. The result is discarded
after those callers are answered: there is no TTL or post-request secret cache,
so a rotation applies to the next resolution.

Overload fails closed instead of accumulating unbounded waiters, each
resolution has a 65-second end-to-end deadline, and queued work with no live
caller is skipped. Startup waits for the first authenticated client and brings
the remaining pool online in the background. Shutdown closes the queue and
joins every worker before the Extism runtimes are released. The service-account
token is never placed in a process argument, VMM configuration, guest
environment, or model context.

Grant the reference directly or through a reusable role:

```text
PUT /admin/v1/principals/:principal_id/secrets/openai
PUT /admin/v1/roles/:role_id/secrets/openai
```

When a VM runtime is created, NanoCentaur computes effective grants and gives
the guest an authenticated loopback `HTTP_PROXY`/`HTTPS_PROXY`, a read-only
process-lifetime interception CA mount, and
`OPENAI_BASE_URL=<scoped lease URL>` as a compatibility fallback. The private
CA key remains only in proxy memory. For replacement delivery it additionally
injects a placeholder whose value is its own name. The transparent proxy:

1. authenticates the unguessable runtime lease on `CONNECT`;
2. permits interception only when the requested TLS authority matches a granted
   secret's fixed upstream origin;
3. terminates guest TLS with a short-lived certificate signed by the mounted CA;
4. rechecks the active agent, API client, principal, origin, method, and path,
   rejecting traversal and ambiguous percent-encoded path separators before
   prefix matching;
5. resolves the secret on the host for that request and injects/replaces the
   configured header before forwarding.

The explicit base-URL route performs the same live checks and constructs the
upstream request itself with redirects disabled.

This makes backend rotation effective on the next request and revocation
effective immediately. Secret policy revisions also evict an idle harness
before its next turn so newly granted environment is reprovisioned. Lease
records disappear when the VM runtime is dropped, and logs contain identifiers
but not resolved values.

The Nanocodex harness and model client remain trusted host-side code. Secret
provider configuration may itself live in the server process environment, so
trusted gateway code can technically resolve those values. They are never
added to the harness configuration, serialized into the VMM config, or exposed
as model context. The VMM child is launched with a cleared host environment and
an operational allowlist (`PATH` and the macOS libkrun firmware loader path).
The guest gets only its revocable proxy credential, public CA, CA paths,
placeholders, and compatibility base URLs. All arbitrary shell, filesystem,
and CLI execution remains inside NanoVM.

The transparent listener binds host loopback and is reached from the guest
through libkrun's TSI networking, so the bearer proxy URL is not exposed as a
public API port. `--secret-gateway-url` still controls the compatibility route
for SDKs that ignore standard proxy variables.

The proxy protects secret use, not general network reachability: a guest can
still make unauthenticated direct network requests, but it cannot obtain the
resolved credential through that path. Enforcing an origin allowlist for all
guest traffic would additionally require a VM network firewall or gvproxy
boundary. As with the VM isolation model generally, a libkrun escape is outside
this boundary.

The standalone server bootstraps one API client and principal from its CLI
configuration. Policy is stored at `$STATE_DIRECTORY/policy.sqlite`; durable
agent sessions are stored independently at
`$STATE_DIRECTORY/sessions.sqlite`.

## Running

```bash
cargo run -p nanocentaur-server -- serve \
  --api-key local-development-key \
  --admin-token local-admin-key \
  --backend mock

cargo run -p nanocentaur-server -- bench \
  --api-key local-development-key \
  --agents 32
```

The HTTP benchmark opens one event stream per in-flight agent. Raise the
service and load-generator file-descriptor limits before testing hundreds of
concurrent agents; otherwise the operating-system socket ceiling is the result,
not NanoCentaur capacity. On macOS:

```bash
ulimit -n 4096
```

With a local debug build and the zero-delay mock backend, 1,000 agents at
100-way concurrency completed at 1,341 agents/second (p50 73 ms, p95 93 ms).
At 500-way concurrency with the raised descriptor limit, 5,000 agents completed
at 1,003 agents/second (p50 489 ms, p95 582 ms). This benchmark exercises the
real HTTP, authentication, SQLite durability, actor, and SSE paths, but not VM
boot or a model provider.

With 1Password Connect:

```bash
OP_CONNECT_HOST=http://127.0.0.1:8080 \
OP_CONNECT_TOKEN="$CONNECT_ACCESS_TOKEN" \
cargo run -p nanocentaur-server -- serve \
  --api-key "$NANOCENTAUR_API_KEY" \
  --admin-token "$NANOCENTAUR_ADMIN_TOKEN"
```

With a 1Password service account:

```bash
ONEPASSWORD_CORE_WASM=.cache/onepassword/core-v0.4.0.wasm \
cargo run -p nanocentaur-server -- onepassword-core

OP_SERVICE_ACCOUNT_TOKEN="$OP_SERVICE_ACCOUNT_TOKEN" \
ONEPASSWORD_CORE_WASM=.cache/onepassword/core-v0.4.0.wasm \
cargo run -p nanocentaur-server -- serve \
  --api-key "$NANOCENTAUR_API_KEY" \
  --admin-token "$NANOCENTAUR_ADMIN_TOKEN" \
  --onepassword-workers 3
```

`onepassword-core` downloads from the official tagged 1Password SDK repository
with redirects disabled, enforces a 16 MiB limit, verifies the compiled-in
digest, and atomically creates the destination. An existing untrusted file is
never overwritten.

The SDK core is large enough that cold Wasmtime compilation is noticeable.
NanoCentaur automatically keeps native compilation artifacts in
`$STATE_DIRECTORY/onepassword-wasmtime-cache`; `smoke-egress` and
`bench-one-password` default to a `wasmtime-cache` directory beside the pinned
core. The directory is canonicalized and rejected when it is group- or
world-writable. It must not be writable by the guest or another tenant.
Authentication still runs on every process start; the cache only removes
repeated compilation.

Use the provider-only benchmark to separate SDK behavior from HTTP, policy,
proxy, and VM costs. Repeat `--secret-reference` to model unrelated credentials:

```bash
nanocentaur bench-one-password \
  --onepassword-core-wasm .cache/onepassword/core-v0.4.0.wasm \
  --secret-reference "op://$OP_VAULT/COINGECKO_API_KEY/credential" \
  --secret-reference "op://$OP_VAULT/OPENAI_API_KEY/credential" \
  --secret-reference "op://$OP_VAULT/GITHUB_TOKEN/credential" \
  --workers 3 \
  --requests 12 \
  --concurrency 12
```

On the development machine with a warm debug-build cache, provider readiness
was 3.09 seconds. Twelve concurrent logical resolutions spread across three
references fell from 11.14 seconds in the serialized implementation to
1.92 seconds with global coalescing and three workers (5.8x faster). Fifty
concurrent callers for one reference completed through one physical lookup in
1.39 seconds (36.1 logical resolutions/second). These are local provider
measurements, not production capacity claims; network latency and 1Password
service behavior remain external variables.

### VM CLI egress smoke test

`smoke-egress` runs a real command in an isolated copy of a directory rootfs.
It creates an ephemeral SQLite policy, principal, managed-agent identity,
secret grant, proxy lease, and CA. Egress files are copied into the disposable
rootfs, while the 1Password reference and service-account token stay in the
host process.

The rootfs should be built ahead of time with the required CLIs. For the
Centaur CoinGecko smoke used during development, the Alpine ARM64 image
contains Python, `centaur-sdk`, and `tools/crypto/coingecko`, with the SDK
source package copied into `site-packages`. The source copy is currently
necessary because Centaur's `centaur-sdk` wheel metadata installs without an
importable `centaur_sdk` package.

```bash
source /path/to/centaur/.env

nanocentaur smoke-egress \
  --rootfs .cache/rootfs/centaur-coingecko \
  --secret-reference "op://$OP_VAULT/COINGECKO_API_KEY/credential" \
  --upstream https://pro-api.coingecko.com \
  --header x-cg-pro-api-key \
  --placeholder COINGECKO_API_KEY \
  --path-prefix /api/v3/ \
  --onepassword-core-wasm .cache/onepassword/core-v0.4.0.wasm \
  coingecko health
```

The CLI sees `COINGECKO_API_KEY=COINGECKO_API_KEY`. Its HTTPS request uses the
ephemeral CA and authenticated proxy; the host resolves 1Password only after
the agent, principal, origin, `GET` method, and `/api/v3/` path checks pass.
The service-account token and resolved CoinGecko key are absent from the guest.

Real harness:

```bash
cargo run -p nanocentaur-server -- serve \
  --api-key "$NANOCENTAUR_API_KEY" \
  --admin-token "$NANOCENTAUR_ADMIN_TOKEN" \
  --backend nanocodex \
  --rootfs ./rootfs.ext4 \
  --openai-api-key "$OPENAI_API_KEY" \
  --allow-capability github.read
```

MPP sessions are available behind the server's `mpp` feature. Authorization
and policy checks happen before payment verification so unauthorized requests
are not charged.
