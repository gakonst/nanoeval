# nanoeval

Nanoeval is an exploration of a small, fast, crash-safe evaluation runner for
coding agents. Its first agent is Nanocodex, embedded directly as a Rust
library. Its first benchmark target is Terminal-Bench 2.1.

The repository is a Cargo workspace modeled after Nanocodex:

- `crates/nanovm` owns typed libkrun configuration, capabilities, and the
  low-level VMM lifecycle;
- `crates/nanoeval` owns task loading, fresh Nanocodex attempts, native
  workspaces, verification, events, and Harbor-compatible retained output;
- `bin/nanoeval` is a thin CLI consumer; and
- `examples` contains compiling public-library consumers.

The current vertical slice runs a complete, Docker-free native attempt. A
reusable `Nanoeval` is built from a `NanocodexBuilder`; every `task` call builds
a fresh Nanocodex session with a fresh tool runtime and disposable workspace.
The exact agent event stream is retained as JSONL. Job and trial configs,
locks, results, and the ATIF-v1.7 trajectory validate with Harbor's current
models; ATIF preserves tool calls and their observations. The retained jobs
directory can be opened directly by Harbor's viewer.

```sh
cargo run -- run tasks/write-greeting --thinking low
cargo run -- run tasks/write-greeting --trials 5 --concurrency 5 --thinking low
harbor view nanoeval-runs --jobs
```

The library API is the primary product:

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
        // Forward the independent typed stream to any external observer.
        drop(event);
    }
});

let result = eval.task(task.clone()).await?;
let five_fresh_results = eval.task_n(task, 5).await?;
# drop((result, five_fresh_results));
# Ok(())
# }
```

Native mode intentionally supports only tasks whose declared userspace already
exists on the host. It never builds or invokes the task's Docker image. Full
Terminal-Bench coverage remains the next backend milestone: consume OCI inputs
directly and execute them in the Docker-free Linux VM/worker path.

## Direction

- No Docker daemon, Docker builds, Compose, or task-side agent installation.
- Start with direct host execution for the fastest development loop.
- Prove a lightweight libkrun micro-VM path for faithful Linux task execution.
- Treat OCI images as immutable input artifacts, not as a requirement to use a
  container runtime.
- Keep the Nanocodex harness outside untrusted task sandboxes. Test both a host
  harness and a trusted in-worker harness; neither is installed in task images.
- Preserve canonical benchmark tasks and verifiers and measure every claimed
  speedup against the same inputs.

See [PLAN.md](PLAN.md) for the proposed architecture and
[HARNESS_PLACEMENT.md](HARNESS_PLACEMENT.md) for the measured tool-call and
developer-experience evaluation. [NANOCODEX_VM_TOOLS.md](NANOCODEX_VM_TOOLS.md)
records the current Nanocodex tool-runtime constraints and proposed VM seam.

## libkrun spike

Nanoeval currently consumes the `libkrun` Rust crate directly from its upstream
`main` branch. `Cargo.lock` records the exact tested commit while the manifest
keeps the dependency on `main` explicit.

On Apple Silicon, install the build prerequisites and prepare the firmware
bundle once:

```console
brew install lld
rustup target add aarch64-unknown-linux-musl
mkdir -p .cache/libkrunfw/v5.5.0
gh release download v5.5.0 --repo containers/libkrunfw \
  --pattern libkrunfw-prebuilt-aarch64.tgz \
  --dir .cache/libkrunfw/v5.5.0
tar -xzf .cache/libkrunfw/v5.5.0/libkrunfw-prebuilt-aarch64.tgz \
  -C .cache/libkrunfw/v5.5.0
make -C .cache/libkrunfw/v5.5.0/libkrunfw
```

Then boot any ARM64 Linux root filesystem directory. Cargo signs the development
binary and discovers the cached firmware automatically:

```console
cargo run -- vm run --root /path/to/rootfs -- /bin/uname -a
```

The first spike embeds libkrun in the Nanoeval process and runs one VM to
completion. It deliberately has no pool, worker subprocess, OCI acquisition,
or attempt protocol yet.

The reusable API will be `NanoVm::spawn(...)` followed by repeated
`vm.exec(...)` calls. The existing `KrunVm` is intentionally lower-level:
`KrunVm::run(...)` enters libkrun's non-returning event loop and therefore
belongs inside the future VMM child process, not in eval application code.
