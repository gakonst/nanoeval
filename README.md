<div align="center">

<h1>Nanoeval</h1>

<p><strong>Blazing-fast, Docker-free, library-first evaluations for coding agents.</strong></p>

[![CI](https://github.com/gakonst/nanoeval/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/gakonst/nanoeval/actions/workflows/ci.yml?query=branch%3Amaster)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)][license]

**[Thesis](#why-nanoeval)** | **[Quick start](#quick-start)** | **[API](#api)** | **[Harbor](#harbor-compatible-output)** | **[Performance](#how-fast)** | **[Architecture](#architecture)**

[license]: LICENSE-MIT

</div>

---

Nanoeval is a small Rust evaluation SDK for coding agents. It embeds
[Nanocodex](https://github.com/gakonst/nanocodex) directly, creates a fresh
agent session and disposable workspace for every attempt, runs the canonical
task verifier, and publishes the complete typed event stream and result.

The current native path invokes no Docker daemon, builds no task image, and
installs no agent into a container. A complete passing attempt can be opened
directly with `harbor view`.

## Why Nanoeval?

Most coding-agent evaluation time should be spent on the model and the work it
requested. In practice, a large fraction can disappear into image builds,
container startup, agent installation, subprocess adapters, log copying, and
cleanup.

Nanoeval starts from a narrower thesis: **an eval is one typed agent recipe run
against one immutable task in one fresh execution environment**. Everything
else should be paid once, kept off the hot path, or derived after the result is
durable.

That leads to a few deliberate choices:

- Nanocodex is a Rust library in the trusted harness, not a binary installed
  into every task image.
- Every attempt receives a fresh Nanocodex session, tool runtime, workspace,
  event sequence, and verifier run.
- Attempts are assigned to bounded three-attempt prompt-cache cohorts. Each
  cohort singleflights one Nanocodex warmup per exact prefix, so later attempts
  skip the redundant request without creating one hot cache key for the whole
  job. They do not share conversation history, response chains, sockets, tool
  state, or workspace.
- Typed results and typed events are the runtime contract. The optional Harbor
  adapter records one subscription as Harbor and ATIF artifacts.
- Native execution provides the fastest development loop for host-safe tasks.
- Linux compatibility and isolation belong in a reusable, Docker-free worker
  built on libkrun, not in per-attempt image builds.

Nanoeval is intentionally not a provider-neutral agent framework, container
orchestrator, or generic benchmark server. It is the smallest useful eval
runtime for Nanocodex and Terminal-Bench-style tasks.

## Quick start

Nanoeval currently builds from source and uses an adjacent Nanocodex checkout
declared in the workspace.

```sh
mkdir nanoeval-dev && cd nanoeval-dev
git clone https://github.com/gakonst/nanocodex
git clone https://github.com/gakonst/nanoeval
cd nanoeval

export OPENAI_API_KEY=...
cargo run -- run --task tasks/write-greeting --thinking low
```

The CLI prints the retained Harbor job directory. Run several independent
attempts concurrently with:

```sh
cargo run -- run \
  --task tasks/write-greeting \
  --trials 5 \
  --concurrency 5 \
  --thinking low
```

Run the same evaluator with workspace tools inside independent libkrun VMs:

```sh
cargo run -- run \
  --task tasks/write-greeting \
  --trials 5 \
  --concurrency 5 \
  --thinking low \
  --vm \
  --vm-rootfs .cache/rootfs/alpine-3.24.1
```

Without `--vm-rootfs`, `--vm` prepares each distinct task's Linux/ARM64 root
disk from OCI and caches it under `.cache/vm`. A multi-task job indexes those
typed environments by canonical task root, so every attempt receives the right
root disk, workdir, image environment, and detected shell. The typed builder
accepts every Dockerfile
instruction shape present in the 89 Terminal-Bench 2.1 tasks: `FROM` (including
named multi-stage builds), `RUN`, `COPY` and `COPY --from`, absolute `WORKDIR`,
`ENV`, `ARG`, and `CMD`. `CMD` is parsed but deliberately not started, matching
Harbor's long-lived task-container override. Unknown instructions are rejected
instead of approximated.
`--vm-rootfs <path>` overrides preparation with either a raw ext4 image or the
earlier trusted virtiofs directory for VM development.

The local cache has three independent identities. A typed reference record maps
the exact OCI reference to its resolved manifest and layer descriptors, so an
unchanged local rerun does not contact the registry; `--vm-refresh` explicitly
resolves the image again. The flattened ext4 template is keyed only by the
resolved manifest, platform, converter version, and disk size. A complete build
is keyed by the Dockerfile, deterministic context archive, every resolved stage
manifest, platform, builder version, and disk size. Build contexts and prior
stages are immutable block devices; `RUN` and `COPY` mutate only a temporary CoW
stage disk, and the final disk is published atomically. Finally, the Nanocodex VM
tool runtime has its own content identity. Each attempt reflinks the immutable
ext4 template and mounts the current runtime as a read-only block disk.
Consequently, ordinary harness or tool changes require only a Rust rebuild and
VM restart, not an OCI pull or task-image rebuild.

The writable attempt disk is retained when an agent is refused, the harness
errors, or verification fails, so `nanoeval inspect` can point directly at the
guest filesystem that needs diagnosis. Passed attempts remove that large CoW
disk after same-guest verification and retain the Harbor result, ATIF
trajectory, exact event stream, verifier output, CTRF, answer, stderr, and
network log. This keeps full-suite runs bounded without weakening failure
evidence. Pass `--vm-retention all` when a successful guest disk is itself the
artifact under investigation.

In a source checkout, `nanoeval run --vm` incrementally builds the static
Linux/aarch64 guest runtime with Cargo, stages only that content-addressed
binary under `.cache/vm/runtimes`, and exposes no source tree or general target
directory to the guest. A typed build record validates the source binary's
size and modification identity before reusing its digest, avoiding a repeated
debug-mode SHA-256 pass. On macOS the Cargo runner similarly executes a
content-addressed, entitled copy of the host binary. Cargo may replace its
ordinary output with an unsigned linker artifact without invalidating the
signed copy or causing a broken VMM child.

Each attempt also receives its own gvproxy-backed virtio-net interface. Nanoeval
downloads the pinned, checksummed gvproxy release once into `.cache/vm` (or uses
`NANOEVAL_GVPROXY`/`PATH`) and owns that process for exactly the guest lifetime.
Guest loopback and listening ports are therefore private per attempt; concurrent
tasks may both bind `localhost:8080` without reaching each other or a host
service.

Prepare one or more Terminal-Bench 2.1 environments without running agents:

```sh
cargo run -- vm prepare \
  --task /path/to/terminal-bench-2-1/count-dataset-tokens \
  --task /path/to/terminal-bench-2-1/regex-log

# Prepare every immediate task in a suite.
cargo run -- vm prepare --suite /path/to/terminal-bench-2-1
```

Preparation writes one root-disk path to stdout per task in input order and
reports task identity, manifest source, cache status, and duration on stderr.
The final preparation line separates guest-runtime time from the number of
environment hits and creations and the total in-process duration. The guest
runtime is prepared once for the whole command. Pass `--refresh` to the
standalone command, or `--vm-refresh` to `nanoeval run`, when intentionally
checking whether a mutable image reference now resolves to different content.

The same repeated-task shape works for one concurrent VM-backed eval job:

```sh
cargo run -- run \
  --task /path/to/task-a \
  --task /path/to/task-b \
  --task /path/to/task-c \
  --trials 5 \
  --concurrency 15 \
  --thinking low \
  --vm
```

Run a complete suite without expanding 89 `--task` flags:

```sh
cargo run -- run \
  --suite /path/to/terminal-bench-2-1 \
  --trials 1 \
  --concurrency 8 \
  --max-memory-mb 24576 \
  --thinking low \
  --vm
```

`--suite` selects immediate child directories containing `task.toml` in stable
name order. It may be combined with explicit, repeated `--task` inputs.
Execution is prioritized by the median duration of matching retained trials,
falling back to declared task timeouts when no trustworthy history exists.
Results and trial numbers remain deterministic; only dispatch order changes.
`--max-memory-mb` is an optional job-wide admission ceiling over each task's
declared guest memory. Slot and memory permits are acquired atomically; a
smaller task may proceed when an earlier large task does not fit, and a task
larger than the ceiling runs alone rather than deadlocking. Omitting the option
retains the simple concurrency-only policy.

The default CLI lifecycle is interruption-safe. Before starting a new job,
Nanoeval reopens the newest incomplete job whose typed `run.json` exactly
matches the requested task roots, agent ID, and trial count. Every trial with a
durable `result.json` is skipped, while an attempt that was active during
Ctrl+C is restarted with a fresh Nanocodex session and CoW disk:

```sh
# The first invocation may be interrupted.
cargo run -- run \
  --suite /path/to/terminal-bench-2-1 \
  --trials 1 \
  --concurrency 6 \
  --thinking low \
  --vm

# The identical command resumes only missing trials in the same Harbor job.
cargo run -- run \
  --suite /path/to/terminal-bench-2-1 \
  --trials 1 \
  --concurrency 6 \
  --thinking low \
  --vm
```

An OS advisory lock prevents two processes from mutating the same job.
`--new` deliberately starts a separate job even when a matching incomplete
one exists. Resume is attempt-level, not conversation-level: Nanoeval never
reuses partial model history, a half-mutated guest, or an event sequence after
interruption.

Completed failures have a separate Foundry-style retry loop:

```sh
# Retry every reward-zero task from the last completed job.
cargo run -- run --rerun

# Inspect the retry queue without running agents.
cargo run -- run --rerun --list

# Literal positional substrings are the short, Foundry-style path.
cargo run -- run --rerun webserver
cargo run -- run --rerun torch dna

# Use the explicit flag only for regular expressions.
cargo run -- run --rerun \
  --match-task 'nginx|git' \
  --match-task '^terminal-bench/torch-'

# Select a specific retained job instead of the last one.
cargo run -- run --rerun \
  --rerun-from nanoeval-runs/<job-id> \
  configure-git-webserver
```

Positional names are escaped literal substrings and repeated values are ORed;
`webserver` therefore selects `terminal-bench/configure-git-webserver` without
requiring regex syntax. `--match-task` is the advanced regular-expression form
over the full canonical name. By default, rerun selects only scored failures;
`--include-refused` adds typed safety refusals and `--include-errored` adds
harness errors. `--list` prints short readable task names and exits before
loading credentials or preparing a VM; add `--json` for canonical names as a
typed JSON array. A retry prints the selected names before starting one fresh
trial per task in a new Harbor job. Pass `--trials 5` to retry each selected
task five times.

Nanoeval retains the source job's non-secret execution policy—thinking level,
concurrency, memory ceiling, VM mode/rootfs, and VM retention—while explicit CLI
flags override it. Credentials are never copied into retained state. Each job's
typed `invocation.json` makes this inheritance deterministic, and
`.nanoeval/last-run.json` points to the latest completed local job. An
interrupted retry uses the ordinary durable-resume path when the same command is
repeated; the completed source job is never mutated. Filtered retries form a
typed lineage: each child result replaces that task's prior status, while
unselected failures remain in the effective queue. A passing retry therefore
removes one task without hiding its unresolved siblings.

Every `--task` is one eval and `--trials` applies to each one. Run the complete
three-eval, `k=5` suite with:

```sh
cargo run -- run \
  --task tasks/write-greeting \
  --task tasks/uppercase-message \
  --task tasks/extract-todos \
  --trials 5 \
  --concurrency 15 \
  --thinking low
```

Nanoeval also reuses the ChatGPT authorization file managed by Codex and
Nanocodex. Without `OPENAI_API_KEY`, it loads `${CODEX_HOME}/auth.json` or
`~/.codex/auth.json`; `--auth-file` selects one explicitly.

## Tracing and measurement

Nanoeval emits the same application-owned `tracing` spans as Nanocodex. The CLI
installs Nanocodex's observability stack, writes diagnostics to stderr by
default, and can append structured JSON locally or export OTLP/HTTP traces:

```sh
cargo run -- run \
  --task tasks/write-greeting \
  --trials 5 \
  --concurrency 5 \
  --thinking low \
  --vm \
  --log-format json \
  --log-file .cache/nanoeval-traces.jsonl
```

Pass `--otel-endpoint http://127.0.0.1:4318` to export the same span stream to
an OpenTelemetry collector. `--log-filter` and `--otel-filter` independently
control local and exported detail.

Each `eval.attempt` is an independent root so concurrent trials appear as
overlapping traces rather than one artificial serial job. Its important
children are:

| Span | Measurement |
| --- | --- |
| `eval.environment.setup` | Disposable task workspace preparation |
| `eval.agent.setup` | Fresh Nanocodex build, attempt hook, rootfs materialization, and VMM child spawn |
| `eval.agent.execution` | Complete agent turn; existing Nanocodex model and tool spans inherit this parent |
| `eval.verifier` | Canonical verifier runtime, exit code, reward, and output sizes |
| `vm.rootfs.materialize` | Current trusted-rootfs copy cost |
| `vm.session.spawn` | Host VMM child-process spawn cost |
| `vm.tool.rpc` | Console queue and round-trip latency, request/response sizes, session age, and first-call status |

Every bounded span records `duration_ns`, `status`, and OpenTelemetry status.
The attempt root also records aggregate tokens, cache hits and writes, warmup
usage and duration, response attempts and retries, model/tool calls, reward,
and cost. The first `vm.tool.rpc` includes guest boot/readiness time; later calls
show warm transport and guest-tool latency. Full task prompts, verifier output,
VM commands, and typed VM requests/responses are emitted as ordered span events,
matching Nanocodex's full-fidelity tracing policy. Treat trace output as a copy
of the evaluated conversation and tool activity.

Every successful CLI invocation ends with one `evaluation run completed`
record. It separates observability installation, task loading, guest-runtime
lookup/build, task-environment preparation, evaluator setup, attempt wall time,
Harbor finalization, output, and total in-process wall time. The same record
aggregates model, warmup, guest-tool work/wall, and verifier time, response
retries, and input/cache tokens across completed attempts. These aggregates are
work totals, while `attempts_wall_duration_ns` is elapsed wall time and therefore
remains meaningful under concurrency.

Cargo compilation, linking, entitlement signing, and runner startup happen
outside the process and are intentionally measured separately with
`/usr/bin/time cargo run ...`. The development profile retains line-number
backtraces without full variable debug info. Host-only source changes rotate the
signed host executable but do not invalidate the Linux guest runtime, OCI
layers, task root disks, or verifier dependency layers.

Tracing remains diagnostic and application-owned. Library consumers may use
`nanocodex-observability` or install any compatible `tracing` subscriber;
Nanoeval's typed events and results remain the contractual API.

## API

`Nanoeval` is a reusable evaluation recipe. It owns a cloneable
`NanocodexBuilder`, not a live conversation. Each call to `task` builds an
independent agent and attempt. Core Nanoeval does not select an output format:

```rust,no_run
use nanocodex::Nanocodex;
use nanoeval::{EvalEventKind, EvalEventStreamError, Nanoeval, Task};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let task = Task::load("tasks/write-greeting")?;

let (eval, events) = Nanoeval::builder(Nanocodex::builder("api-key"))
    .output_directory("nanoeval-native-runs")
    .max_concurrency(5)
    .max_memory_mb(24_576)
    .build()?;

let mut observer = events.subscribe();
let observer = tokio::spawn(async move {
    while let Some(event) = observer.recv().await? {
        if matches!(&event.kind, EvalEventKind::Completed(_)) {
            break;
        }
    }
    Ok::<_, EvalEventStreamError>(())
});

let first = eval.task(task.clone()).await?;
let five_fresh_attempts = eval.task_n(task, 5).await?;
let one_each = eval.tasks(vec![task.clone(), task.clone()]).await?;
let five_each = eval.tasks_n(vec![task.clone(), task], 5).await?;
assert!(first.artifacts.workspace.is_dir());
observer.await??;
# drop(five_fresh_attempts);
# drop(one_each);
# drop(five_each);
# Ok(())
# }
```

The runnable core-only consumer is
[`examples/src/bin/native_task.rs`](examples/src/bin/native_task.rs):

```sh
OPENAI_API_KEY=... cargo run -p nanoeval-examples --bin native-task
```

The default multi-task API deliberately takes a `Vec<Task>`: `tasks` runs each
task once, while `tasks_n` runs each task `k` times. Callers do not construct a
plan, scheduler slot, or run manifest.

### Advanced sweeps

`Sweep` is the opt-in API for comparing several independently configured agent
recipes across tasks and trials:

```rust,no_run
# use nanocodex::{Nanocodex, Thinking};
# use nanoeval::{Nanoeval, Sweep, Task};
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let task = Task::load("tasks/write-greeting")?;
let nanocodex = Nanocodex::builder("api-key");
let sweep = Sweep::builder()
    .tasks(vec![task])
    .trials(5)
    .agent("thinking-low", nanocodex.clone().thinking(Thinking::Low))?
    .agent("thinking-high", nanocodex.clone().thinking(Thinking::High))?
    .build()?;

assert_eq!(sweep.attempt_count(), 10);
let (eval, events) = Nanoeval::builder(nanocodex)
    .output_directory("nanoeval-runs")
    .resume_incomplete(&sweep)
    .max_concurrency(10)
    .build()?;
let remaining = eval.remaining_attempts(&sweep)?;
let results = eval.sweep(sweep).await?;
assert_eq!(results.attempts().len(), remaining);
assert_eq!(results.attempts().len() + results.skipped(), 10);
if let Some(first) = results.attempts().first() {
    assert_eq!(first.agent().as_str(), "thinking-low");
}
# drop(events);
# Ok(())
# }
```

The sweep expands in task, agent, then one-based trial order. Every
`SweepAttemptResult` exposes its agent ID, trial number, and ordinary
`EvalResult`; sessions, workspaces, tool runtimes, and event sequences remain
independent. Nanoeval privately retains `run.json` with canonical task roots,
trial count, and stable agent IDs, but never credentials or opaque builder
state. See the compiled
[`thinking-sweep`](examples/src/bin/thinking_sweep.rs) and
[`tool-sweep`](examples/src/bin/tool_sweep.rs) consumers for executable `k=5`
thinking and default-tools-versus-MCP examples.

### Harbor adapter

Harbor is a separate streaming adapter. Give it one subscription before
starting any tasks. Application telemetry, a UI, or another exporter can drain
another subscription concurrently without competing with Harbor:

```rust,no_run
# use nanocodex::Nanocodex;
# use nanoeval::{EvalEventKind, EvalEventStreamError, Nanoeval, Task};
# use nanoeval_harbor::Harbor;
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let task = Task::load("tasks/write-greeting")?;
let (eval, events) = Nanoeval::builder(Nanocodex::builder("api-key"))
    .output_directory("nanoeval-runs")
    .build()?;

// Both subscriptions exist before the first attempt emits anything.
let harbor = Harbor::new(&eval)?.record(events.subscribe())?;
let mut application_events = events.subscribe();
let application = tokio::spawn(async move {
    let mut observed = 0_u64;
    while let Some(event) = application_events.recv().await? {
        observed += 1;
        if matches!(&event.kind, EvalEventKind::Completed(_)) {
            break;
        }
    }
    Ok::<_, EvalEventStreamError>(observed)
});

// Nanoeval executes the task and returns its native typed result. Meanwhile,
// Harbor writes exact events and builds the ATIF trajectory incrementally.
let result = eval.task(task).await?;

// Finish waits until Harbor has processed this result's Completed event, then
// commits the final job metadata. Pass every result belonging to this run.
let job = harbor.finish(vec![result]).await?;
let observed = application.await??;

assert!(observed > 0);
println!("Harbor job: {}", job.directory().display());
# Ok(())
# }
```

`output_directory("nanoeval-runs")` selects the jobs parent. `build()` creates one
UUID-named evaluation job beneath it, and `Harbor::new(&eval)` explicitly
attaches Harbor output to that evaluator. `record(...)` starts draining immediately;
`finish(...)` is finalization, not deferred event conversion.

For a batch that must retain errored attempts as well as scored results, finalize
by the expected terminal count:

```rust,no_run
# use nanoeval_harbor::HarborRecorder;
# async fn finish(harbor: HarborRecorder) -> Result<(), Box<dyn std::error::Error>> {
let job = harbor.finish_all(15).await?;
println!("Harbor job: {}", job.directory().display());
# Ok(())
# }
```

Every accepted attempt emits exactly one `Completed` or `Failed` terminal event.
`finish_all(...)` waits for all of them, so one refusal does not cancel or hide
unrelated trials. A refusal is retained with a partial ATIF trajectory, a null
reward, and Harbor `exception_info` such as `AgentSafetyRefusalError`; scored
failures remain ordinary verifier results with reward `0.0`.

See [`examples/src/bin/harbor_task.rs`](examples/src/bin/harbor_task.rs):

```sh
OPENAI_API_KEY=... cargo run -p nanoeval-examples --bin harbor-task
```

`task`, `task_n`, and `tasks` are the complete scheduling surface. Concurrency
is ordinary Rust async concurrency bounded by the evaluator policy; it does not
change attempt ownership or introduce a second session abstraction.

### Parallel suites

Run independent evals concurrently with ordinary Tokio composition. This
launches three tasks at `k=5`; all 15 attempts may run concurrently because the
evaluator's global limit is also 15:

```rust,no_run
# use nanocodex::{Nanocodex, Thinking};
# use nanoeval::{Nanoeval, Task};
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
const K: usize = 5;
let greeting = Task::load("tasks/write-greeting")?;
let uppercase = Task::load("tasks/uppercase-message")?;
let todos = Task::load("tasks/extract-todos")?;
let agent = Nanocodex::builder("api-key").thinking(Thinking::Low);
let (eval, events) = Nanoeval::builder(agent)
    .max_concurrency(3 * K)
    .build()?;
let harbor = nanoeval_harbor::Harbor::new(&eval)?
    .record(events.subscribe())?;

let (greeting, uppercase, todos) = tokio::try_join!(
    eval.task_n(greeting, K),
    eval.task_n(uppercase, K),
    eval.task_n(todos, K),
)?;

assert_eq!(greeting.len() + uppercase.len() + todos.len(), 3 * K);
# let results = greeting.into_iter().chain(uppercase).chain(todos).collect();
# harbor.finish(results).await?;
# Ok(())
# }
```

`max_concurrency` is the one global bound for that `Nanoeval`. Set it to `K`
to keep at most five attempts in flight across the whole suite, or to
`number_of_tasks * K` to allow every trial to run at once. The complete
executable is [`examples/src/bin/native_suite.rs`](examples/src/bin/native_suite.rs).

```sh
OPENAI_API_KEY=... cargo run -p nanoeval-examples --bin native-suite
```

On an Apple M1 Max with a warm debug binary, `gpt-5.6-sol` at low effort ran
this exact 15-attempt suite in **22.45 seconds**. All attempts began within
41 ms, all 15 verifiers passed, and the slowest individual agent took 22.09
seconds. The roughly 0.36-second difference is the observed scheduling,
workspace, verification, ATIF, and Harbor-retention overhead beyond the
critical agent path. Build time is deliberately excluded from this warm eval
measurement.

### Lifecycle and dataflow

```text
NanocodexBuilder
       │ cloned per attempt
       ▼
   Nanoeval ─────────────────────────► NanoevalEvents
       │                                  │ independent subscriptions
       │ task(task)
       ▼                                  ├──► application observer
 fresh Nanocodex session ────────────► AgentEvents
       │                                  │
       │ standard tools in a fresh workspace
       ▼                                  └──► nanoeval-harbor recorder
 canonical verifier                           ├──► exact events.jsonl
       │                                      ├──► ATIF-v1.7 trajectory
       └──► typed EvalResult                  └──► Harbor job/trial records
```

The evaluator can be reused indefinitely. Attempts never reuse conversation
history, tool sessions, mutable workspace state, or event sequence numbers.
Within one attempt, Nanoeval publishes the complete agent loop in sequence.
The Harbor adapter incrementally projects it into ATIF: the initial user prompt
is one step and every Nanocodex model inference is a separate agent step
containing that turn's message, reasoning, usage, tool calls, and matching
observations.

### VM-owned standard tools

`nanocodex-vm` replaces only the standard tools whose effects must occur in the
guest. Their names and model-visible definitions come directly from
`nanocodex-tools`; the VM crate does not maintain copies of their JSON schemas
or the `apply_patch` grammar. Code Mode remains the host-side dispatcher and
`update_plan` remains a normal host tool:

```rust,no_run
# use nanocodex::{Tools, ToolsBuildError, UpdatePlanTool};
# use nanocodex_vm::{VmToolClient, VmTools};
# fn tools(client: impl VmToolClient + 'static) -> Result<Tools, ToolsBuildError> {
let vm = VmTools::new(client);
Tools::builder()
    .without_defaults()
    .working_directory("/workspace")
    .default_shell("sh")
    .tool(vm.exec_command_tool())
    .tool(vm.write_stdin_tool())
    .tool(vm.apply_patch_tool())
    .tool(vm.view_image_tool())
    .tool(UpdatePlanTool::new())
    .build()
# }
```

`VmToolSession` is the concrete retained client. It speaks newline-framed,
strictly typed requests over libkrun's piped virtio console to a statically
linked Linux guest runtime. The guest constructs the canonical
`nanocodex-tools::ToolRuntime`, so shell session IDs, subprocesses, patches,
and image reads are guest-owned; complete `ToolExecution` values cross back to
the host without flattening known fields into `serde_json::Value`.

Nanoeval exposes the per-attempt builder boundary needed to bind that session:

```rust,ignore
let (eval, events) = Nanoeval::builder(agent)
    .attempt_agent(move |attempt, builder| {
        // Materialize a rootfs at attempt.directory(), then start its VMM.
        let vm = VmTools::new(VmToolSession::spawn(&mut vmm_command(attempt))?);
        let tools = Tools::builder()
            .without_defaults()
            .working_directory("/workspace")
            .default_shell("sh")
            .tool(vm.exec_command_tool())
            .tool(vm.write_stdin_tool())
            .tool(vm.apply_patch_tool())
            .tool(vm.view_image_tool())
            .tool(UpdatePlanTool::new())
            .build()?;
        Ok(nanoeval::AttemptAgent::new(builder.tools(tools))
            .verifier(vm_verifier_for_the_retained_session(attempt)?))
    })
    .max_concurrency(15)
    .build()?;
```

The working-directory and shell overrides are model context supplied by the
general Nanocodex tools recipe; they do not special-case VM argument rewriting.
Code Mode and `update_plan` stay on the host. The complete runnable rootfs and
VMM composition is in [`bin/nanoeval/src/eval.rs`](bin/nanoeval/src/eval.rs),
while [`vm-tools`](examples/src/bin/vm_tools.rs) directly exercises all four
tools through one real VM without a model call.

On the July 21, 2026 Apple Silicon development host, the VM-backed three-task
`k=5`, concurrency-15 run completed all 15 Harbor-recorded trials in **21.10
seconds**. All 15 verifiers passed. The trajectories contain 15 guest
`apply_patch` calls and 29 guest `exec_command` calls, and all 29 explicit
working directories are `/workspace`. Rootfs materialization plus agent setup
and VMM-child spawn averaged 63.9 ms per attempt and reached 95.2 ms at the
maximum.

This is a correctness and throughput proof, not the final sandbox: it copies a
trusted 15 MiB Alpine directory into every retained attempt and exposes that
directory through writable virtiofs. The 15-trial job therefore occupies 232
MiB. Immutable rootfs snapshots, a private CoW workspace, guest confinement,
and a warm multi-attempt VM pool remain the next isolation/storage slice.

The newer canonical proof uses a raw sparse ext4 block device instead of
virtiofs. `count-dataset-tokens` was rebuilt from the Linux/ARM64
`python:3.13-slim-bookworm` OCI layers and executed in one APFS CoW clone. The
unmodified verifier passed with
answer `79586` and reward `1`; the retained output includes the mutated ext4
disk, `answer.txt`, `ctrf.json`, ATIF-v1.7 trajectory, and Harbor-shaped result.
This was the initial one-task/one-trial gate; the same path now executes the
complete 89-task suite.

On the July 22, 2026 Apple Silicon development host, a clean cache took 8.13
seconds to resolve, download, flatten, and create the 10 GiB sparse ext4 image;
retained OCI blobs reduced a fresh flatten to 4.12 seconds. The unchanged local
image path now takes 0.03--0.04 seconds in the built binary (1.08 seconds through
warm `cargo run`). Fresh VM boot plus the
first typed command takes 0.16--0.22 seconds. The latest complete warm trial
took 83.55 seconds at the CLI and retained 82.12 seconds: 76.61 seconds of agent
execution and 5.49 seconds of unmodified verification. Environment setup was
0.38 ms, agent/VMM setup was 1.88 ms, and the CoW clone plus VMM child spawn was
1.61 ms. Model calls accounted for 54.27 seconds and guest tool work for 21.53
seconds; the verifier spent nearly all of its time installing curl, uv, and
pytest before its 0.01-second test. The retained Harbor result has reward `1`,
answer `79586`, 11 ATIF steps, 175,602 input tokens, and 133,120 cached input
tokens. These numbers make dependency acquisition and model behavior the next
material targets; VM RPC and attempt construction are already noise.

Verifier setup is shared benchmark infrastructure rather than a task-specific
corner case. All 89 Terminal-Bench 2.1 verifier scripts install dependencies at
runtime; 82 run `apt-get update`, 82 use the same pinned uv 0.9.5 installer and
`uvx` pattern, and 66 install exactly `curl`. Running the unchanged
`count-dataset-tokens` verifier twice in one disposable guest measured 6.445
seconds cold and 2.784 seconds warm. The cache must remain a post-agent layer:
baking it into the task image would expose curl, uv, pytest, package metadata,
and different command behavior to the agent before Harbor would.

The verifier runs in the same live guest as the agent. Nanoeval withholds
`/tests` during agent work, injects the canonical verifier directory only after
the agent turn completes, executes it through the trusted typed console, reads
reward/CTRF/answer artifacts, then synchronizes and shuts down the guest. This
is required for service tasks: SSH, Nginx, PyPI, nested QEMU, VNC, monitor
sockets, and other agent-started processes remain alive for verification.

Verifier dependencies remain a separate content-addressed ext4 cache. The
dependency key includes the base ext4 identity, architecture, complete verifier
script, disk geometry, and cache builder version. A private CoW clone is
attached at initial boot but mounted only after the agent phase. Unknown
verifier shapes execute cold. A miss runs the byte-for-byte canonical script
and atomically publishes the cache only after the pinned uv installation
exists. For the exact pinned Terminal-Bench `apt curl -> uv installer -> uv
activation` bootstrap, a hit omits the whole recognized bootstrap and activates
the cached installation directly; this avoids rerunning a network-bound
`apt-get update` whose only purpose was installing the already-unneeded curl.
The matcher rejects any additional setup command rather than silently omitting
task state. The task-owned `uvx`, test, and reward logic remain unchanged.
Concurrent trials never share writable package or rootfs state.

`regex-log` supplied the second canonical task proof and a different base image,
`ubuntu:24.04`. Direct OCI images leave `/etc/resolv.conf` for the container
runtime to inject, so Nanoeval points each guest at its private gvproxy DNS
gateway before starting its runtime. The untouched task passed twice
with reward `1`, one passing CTRF test, six ATIF-v1.7 steps, and four tool-bearing
steps. The original directory-cache hit took 7.695 seconds while reinstalling
curl and uv. With the final task-sized cache geometry, the private block-cache
hit took 0.960 seconds, an 87.5% verifier reduction, produced reward `1`, and
retained the passing CTRF result. End-to-end wall time was 43.72 seconds, of
which 39.07 seconds was agent execution; VM and agent setup took 63.3 ms. The
VMM command contained one private
`--writable-disk` and no `--writable-share`.

The first latest-master hill-climb baseline pinned Nanocodex at
`621300f0db6d485a62f0a81344f7ae879a1964e0` and ran `regex-log` at `k=5`,
concurrency 5. All five independent VM attempts passed in 60.39 seconds wall
time with no retries or errors. Agent turns ranged from 31.07 to 57.83 seconds;
guest tool work was only 15.5--23.9 ms. All five verifier caches hit, verifier
time was 0.543--0.616 seconds for four trials with one 2.286-second contention
outlier, and every retained reward/CTRF pair passed. The trajectories show the
next climb belongs in agent behavior: every rollout spent at least one extra
model/tool round trip probing an unavailable Python runtime, while VM RPC and
verification are no longer material.
The same run showed that retaining five disposable cache clones consumed about
1.0 GiB, so successful attempts now discard those clones after publishing or
reusing the authoritative cache. The live follow-up retained 218 MiB (the
109-MiB mutated agent root and 109-MiB verifier root), no `cache.ext4`, reward
`1`, and the passing CTRF result; warm verification remained 0.963 seconds.

The instrumented repeat path makes cache behavior explicit. Three warm task
environment hits took 23 ms inside the built binary (0.58 ms runtime lookup and
21 ms of environment validation), 0.03 seconds end to end via that binary, and
1.42 seconds through unchanged `cargo run`. After a host-only evaluator edit, a
real VM-backed `write-greeting` run still reported both the guest runtime and
root disk as hits: runtime preparation took 3.45 ms, environment preparation
18.69 ms, evaluator setup 4.74 ms, and Harbor finalization 4.17 ms. The
29.84-second in-process total was dominated by 28.60 seconds of model work;
guest tool work was 9.69 ms and fresh-guest verification 285.86 ms. The retained
Harbor job passed with reward `1`, 4 ATIF-v1.7 steps, and 23,808 of 25,714 agent
input tokens served from cache.

The complete warm milestone ran all 89 Terminal-Bench 2.1 tasks in one durable
VM-backed job at concurrency six on the same Apple Silicon host. It completed
in **3,523.21 seconds (58m43s)**, compared with 3,988 seconds (66m28s) for the
earlier alphabetical Nanoeval run. Duration-aware ordering therefore removed
about 7m45s, or 11.7%, without changing task inputs or concurrency. The result
contained 62 passes, 22 normal verifier-scored failures, four typed API safety
refusals, and one agent timeout. There were no environment, verifier,
VM-session, or Nanoeval errors.

The run reused 29,675,008 of 32,741,686 input tokens (90.63%). Aggregate work
was 11,409 agent-model seconds, 6,294 guest-tool seconds, and 2,086 canonical
verifier seconds; those overlap across six attempts. Task loading, cached guest
runtime preparation, cached environment preparation, evaluator setup, Harbor
finalization, and output together took about 2.2 seconds outside the attempt
wall. This is the important developer-loop result: changing the Rust harness
does not rebuild task images, and Docker is absent from the warm critical path.

The service regressions all pass in the full concurrent run:
`pypi-server`, `git-multibranch`, `install-windows-3.11`, and
`nginx-request-logging`. Each agent and verifier shared one live guest, while
each attempt had private loopback, processes, and CoW disks. Harbor 0.20.0's own
models accepted the job plus all 89 trial configs, results, and ATIF-v1.7
trajectories. `harbor view` served the aggregate, a passing service trial, and
its trajectory through the normal viewer APIs.

The 89-task measurement used Nanocodex `508c0254`. After advancing Nanoeval's
workspace pin to current master `f1b4f8bc`, a real `regex-log` VM smoke passed
in 42.61 seconds with reward `1`, five Harbor-valid ATIF steps, a cached task
image, and a verifier dependency-cache hit. The revision change produced one
new content-addressed guest runtime. The immediate repeat resolved that runtime
in 0.87 ms and the task environment in 0.95 ms, hit the same verifier cache,
and passed again without rebuilding an image.

The definitive current-master run used concurrency eight plus a 24 GiB
task-declared memory ceiling. All 89 attempts reached trustworthy terminal
artifacts in **3,310.32 seconds (55m10s)**, 6.04% faster than the prior
concurrency-six milestone: 68 passed, 17 were ordinary verifier-scored
failures, four were typed safety refusals, and zero errored. The other 88
attempts completed in exactly 2,520 seconds (42m00s); the final generated
tensor-parallel candidate then reached the benchmark's canonical 900-second
verifier deadline. Nanoeval retained that deadline as exit-124 evidence,
reward `0.0`, and no exception instead of misclassifying it as a harness error.
The run reused 29,143,296 of 32,101,467 input tokens (90.78%) and had no
environment, VM-session, verifier-infrastructure, or Nanoeval failure.

Harbor 0.20 accepted the final job plus every one of its 89 configs, results,
and ATIF trajectories. `harbor view` served the 89-trial aggregate, the
deadline-scored tensor trial, and its eight-step trajectory directly. The four
service regressions—`pypi-server`, `git-multibranch`,
`install-windows-3.11`, and `nginx-request-logging`—all passed with their
agent-started processes and private loopback retained in the same guest through
verification.

The memory-aware scheduler kept the declared live set at or below 24 GiB,
including three concurrent 8 GiB attempts, and a separate three-task VM proof
with a 256 MiB ceiling showed strictly non-overlapping attempts. A deterministic
library test also proves work-conserving admission: a fitting small task may use
an available slot while a larger waiter is memory-blocked. Nanoeval does not
yet claim a speed ratio against Harbor because an exact same-revision Harbor
baseline has not been run.

## Tasks

`Task::load` reads a typed Terminal-Bench 2.1 task directory:

```text
task/
├── task.toml
├── instruction.md
├── environment/
└── tests/
    └── test.sh
```

The prompt and verifier are canonical inputs. Nanoeval does not rewrite either
to improve an agent's score. The included `write-greeting`,
`uppercase-message`, and `extract-todos` tasks are minimal end-to-end fixtures,
not substitutes for the full benchmark.

Native mode copies `environment/` into a disposable workspace and executes the
agent and verifier there. It rejects custom Compose environments and should be
used only for tasks whose declared userspace is already compatible with the
host.

## Harbor-compatible output

The optional `nanoeval-harbor` recorder writes a normal Harbor-shaped jobs
directory while attempts execute:

```text
nanoeval-runs/
└── <job-id>/
    ├── config.json
    ├── lock.json
    ├── result.json
    ├── job.log
    ├── job.json
    ├── run.json
    ├── .nanoeval.lock
    └── <trial-name>/
        ├── config.json
        ├── lock.json
        ├── result.json
        ├── trial.log
        ├── agent/
        │   ├── input.jsonl
        │   ├── events.jsonl
        │   ├── trajectory.json
        │   └── stderr.log
        ├── verifier/
        ├── artifacts/
        └── workspace/
```

`job.json`, `run.json`, `invocation.json`, and the advisory lock are Nanoeval's
typed durable lifecycle state; Harbor ignores those extra files. Trial and job
`result.json` files remain the terminal commit points used by resume, rerun
selection, and Harbor tooling.

Open it directly with Harbor:

```sh
harbor view nanoeval-runs --jobs
```

The current output validates with Harbor's own `JobConfig`, `JobLock`,
`JobResult`, `TrialConfig`, `TrialLock`, `TrialResult`, and ATIF-v1.7
`Trajectory` models. The adapter accumulates ATIF directly from its independent
subscription to the ordered Nanoeval event stream, with one step per model turn
and same-step tool calls and observations. It also matches Harbor's Python
`dirhash` task checksum and Packager content digest, so result and lock identity
agree with Harbor.

Harbor remains an output and inspection boundary; its Python runtime and Docker
executor are not dependencies of an attempt.

Use the typed inspector to turn a retained job into a failure census, then drill
into one exact trial:

```sh
cargo run -- inspect nanoeval-runs/<job-id>
cargo run -- inspect nanoeval-runs/<job-id> --trial nginx-request-logging
cargo run -- inspect nanoeval-runs/<job-id> --trial nginx-request-logging --full
cargo run -- inspect nanoeval-runs/<job-id> --json
```

The job view lists pass/fail/error status and failed CTRF tests. The trial view
adds exact assertion messages and traces, the final agent response, phase
timings, token/cache measurements, exception/refusal details, and paths to the
result, ATIF trajectory, exact JSONL, verifier output, network log, and retained
CoW root disk. `--full` inlines verifier stdout, agent stderr, and gvproxy logs;
`--json` emits the same report as strict typed JSON.

## How fast?

On the same `write-greeting` task, model, effort, and tool configuration on an
Apple Silicon development host, one measured native Nanoeval attempt completed
in **6.96 seconds** versus **32.95 seconds** through Harbor's Docker executor:

| Phase | Nanoeval native | Harbor Docker |
| --- | ---: | ---: |
| Environment setup | 0.001 s | 5.841 s |
| Agent setup | 0.000 s | 3.168 s |
| Agent execution | 6.917 s | 8.316 s |
| Verification | 0.038 s | 0.295 s |
| Other orchestration/finalization | ~0 s | 15.333 s |
| **Total** | **6.956 s** | **32.953 s** |

That run was **4.74x faster end to end**. Model time was nearly identical—6.04
seconds in Nanoeval and 6.22 seconds in Harbor—so the measured difference was
almost entirely harness overhead. A second successful Harbor run completed in
19.06 seconds, putting the observed improvement at 2.7–4.7x for this fixture.

These are diagnostic measurements, not a full Terminal-Bench result. Native
execution does not yet provide Harbor-equivalent Linux compatibility or
isolation, and model-service latency varies. The meaningful release gate is the
same comparison on representative Terminal-Bench tasks through Nanoeval's
Linux worker backend.

## Architecture

The repository follows the same library-first split as Nanocodex:

| Crate | Responsibility |
| --- | --- |
| `nanocodex-vm` | Exact standard Nanocodex tool contracts proxied to a VM-owned executor |
| `nanoeval` | Tasks, attempts, verification, scheduling, native job state, and typed event subscriptions |
| `nanoeval-harbor` | Streaming Harbor job/trial persistence and ATIF projection |
| `nanovm` | libkrun configuration, host capabilities, guest commands, and the low-level VMM lifecycle |
| `nanoeval-bin` | Thin CLI over the libraries |
| `nanoeval-examples` | Compiling public API consumers |

### Execution backends

The native backend is complete for compatible tasks. It is deliberately fast
and deliberately not a security boundary: task commands start in the attempt
workspace but otherwise execute as ordinary host processes.

The VM path embeds libkrun from its upstream `main` branch. On macOS it uses
Hypervisor.framework; on Linux it uses KVM. OCI/Dockerfile preparation produces
immutable ARM64 ext4 templates; every attempt reflinks one private writable root
disk, boots one guest, and routes the exact standard Nanocodex workspace tools
through its typed console:

```sh
cargo run -- vm run --root /path/to/rootfs -- /bin/uname -a
```

The Nanocodex harness remains outside the untrusted task guest while its
standard tools target that guest. Verification stays in the same live guest,
but tests remain hidden until the agent phase ends. Each attempt owns a private
CoW disk, VMM process, guest process tree, loopback namespace, and gvproxy
network stack.

See [`PLAN.md`](PLAN.md) for the ordered implementation plan,
[`HARNESS_PLACEMENT.md`](HARNESS_PLACEMENT.md) for the measured harness-placement
tradeoffs, and [`NANOCODEX_VM_TOOLS.md`](NANOCODEX_VM_TOOLS.md) for the proposed
tool seam.

## CLI and development

```sh
# Inspect the command surface.
cargo run -- --help
cargo run -- run --help
cargo run -- inspect --help

# Load and validate a task without running an agent.
cargo run -- task tasks/write-greeting

# Compile the public examples.
cargo check -p nanoeval-examples --bins

# Quality gates.
cargo fmt -p nanoeval -p nanoeval-bin -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Runnable library consumers:

```sh
cargo run -p nanoeval-examples --bin load-task -- tasks/write-greeting
cargo run -p nanoeval-examples --bin native-task -- tasks/write-greeting
cargo run -p nanoeval-examples --bin native-suite
cargo run -p nanoeval-examples --bin run-vm -- /path/to/rootfs /bin/uname -a
```

## Nanoeval versus Harbor

Use Nanoeval when Nanocodex is part of your Rust evaluation application and
you want attempts to be cheap, typed, and directly observable. Use Harbor when
you want its complete benchmark runner, environment-provider catalog, hosted
integrations, and established submission workflow.

| | Nanoeval | Harbor |
| --- | --- | --- |
| Product boundary | Rust library in your process | Python benchmark runner and orchestration product |
| Agent integration | Nanocodex builder linked into the harness | Installed agent adapter |
| Attempt events | Typed stream plus exact JSONL | Adapter-defined logs and trajectory |
| Native hot path | No Docker or agent installation | Environment-provider lifecycle |
| Isolation today | None in native mode | Docker and hosted environment providers |
| Retained output | Harbor-compatible job, trial, and ATIF | Canonical Harbor records |
| Linux direction | Warm libkrun workers, direct OCI inputs | Provider-specific environments |

The smaller boundary is the feature. Nanoeval does not replace Harbor's whole
product; it removes Harbor from the attempt hot path while preserving the
output needed to inspect and compare runs with Harbor tooling.

## Current tradeoffs

Nanoeval intentionally supports one agent SDK and one Terminal-Bench verifier
contract. Native mode offers workspace disposability but no hostile-code
containment. The VM path directly materializes every environment recipe in the
89-task Terminal-Bench 2.1 suite on Linux/ARM64, but currently boots one
libkrun guest per attempt rather than pooling warm multi-attempt workers.
Hardware CPU and memory boundaries come from the VM; job-level weighted memory
admission uses task declarations, while weighted CPU admission, sealed VM
snapshots, architectures other than the host, and task-specific outbound-network
policy remain active work.

That is substantially less infrastructure than a mature eval platform. It is
also much less machinery between a task and an agent attempt.

## License

Licensed under either the MIT License or the Apache License, Version 2.0, at
your option.
