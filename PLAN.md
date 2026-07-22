# Nanoeval plan

Status: the native vertical slice and first real libkrun eval slice are complete. A reusable `Nanoeval`
accepts a `NanocodexBuilder`, creates a fresh session and workspace per task,
runs a canonical verifier, and fans sequenced typed events out to independent
loss-aware subscriptions. The separate `nanoeval-harbor` adapter records one
subscription into Harbor-compatible job/trial/ATIF output while application
consumers receive the same stream independently. The simple `write-greeting` fixture has
passed live; its configs, locks, results, and ATIF validate with Harbor's own
models, and `harbor view` reads the job, trial, trajectory, verifier, and file
endpoints directly. ATIF is now a typed field on `EvalResult` and is folded
from the live event stream with one step per model turn rather than a lossy
two-step summary. The native concurrency milestone is also complete: the public
library example ran three tasks at `k=5` as 15 concurrent attempts, passed all
15 verifiers, and retained a Harbor-valid 15-trial job in 22.45 seconds from a
warm binary. The VM-backed version also passes all 15 trials with one retained
VM and Nanocodex session per attempt, 15 concurrent VMs, Harbor-compatible
output, and 21.10 seconds end to end. Agent/VMM setup averaged 63.9 ms. This
proof uses copied trusted rootfs directories and writable virtiofs; it is not
yet the scored isolation design.

## Library boundaries

The repository follows the same library-first layout as Nanocodex:

- `nanovm` owns virtualization: configuration, host capability discovery,
  libkrun/KVM/HVF lifecycle, the future VMM process protocol, execution, and
  pause/resume;
- `nanocodex-vm` owns the host-side Nanocodex tool proxies, typed console
  protocol, retained VMM session, and canonical guest tool runtime;
- `nanoeval` owns evaluation: task loading, preparation, attempts, verification,
  native job state, typed event subscriptions, and scheduling;
- `nanoeval-harbor` owns the explicit streaming Harbor and ATIF projection;
- `bin/nanoeval` is a thin CLI over those libraries; and
- `examples` compiles the intended public consumption paths.

The public composition starts from a cloneable `NanocodexBuilder`, not a live
session. `Nanoeval::task` clones that recipe and binds a fresh session, tool
runtime, workspace, and session ID for every attempt. `attempt_agent` applies
per-attempt resources to that fresh builder before it becomes a live agent; it
does not retrofit tools onto an already-built session.

## Thesis

Nanoeval should make coding-agent evaluation materially faster and more robust
by removing orchestration that does not contribute to the evaluation:

- link Nanocodex into a small attempt agent instead of dynamically installing
  an agent and its dependencies in every task image;
- use typed in-process events instead of a subprocess JSONL protocol;
- materialize each task environment once and start attempts from immutable
  snapshots;
- use a bounded work-conserving scheduler with durable per-attempt results; and
- keep compatibility exporters downstream of the native result rather than
  making Harbor's internal representation the runtime contract.

The first success condition is not a new general-purpose eval platform. It is
one reproducible Terminal-Bench 2.1 slice that is faster than the current Harbor
path without changing the task, prompt, verifier, model policy, or score.

## Hard constraints

1. Nanoeval does not invoke Docker, BuildKit, Compose, or a Docker-compatible
   daemon. OCI remains acceptable as an image distribution format.
2. Nanocodex is an in-process Rust dependency of Nanoeval's trusted harness. It
   is never installed in or made readable from an untrusted task rootfs.
3. A task environment is disposable. Attempts never share writable state.
4. Verifiers are canonical inputs. Nanoeval may package or inject them, but may
   not edit them to change outcomes.
5. Raw events, command streams, verifier output, and terminal state are
   committed before reporters or compatibility exporters run.
6. Cold task acquisition/materialization is reported separately from warm
   attempt execution.

## Two execution backends

### Native backend

The first backend runs Nanocodex's existing shell, patch, and file tools against
a host directory. This requires no Nanocodex changes and gives the shortest
development loop for deterministic fixtures and tasks known to be host-safe.

Native execution is not automatically Terminal-Bench-compatible. A task may
depend on Linux binaries, users, paths, services, or package versions that do
not exist on macOS. Such a mismatch must be classified as an unsupported
environment, not as an agent failure.

### libkrun micro-VM backend

