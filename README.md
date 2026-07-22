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
task verifier, and retains the complete event stream and result bundle.

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
- Typed results and typed events are the runtime contract. Harbor and ATIF are
  faithful retained representations of that data.
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

## API

`Nanoeval` is a reusable evaluation recipe. It owns a cloneable
`NanocodexBuilder`, not a live conversation. Each call to `task` builds an
independent agent and attempt:

```rust,no_run
use nanocodex::Nanocodex;
use nanoeval::{Nanoeval, Task};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let task = Task::load("tasks/write-greeting")?;

let (eval, mut events) = Nanoeval::builder(Nanocodex::builder("api-key"))
    .output_directory("nanoeval-runs")
    .max_concurrency(5)
    .build()?;

tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        // Forward typed attempt and agent events to your own observer.
        drop(event);
    }
});

let first = eval.task(task.clone()).await?;
let five_fresh_attempts = eval.task_n(task, 5).await?;

assert!(first.artifacts.result_json.is_file());
assert_eq!(
    first.trajectory.final_metrics.total_steps,
    u32::try_from(first.trajectory.steps.len())?,
);
# drop(five_fresh_attempts);
# Ok(())
# }
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
drop(events);

let (greeting, uppercase, todos) = tokio::try_join!(
    eval.task_n(greeting, K),
    eval.task_n(uppercase, K),
    eval.task_n(todos, K),
)?;

assert_eq!(greeting.len() + uppercase.len() + todos.len(), 3 * K);
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
       │                                  optional side channel
       │ task(task)
       ▼
 fresh Nanocodex session ────────────► AgentEvents ──► events.jsonl
       │
       │ standard tools in a fresh workspace
       ▼
 canonical verifier
       │
       ├──► typed EvalResult
       ├──► ATIF-v1.7 trajectory
       └──► Harbor job and trial records
```

The evaluator can be reused indefinitely. Attempts never reuse conversation
history, tool sessions, mutable workspace state, or event sequence numbers.
Within one attempt, Nanoeval preserves the complete agent loop: the initial
user prompt is one ATIF step and every Nanocodex model inference is a separate
agent step containing that turn's message, reasoning, usage, tool calls, and
matching observations.

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

Nanoeval writes a normal Harbor-shaped jobs directory:

```text
nanoeval-runs/
└── <job-id>/
    ├── config.json
    ├── lock.json
    ├── result.json
    ├── job.log
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

Open it directly with Harbor:

```sh
harbor view nanoeval-runs --jobs
```

The current output validates with Harbor's own `JobConfig`, `JobLock`,
`JobResult`, `TrialConfig`, `TrialLock`, `TrialResult`, and ATIF-v1.7
`Trajectory` models. The typed `EvalResult` owns that `AtifTrajectory`; Harbor
only serializes the completed result. ATIF is accumulated directly from the
ordered Nanocodex event stream, with one step per model turn and same-step tool
calls and observations. Nanoeval also matches Harbor's Python `dirhash` task
checksum and Packager content digest, so result and lock identity agree with
Harbor.

Harbor remains an output and inspection boundary; its Python runtime and Docker
executor are not dependencies of an attempt.

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
| `nanoeval` | Tasks, attempts, verification, scheduling, typed results, and Harbor/ATIF retention |
| `nanovm` | libkrun configuration, host capabilities, guest commands, and the low-level VMM lifecycle |
| `nanoeval-bin` | Thin CLI over the libraries |
| `nanoeval-examples` | Compiling public API consumers |

### Execution backends

The native backend is complete for compatible tasks. It is deliberately fast
and deliberately not a security boundary: task commands start in the attempt
workspace but otherwise execute as ordinary host processes.

The experimental VM path embeds libkrun from its upstream `main` branch. On
macOS it uses Hypervisor.framework; on Linux it uses KVM. The current spike can
boot an ARM64 Linux root filesystem and run one command:

```sh
cargo run -- vm run --root /path/to/rootfs -- /bin/uname -a
```

The intended scored backend is a small pool of warm Linux workers. Each worker
will materialize OCI inputs without Docker and run many attempts concurrently,
with a private OverlayFS, namespaces, cgroup, and process tree per attempt. The
Nanocodex harness remains outside the untrusted task sandbox while its standard
tools target that sandbox.

See [`PLAN.md`](PLAN.md) for the ordered implementation plan,
[`HARNESS_PLACEMENT.md`](HARNESS_PLACEMENT.md) for the measured harness-placement
tradeoffs, and [`NANOCODEX_VM_TOOLS.md`](NANOCODEX_VM_TOOLS.md) for the proposed
tool seam.

## CLI and development

```sh
# Inspect the command surface.
cargo run -- --help
cargo run -- run --help

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

Nanoeval currently supports one agent SDK, one native verifier shape, and a
small subset of Terminal-Bench task environments. Native mode offers workspace
disposability but no hostile-code containment. The libkrun worker, direct OCI
materializer, resource enforcement, cancellation protocol, and full
Terminal-Bench 2.1 gate remain under active development.

That is substantially less infrastructure than a mature eval platform. It is
also much less machinery between a task and an agent attempt.

## License

Licensed under either the MIT License or the Apache License, Version 2.0, at
your option.
