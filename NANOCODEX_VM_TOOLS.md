# Nanocodex VM tool integration

Status: source audit and required small Nanocodex change. No implementation
exists.

## Short answer

`tools_factory` is the correct general seam. Nanoeval does not need a
`WorkspaceBackend`, execution-environment trait, or lifecycle hook.

The current Nanocodex implementation needs one focused correction before that
works: `ToolsBuilder::without_defaults()` only turns off web search and image
generation today. `ToolRuntime` still installs `exec_command`, `write_stdin`,
`update_plan`, `apply_patch`, and `view_image`, and registration rejects custom
tools with those names. The defaults must become an ordinary tool selection so
disabling a default also permits a replacement with the same canonical name.

Code Mode is a separate concern. The planned V8 isolate remains host-side and
acts only as a capability dispatcher. It can call either host tools or VM-bound
tools from the same registry; it must not expose unrestricted host filesystem,
process, module-loading, or environment APIs.

## Current construction path

The current path is:

```text
NanocodexBuilder
  .workspace(host PathBuf)
  .tools(...) or .tools_factory(...)
          |
          v
model agent canonicalizes the host directory
loads AGENTS.md from the host filesystem
constructs ToolRuntime::new(host workspace)
          |
          +-- local Node Code Mode host
          +-- local ShellSessions
          +-- local apply_patch
          +-- local view_image
          +-- update_plan
          +-- optional web/image generation
          +-- application tools from tools/tools_factory
```

Relevant behavior at the audited Nanocodex revision:

- `tools_factory` replaces only the `Tools` collection and receives an
  `AgentHandle`; it does not construct `ToolRuntime`.
- `without_defaults()` changes only `web_search` and `image_generation`.
- core workspace tool names are always reserved.
- `ToolRuntime::new()` installs every core workspace handler unconditionally.
- turn cancellation stops the local Code Mode process and local shell sessions
  owned by `ToolRuntimeControl`.
- workspace canonicalization and AGENTS.md discovery read the host filesystem
  before the tools run.

Those facts describe the implementation to simplify, not a reason to add a
second abstraction. Workspace context can be supplied as explicit agent input;
all effects remain ordinary tools.

## The Code Mode issue

The model sees Code Mode's `exec` and `wait` tools. A model-authored `exec`
cell runs in a local Node child process, which then invokes nested tools such as
`tools.exec_command(...)`.

The Node host currently:

- starts with the configured host workspace as its current directory;
- receives `require` created relative to that directory;
- evaluates model-authored source with `AsyncFunction`; and
- inherits the parent process environment.

Therefore the current Node host cannot remain the dispatcher. Once replaced by
the capability-only V8 isolate, replacing the nested tools does establish the
desired boundary: JavaScript orchestration stays on the host and filesystem or
process effects occur only through the selected tools.

## Recommended ownership

Nanocodex should continue to own:

- the model-visible `exec` and `wait` schemas;
- the nested workspace tool names, descriptions, argument schemas, and result
  shapes;
- Code Mode orchestration and nested-call ordering;
- output token limits and process telemetry;
- typed tool call/result events;
- turn cancellation semantics; and
- the distinction between model-visible command failures and fatal environment
  loss.

Nanoeval should own the attempt-scoped VM tools:

- logical workspace identity such as `/app`;
- command spawn, output, PTY, stdin, and process-session operations;
- rooted patch and file operations;
- image-file reads;
- default shell and guest environment policy;
- attempt cancellation and cgroup cleanup; and
- a terminal notification when the environment is lost.

Each VM-aware tool holds a cheap clone of the concrete `NanoVm` handle. The
model and ordinary tool authors do not know whether a tool targets the host or
a guest.

## Proposed construction

```rust,ignore
let vm = NanoVm::spawn(vm_config).await?;
let vm_tools = vm.clone();

let (agent, events) = Nanocodex::builder(auth)
    .tools_factory(move |_agent| {
        let vm = vm_tools.clone();
        Tools::builder()
            .without_defaults()
            .tool(VmExecCommand::new(vm.clone()))
            .tool(VmWriteStdin::new(vm.clone()))
            .tool(VmApplyPatch::new(vm.clone()))
            .tool(VmViewImage::new(vm))
            .tool(UpdatePlanTool::new())
            .build()
    })
    .build()?;

let eval = Nanoeval::new(agent, vm);
```

The factory runs once per agent driver, so forks and clean child agents receive
fresh tool objects. Multiple drivers may deliberately target the same attempt;
the concrete VM handle scopes process-session IDs to the attempt.