The Linux backend starts with upstream libkrun 2.0 from its `main` branch.
Cargo embeds libkrun's Rust crate directly and `Cargo.lock` pins the exact
tested commit. libkrun supplies a minimal device model over HVF on Apple
Silicon and KVM on Linux without an external VMM executable.

```text
Nanoeval host process
  worker launcher/monitor
  optional harness placement
           |
           | coarse job control and artifact export
           v
  libkrun virtio-vsock/control channel
           |
           v
Warm libkrun worker VM
  trusted control namespace
    Nanoeval scheduler and durable event journal
    Nanocodex harnesses + model credentials
    local execution broker
  content-addressed task rootfs cache
  attempt A: untrusted private rootfs/namespaces/cgroup
  attempt B: untrusted private rootfs/namespaces/cgroup
  attempt C: untrusted private rootfs/namespaces/cgroup
```

libkrun uses HVF on macOS and KVM on Linux. It virtualizes the host architecture
and does not provide software CPU emulation.

The primary throughput hypothesis remains a small pool of large, warm worker
VMs, not one VM boot per attempt. OCI images normally supply userspace, while
the worker supplies the Linux kernel. A worker can therefore execute many
different task images concurrently as long as their architecture and required
kernel capabilities match.

Each active attempt owns a sandbox inside the worker:

- a read-only content-addressed task rootfs;
- a private OverlayFS upper and work directory;
- mount, PID, network, IPC, and UTS namespaces;
- a cgroup v2 subtree for accounting, limits, and reliable process cleanup; and
- one logical RPC channel identified by its attempt ID.

The worker's execution broker creates the namespaces, pivots into the overlaid
rootfs, and starts a small reaper as PID 1 for the sandbox. The task rootfs does
not contain the harness or its credentials. A trusted Nanocodex harness invokes
workspace effects through the broker's narrow execute/file/session API. When
the harness is placed inside the worker's control namespace, this hot path is a
local Unix socket rather than a VM boundary. The verifier executes in the same
task sandbox after the agent, so it observes the attempt's mutations.

Cleanup uses the cgroup kill operation, waits for process exit and stream EOF,
unmounts the overlay, and removes only that attempt's writable directories. The
libkrun worker stays alive for subsequent attempts. This uses the same Linux
kernel primitives that container runtimes use, but there is no Docker daemon,
image build, container CLI, or per-attempt VM boot in the path.

A dedicated per-attempt micro-VM remains a compatibility and isolation fallback
for tasks that need global kernel changes, privileged devices, conflicting
kernel modules, or stronger separation. The scheduler records the selected
isolation class; results from different classes are never silently conflated.

The first bring-up deliberately maps a trusted rootfs directory through
virtio-fs and executes one command with libkrun's embedded init. This is a
developer baseline, not the scored isolation design: libkrun documents that
virtio-fs must be combined with host mount isolation for untrusted guests.
The scored path must either confine the VMM's host process or use immutable raw
block images plus a fresh writable disk.

On the July 21, 2026 Apple Silicon development host, the first uncached process
and VM invocation completed `uname` in 0.48 seconds. Ten immediately repeated
fresh-process/fresh-VM `/bin/true` runs completed in 0.13--0.17 seconds. TSI
outbound networking also worked after installing a TCP DNS resolver entry.
These are bring-up observations, not benchmark claims.

## Docker-free task image path

Terminal-Bench task images should be consumed without a container runtime:

1. Resolve the configured OCI reference to an immutable manifest digest.
2. Pull and cache the manifest, config, and compressed layers by digest using
   the OCI Distribution API.
3. Apply layers in order into a staging rootfs, including whiteouts, ownership,
   permissions, links, and extended attributes required by the benchmark.
4. Add the small guest-supervisor/init contract without changing task
   userspace semantics.
5. Materialize a sparse ext4 base disk and an asset manifest keyed by every
   source digest and conversion version.
6. Transfer or expose the immutable rootfs to each compatible warm worker once.
7. Start every attempt from a new in-guest OverlayFS upper directory.
8. Optionally seal a worker checkpoint after deterministic worker
   initialization to make worker recycling faster.

This is intentionally different from Gondolin's current OCI builder, which
uses Docker or Podman to pull and export an OCI filesystem. Nanoeval needs a
direct OCI materializer so that the no-Docker invariant is true during both
preparation and execution.

The image conversion is a cold operation. A warm evaluation must never rebuild
or flatten an unchanged rootfs.

## Nanocodex library integration and placement

