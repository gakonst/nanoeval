# Harness placement evaluation

Status: measured trace analysis plus proposed experiment. No transport or
runner implementation exists yet.

## Provisional conclusion

Test the trusted host harness first, with the trusted in-worker harness as the
challenger. Do not put Nanocodex inside the untrusted task sandbox for the
durable design.

The retained Terminal-Bench traces suggest that a well-designed virtio tool
boundary will not materially affect end-to-end performance. The harder and more
important work is preserving stream, cancellation, process-session, file, and
failure semantics. Host placement also follows the useful Managed Agents
separation: the harness and authoritative session survive independently from a
replaceable execution environment.

This is not yet a final selection. Both separated placements must replay the
same representative traces before the architecture is locked.

## How Nanocodex tool calls work

Nanocodex does not expose every shell/file primitive directly to the model. The
model normally sees Code Mode's `exec` and `wait` tools.

```text
model emits an exec JavaScript cell
          |
          v
Nanocodex Code Mode evaluates it in the trusted harness
          |
          | tools.exec_command(...)
          | tools.write_stdin(...)
          | tools.apply_patch(...)
          | tools.view_image(...)
          v
workspace-effect handlers
          |
          v
selected attempt sandbox
```

`exec` itself stays with the harness. Only nested effects that read or mutate
the task environment need to enter the sandbox. `update_plan`, context access,
model calls, retries, typed history, and event persistence remain harness-local.

The current built-in handlers execute directly against a host workspace. A
separated Nanoeval topology needs VM-aware tools installed through Nanocodex's
general tool factory. They preserve the same model-visible names and result
shapes while changing where the effect occurs.

### Execution protocol

The backend capability should be attempt-scoped. Its minimum operations are:

- start an exec with command, cwd, environment policy, output limit, and initial
  yield deadline;
- stream ordered stdout and stderr chunks with a call ID and sequence;
- return exit code, elapsed time, truncation metadata, and optional persistent
  session ID;
- write bytes or signals to a persistent session;
- apply a patch or perform rooted file operations;
- read an image or file without escaping the task root;
- cancel one call or terminate every process in an attempt; and
- report that the sandbox itself was lost distinctly from a command failure.

For a host harness, these frames travel over a persistent virtio transport into
the warm worker. For an in-worker harness, the same capability uses a local Unix
socket. Nanocodex should not know which transport implements it.

The result path must be streaming. A tool call cannot accumulate its complete
output into one transport message before the harness sees anything. Backpressure
must bound memory without blocking guest-process pipe drainage.

### Cancellation

`Turn::cancel()` first stops model/tool orchestration, then cancels the active
workspace call. The sandbox broker sends the owned process group a graceful
termination, waits a bounded interval, kills the attempt cgroup, drains final
pipe bytes through EOF, and returns one terminal call status. A closed RPC
stream is not proof that the process stopped.

## Evidence from retained Terminal-Bench traces

Source: 321 retained event streams from
`2026-07-19__tb21-leaderboard-high-k5-r2` in the Nanocodex checkout. The
analysis counts only sandbox effects: `exec_command`, `write_stdin`,
`apply_patch`, and `view_image`. Code Mode `exec`, `wait`, and `update_plan`
remain harness-local.

| Measure | Observed |
| --- | ---: |
| Effect calls | 7,651 |
| Calls per trace, mean | 23.83 |
| Calls per trace, p50 / p90 / p95 / p99 / max | 14 / 55 / 78 / 117 / 170 |
| Effect duration, p50 | 847 ms |
| Effect duration, p90 / p95 / p99 | 30.002 / 30.005 / 30.102 s |
| Calls below 1 ms / 10 ms / 100 ms | 329 / 829 / 1,309 |
| Encoded result volume | about 278 MB |
| Result size, p50 / p90 / p95 | 451 B / 8.2 KB / 18 KB |
| Result size, p99 / max | about 1.0 MB / 64.4 MB |

Per-tool durations:

| Tool | Calls | p50 | p90 | p99 |
| --- | ---: | ---: | ---: | ---: |
| `exec_command` | 4,959 | 634 ms | 8.99 s | 30.02 s |
| `write_stdin` | 1,772 | 10.01 s | 30.01 s | 50.01 s |
| `apply_patch` | 690 | 2.53 ms | 31.64 ms | 367.83 ms |
| `view_image` | 230 | 2.58 ms | 22.08 ms | 1.12 s |

These are tool-handler durations, not isolated process times, and the 10/30/50
second clusters reflect yield and polling behavior. They are still the correct
corpus for estimating how often a sandbox boundary is exercised.