`Nanoeval::new` does not mutate or rebuild the agent. The caller binds its tools
to the VM before constructing Nanoeval, keeping both libraries independently
usable and ownership obvious. The returned `events` stream also remains
caller-owned, matching Nanocodex's normal library contract.

## Workspace tool behavior

### `exec_command` and `write_stdin`

These are one subsystem, not independent stateless tools. The VM protocol must
retain running processes, incremental captured output, PTY state, stdin, yield
deadlines, truncation metadata, and process-group ownership.

Remote session IDs must be opaque and scoped by attempt plus agent driver. A
session from one attempt must be impossible to address from another, even if
their numeric IDs coincide.

The guest should own process creation and pipe drainage. The harness should
retain Nanocodex's model-facing result shape and telemetry. Output travels as
bounded ordered chunks rather than one complete RPC response.

### `apply_patch`

The grammar and patch semantics must remain Nanocodex-owned. The efficient VM
path is a high-level patch operation executed next to the task filesystem using
the same shared patch engine, not a sequence of remote read/write calls for
every hunk.

The result remains the current concise added/modified/deleted summary. A failed
context match remains a model-visible tool failure.

### `view_image`

The sandbox resolves and reads the rooted path. Bytes stream to the trusted
harness, which owns model-history image encoding and detail policy. Large images
must not become one unbounded control-plane frame.

### `update_plan`

This stays entirely in the trusted harness. It has no task-workspace effect.

### Web search and image generation

Web search remains a trusted harness service. Image generation needs an
explicit policy if it writes a file into the task workspace: either transfer
the generated artifact through the workspace capability or keep it as
model-only content. Terminal-Bench can initially disable both without changing
the core design.

## Failure and cancellation semantics

Not every backend failure should be shown to the model as an ordinary failed
command.

| Failure | Owner | Outcome |
| --- | --- | --- |
| command exits nonzero | task process | model-visible result |
| invalid patch/path | workspace handler | model-visible result |
| output limit reached | Nanocodex policy | successful/truncated result with metadata |
| command timeout | attempt policy | terminate process tree, typed timeout result |
| transport interruption with sandbox intact | environment backend | reconnect or typed tool failure according to policy |
| sandbox/worker lost | Nanoeval supervisor | cancel turn and terminate attempt |

Today application `Tool` errors are converted into model-visible failed results.
A VM backend therefore also needs an out-of-band fatal-environment signal to
the attempt supervisor, which holds `TurnControl` and can cancel the active
turn. Environment loss cannot safely continue because the mutated filesystem
and process state no longer exist.

Cancellation is complete only after Code Mode cells stop, remote shell sessions
are terminated, the attempt cgroup is empty, and final output reaches EOF. A
closed transport is not a cleanup acknowledgement.

## Minimal spike versus durable API

A minimal experiment could:

1. make `without_defaults()` remove the local workspace handlers;
2. allow custom tools to use the previously reserved names;
3. install VM-aware replacements through `tools_factory`; and
4. run Code Mode in the host V8 capability isolate.

Nanocodex should expose reusable standard tool definitions/result helpers so
the VM-aware implementations do not copy schemas or formatting. Tool calls
still flow through the normal Nanocodex runtime, preserving integrated events,
output policy, and telemetry.

The durable lifecycle remains normal Rust ownership, not a `.lifecycle(...)`
hook: `Nanoeval` owns the Nanocodex session and `NanoVm`; each tool holds a cheap
VM handle; the attempt supervisor uses `TurnControl` and VM cancellation when
the environment dies; dropping the attempt performs bounded process cleanup.

## Validation gates

Before the VM environment can replace the local implementation, run the same
deterministic contract suite against both:

- byte-identical tool definitions and Code Mode descriptions;
- simple exec, nonzero exit, missing shell, custom cwd, and sanitized env;
- PTY and pipe modes;
- yield, polling, chunk IDs, output truncation, and persistent stdin;
- concurrent nested calls and multiple live Code Mode cells;
- patch add/update/move/delete and failed context;
- small, large, and missing images;
- turn cancellation during model, Code Mode, exec, blocked stdin, and output;
- fork and clean-child session isolation;
- AGENTS.md discovery and compaction context reload;
- worker loss and exactly one terminal attempt result; and
- representative retained-trace replay with no unexplained event divergence.

## Decision

Implement VM-aware workspace operations as ordinary application tools supplied
by `tools_factory`. Make Nanocodex's defaults genuinely selectable, retain Code
Mode as a host-side V8 dispatcher, and do not add `WorkspaceBackend` or generic
lifecycle abstractions.