One trusted harness process owns each fresh Nanocodex session and its event
receiver. It supplies stable instructions, thinking policy, session identity,
workspace policy, and tools; awaits the typed turn result; and concurrently
commits the typed `AgentEvents` stream. The harness is never part of the task
rootfs or task PID namespace.

There are three placements worth measuring:

| Placement | Hot tool path | Main benefit | Main cost |
| --- | --- | --- | --- |
| macOS/Linux host | virtio RPC into worker | strongest failure, credential, and event separation | every workspace effect crosses the VM boundary |
| trusted worker control namespace | local Unix RPC into task sandbox | no VM hop for tools; harness still outside task | worker failure also interrupts harnesses |
| inside task sandbox | direct syscalls | simplest local tools | task code shares the harness failure and credential boundary |

The third placement is a useful throwaway baseline, not the desired durable
architecture. Anthropic's Managed Agents experience is directly relevant: the
harness, session, and sandbox should have independent lifecycles, and the
sandbox should be replaceable after an execution failure without containing the
model credential or authoritative session state.

The likely Nanoeval sweet spot is the second placement. The large libkrun VM is an
outer macOS/Linux compatibility and containment envelope. Inside it, Nanoeval
and Nanocodex live in a trusted control namespace, while each Terminal-Bench
attempt is an untrusted set of namespaces and a private rootfs. This preserves
the harness/sandbox interface without paying a VM round trip per command.

The first placement remains attractive if measured virtio overhead is
negligible. It keeps the durable journal alive when a worker crashes and keeps
all credentials outside the VM. Placement must therefore be selected by a
representative trace replay, not intuition.

Both separated placements use Nanocodex's general `tools_factory` to install
the standard tool names with VM-aware implementations. No Nanoeval-specific
`WorkspaceBackend` trait is needed. `nanocodex-tools` owns the canonical
standard names, definitions, schemas, and apply-patch grammar; both its native
handlers and `nanocodex-vm` consume those contracts. `nanocodex-vm` proxies
`exec_command`, `write_stdin`, `apply_patch`, and `view_image` through one
clone-cheap VM tool client. Code Mode and `update_plan` remain on the host.
The future concrete VM client covers bounded exec, guest-owned persistent
sessions, cancellation, patch/file operations, and image reads. With trusted
in-worker placement its implementation uses local Linux IPC rather than a VM
hop.

The model loop, retries, Responses WebSocket, typed history, code mode, and
event semantics stay in Nanocodex. Nanoeval owns sandbox capabilities, attempt
lifecycle, persistence, and verification.

## What to adopt from related libkrun projects

Adopt or validate:

- libkrun's direct kernel boot, embedded init, TSI, virtio-fs, block, console,
  and vsock APIs;
- krunkit's macOS entitlement/signing and libkrun lifecycle behavior;
- krunvm's simple rootfs/command/environment setup, without its Buildah image
  management;
- a small guest supervisor and framed vsock event/control protocol;
- explicit command cancellation and stream completion;
- content-addressed guest assets; and
- cheap worker recycling from prepared state.

Do not initially adopt:

- a Node/TypeScript host sidecar;
- a JavaScript virtual filesystem;
- krunvm itself or its Buildah-backed OCI management;
- krunkit's EFI/GPU-oriented general VM surface;
- SSH as the normal command transport; or
- Docker/Podman-backed OCI export.

libkrun, krunvm, and krunkit are Apache-2.0. They are evidence and source
references; Nanoeval should own only its small eval-specific control plane.

## Architecture risk: guest CPU architecture

Apple Silicon libkrun runs ARM64 guests with HVF and cannot run an x86_64 guest.
If Terminal-Bench's published images are amd64-only, they require compatible
ARM64 userspace, explicit user-mode translation, or an x86_64 KVM worker.

Before the VM backend is selected, audit every pinned Terminal-Bench 2.1 image
manifest and classify it as:

- native arm64 available;
- amd64-only but reproducibly materializable for arm64;
- amd64-only and suitable for user-mode translation inside an arm64 guest; or
- requires an x86_64 KVM worker for canonical execution.

Likely deployment can support both Apple Silicon development workers and
x86_64 Linux KVM scoring workers while keeping the same guest protocol and
result format. Cross-architecture scores must never be silently mixed.

## Attempt lifecycle