## Modeled latency impact

At the observed mean of 23.83 effects per trace:

| Added round-trip cost | Added mean time per trace |
| ---: | ---: |
| 1 ms | 24 ms |
| 5 ms | 119 ms |
| 10 ms | 238 ms |

The retained run's median agent duration was approximately 185 seconds. Even a
5 ms cost per effect would model to roughly 0.06% of that median duration.
This estimate does not prove virtio performance, but it shows that ordinary
round trips are unlikely to decide throughput.

The fast file tools are different: a 5 ms boundary would dominate a 2.5 ms
`apply_patch` or `view_image` handler. This can affect a microbenchmark and
interactive feel even when it barely changes a complete evaluation. The
experiment must therefore report both tool latency and end-to-end attempt time.

Large and bursty results are a more credible concern than call latency. The
transport must demonstrate bounded memory and good throughput with 1 MB and
64 MB outputs, concurrent calls, and a slow journal consumer.

## Placement comparison

### Host harness

Developer experience strengths:

- `cargo run` and debugger operate directly on Nanoeval and Nanocodex;
- credentials and authoritative events never enter the worker VM;
- worker death is an explicit environment error rather than disappearance of
  the harness and its diagnostics;
- native and libkrun environments can implement the same workspace capability;
- replacing or resizing workers does not redeploy the harness; and
- the UI can observe typed events without reconstructing them across a failed
  worker.

Developer experience costs:

- workspace tools need a real remote backend rather than today's local
  built-ins;
- every error needs a clear command-versus-transport-versus-sandbox-loss class;
- debugging a tool call spans host and guest logs; and
- path handling, large image reads, persistent sessions, and cancellation must
  work across the transport.

### Trusted in-worker harness

Developer experience strengths:

- the effect transport is a local Unix socket;
- Linux path, process, and shell behavior are easier to reproduce locally;
- large outputs stay inside the VM until event/artifact publication; and
- there is no virtio tool hot path.

Developer experience costs:

- updating or debugging the harness requires deploying or attaching to the
  worker;
- a libkrun VM failure interrupts every harness assigned to it;
- authoritative state needs a durable attached disk or external session store;
- credentials exist in the worker's trusted namespace; and
- the macOS developer loop has a remote-debugging step.

### Coupled task-sandbox harness

This is easiest to prototype because existing Nanocodex tools make direct
syscalls. It is a useful performance floor, but poor as the durable design:
task code shares the harness failure boundary, credentials are harder to make
structurally unreachable, and a dead sandbox can take the only useful
diagnostics with it.

## Developer-experience contract

Placement should not change how an eval author works. A good design provides:

- one task manifest, instruction, environment digest, and verifier;
- the same run command for native and VM workers;
- explicit cold acquisition versus warm execution status;
- live ordered model and tool events without scraping guest logs;
- a shell attached to a retained failed sandbox when requested;
- replay of an exact attempt or recorded tool transcript;
- typed failures that name the broken layer;
- automatic cgroup/process cleanup on cancellation; and
- no Docker daemon, image rebuild, task-side agent install, or manually managed
  port mapping.

The task author should never implement the host/guest protocol. Environment
backends own it. Nanocodex tool authors continue implementing the public `Tool`
contract; only workspace-relative built-ins use the environment capability.

## Experiment matrix

Replay the same deterministic workload through all three placements:

1. 10,000 no-op and tiny-output execs to expose boundary latency.
2. Representative retained call timing and output-size distributions.
3. Concurrent 1 MB and 64 MB stdout/stderr streams with a slow consumer.
4. Persistent sessions with interleaved `write_stdin` and output.
5. Patch and image operations on small and large files.
6. Cancellation during spawn, active output, blocked stdin, and descendant
   process creation.
7. Worker termination during each call phase, followed by durable retry.
8. Concurrency 1, 2, 4, 8, and the first CPU/memory contention point.

Report:

- first-byte and terminal latency by operation;
- end-to-end replay time and percent overhead over coupled direct execution;
- bytes copied, peak buffered bytes, host and guest CPU, and memory;
- cancellation-to-empty-cgroup latency;
- lost, duplicated, or reordered stream chunks;
- worker recovery time and terminal attempt accounting; and
- setup steps and debugging actions required for each placement.

## Decision gate

Choose host placement if representative end-to-end overhead is within natural
run-to-run noise and all stream/cancellation invariants hold. Choose trusted
in-worker placement only if it produces a material measured advantage that
outweighs its deployment, durability, and credential costs.

Do not select the coupled task-sandbox placement solely because it wins a
no-op microbenchmark.