```text
pending
  -> resolving immutable inputs
  -> acquiring backend permit
  -> starting environment
  -> running Nanocodex turn
  -> cancelling or completing agent work
  -> running canonical verifier
  -> sealing artifacts and result
  -> cleaning environment
  -> terminal
```

One host attempt supervisor owns the durable record, deadline, worker sandbox
lease, verifier decision, and terminal transition. One guest attempt-agent owns
the Nanocodex session, typed event receiver, and task process tree. The worker
process is shared infrastructure, but no mutable task state is shared. A retry
is a new attempt with explicit lineage and never overwrites its parent.

The scheduler should have separate permits for model attempts and expensive VM
or verifier work only after measurements show that one global limit leaves
resources idle or causes contention.

## Durable finite jobs

Automatic completion and restart are defined only for a finite `EvalPlan`.
The existing reusable `eval.task(...)` and `eval.task_n(...)` calls remain
useful ad hoc operations, but an open-ended library object cannot infer when a
caller has finished adding work and therefore cannot seal itself automatically.

The durable CLI contract will be:

```text
nanoeval run ...    resume the matching active plan, or create and execute it
nanoeval run        resume the retained active plan after interruption
nanoeval start ...  require no active job, then create and execute a fresh plan
nanoeval end        request cancellation and seal the active job as stopped
nanoeval status     inspect the active job without mutating it
```

One atomic active-job pointer exists per output directory. A job retains an
immutable plan, an atomic state snapshot, and an append-only typed transition
journal. The stable trial slot is task identity × agent variant identity ×
one-based ordinal. Each execution of that slot has a fresh execution ID,
Nanocodex session, tool runtime, event sequence, and workspace.

After Ctrl-C, completed slots remain committed and incomplete slots are
interrupted. The next `run` skips completed slots and starts fresh executions
for incomplete slots; it never resumes an interrupted conversation or dirty
workspace. A live executor lease prevents two CLI processes from executing the
same job concurrently. `end` prevents new claims, cancels live work, waits for
cleanup, and preserves every completed result.

Native state is authoritative. Events are durably appended before live fanout,
and Harbor must be rebuildable from the retained plan, journal, and typed
results rather than owning the only copy of them. Thinking and tool sweeps use
caller-defined stable variant and tool-profile IDs so resume can reject a
changed plan without attempting to hash opaque builder closures.

## Measurement model

Every attempt reports at least:

- cold OCI download and rootfs materialization;
- worker acquisition and worker boot/recycle latency when applicable;
- task-rootfs cache and per-attempt OverlayFS setup;
- model connection, model work, and Nanocodex tool work;
- per-command RPC and guest execution time;
- verifier time;
- artifact publication and cleanup; and
- peak host CPU, memory, and disk use.

Initial hypotheses to falsify, not promises:

- warm attempt sandbox creation should be tens of milliseconds or less;
- a prepared native-architecture worker should become exec-ready in about one
  second or less after a recycle, with that cost amortized across many attempts;
- control-plane latency should be negligible relative to normal shell commands;
- embedding Nanocodex should remove all per-attempt agent installation time;
  and
- duration-aware scheduling should reduce the long tail of a full benchmark.

Comparisons use the same task order, concurrency, task and verifier digests,
Nanocodex revision, model, thinking level, prompt, and network policy. Report
trials/hour and tail completion alongside correctness; a faster runner that
changes scores is not a valid improvement.

## Planned slices and decision gates

### 0. Freeze the comparison inputs

- Pin the Nanocodex revision and Terminal-Bench 2.1 package manifest.
- Record the current Harbor result and artifact semantics used for submission.
- Inventory all task OCI digests, architectures, entrypoints, users, environment
  variables, working directories, services, resource needs, and verifier
  dependencies.

Gate: Nanoeval can state exactly which upstream inputs define an equivalent
trial.

### 1. Native deterministic attempt

- Embed Nanocodex against a temporary host workspace.
- Journal typed events while awaiting the turn result.
- Run a deterministic verifier and atomically publish one result.
- Exercise timeout, cancellation, runner termination, and restart.

Gate: no partial stream becomes a completed result; no process survives a
cancelled attempt; restart produces exactly one terminal record.

### 2. libkrun no-model worker spike

- Keep the completed one-command libkrun baseline reproducible.
- Add a tiny guest supervisor and compare one-VM-per-attempt with one warm
  worker running namespace-isolated task sandboxes on the same machine.
- Measure worker boot once, then 100 cycles of sandbox creation, exec, streamed
  output, cancellation, verification, cgroup cleanup, and overlay deletion.
- Repeat at concurrency 1, 2, 4, and the first contention point.
- Deliberately crash and recycle a worker while other job state remains durable.

Gate: select the worker topology and a Rust-native guest/control plane from
measured complexity, fault containment, and performance.

### 3. Docker-free OCI materializer

- Pull one simple task image directly by digest.
- Apply layers into ext4 and prove filesystem metadata against an independent
  OCI reference extraction.
- Cache by complete content and converter digest.
- Expand to the full architecture and image inventory.

Gate: no Docker-compatible runtime is invoked, and repeated preparation of an
unchanged task is a cache hit.

### 4. Harness placement and workspace capability

- Add the smallest general workspace execution backend to Nanocodex.
- Replay the same representative tool transcripts with the harness on the host,
  in the trusted worker namespace, and as an intentionally coupled baseline.
- Measure per-call latency, stream throughput, CPU, cancellation, and failure
  recovery with small commands, large outputs, and persistent sessions.
- Verify command ordering, output truncation, patches, credential isolation,
  typed events, and worker-crash accounting.

Gate: select placement from measured end-to-end overhead and failure behavior;
Nanocodex's public event contract is unchanged, and no task sandbox can access
the harness credential or authoritative journal.

### 5. One canonical Terminal-Bench task

- Materialize an image directly from its pinned OCI digest.
- Execute the unmodified task instruction and verifier.
- Reconcile filesystem outcome, verifier output, trajectory, reward, usage, and
  timing with Harbor.

Gate: repeated Nanoeval and Harbor trials show equivalent environment and result
semantics before any full benchmark run.

### 6. Durable bounded scheduler

- Add stable job and attempt IDs, immutable artifacts, resume, retry lineage,
  duration-aware ordering, and serialized reporting outside execution permits.
- Fault-inject termination at every durable transition.

Gate: randomized restart tests retain byte-identical completed artifacts, at
most one committed verifier result per attempt, deterministic accounting, and
no leaked VM or guest process.

### 7. Focused and full comparisons

- Run a cheap representative subset first.
- Compare warm and cold paths separately at increasing concurrency.
- Inspect exact event logs, guest command streams, verifier output, and result
  exports before claiming parity or speed.
- Run the full configured evaluation only as a milestone gate.

Gate: measured throughput or tail-time improvement with no unexplained score or
artifact divergence.

## Open decisions

- Are enough Terminal-Bench images native arm64 to make Apple Silicon a useful
  scoring worker, or should canonical runs target x86_64 KVM immediately?
- Does libkrun 2.0 `main` expose every lifecycle primitive the warm worker needs,
  or should the guest remain long-lived while attempts are managed entirely
  inside it?
- Is host-to-worker virtio tool latency material on representative Nanocodex
  traces, or is host placement operationally superior at effectively no cost?
- If the harness lives in the trusted worker namespace, is an attached durable
  result disk sufficient to recover authoritative events after VM failure?
- How many concurrent sandboxes should a worker accept before CPU, memory, page
  cache, or verifier contention dominates?
- Which tasks require a dedicated VM because they mutate global kernel or
  network state rather than staying inside normal namespace boundaries?
- How frequently should a shared worker be health-checked or recycled to bound
  contamination and memory fragmentation?
- Which tasks require network access, background services, privileged kernel
  features, nested virtualization, or more than one guest?
- Is qcow2 overlay deletion sufficient for reset, or do any task/verifier
  artifacts need a separate host-owned journal before VM shutdown?
- Which Harbor and ATIF fields are required for Terminal-Bench submission, as
  opposed to Harbor-internal convenience?

## Immediate next experiment

Do not build the full runner yet. Continue two narrow tracks:

1. make the finite `EvalPlan` the durable native execution source of truth and
   implement `run` resume, `start`, `end`, and stale-executor recovery;
2. turn the one-shot guest command into a tiny, long-lived guest supervisor
   reachable over a libkrun console or vsock channel;
3. connect its concrete client to the existing `nanocodex-vm` proxies and
   measure 100 in-guest process launches, streamed output, cancellation, and
   cleanup without rebooting the VM; and
4. inventory every pinned Terminal-Bench 2.1 OCI manifest architecture before
   building the image materializer.

Those results decide whether one warm libkrun VM should multiplex attempts or
whether fast one-VM-per-attempt execution is already the simpler throughput
winner.
