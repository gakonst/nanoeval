# KVM and Hardware-Virtualization Sandbox Landscape for Nanoeval

Research date: July 21, 2026.

## Executive conclusion

Nanoeval should keep libkrun, but should not make one large warm libkrun VM
multiplexing mutually untrusted attempts its only architecture.

The strongest design is a shared host-side runner contract with three
interchangeable execution modes:

1. Canonical Linux scoring: one Firecracker VM per attempt, restored from a
   warm memory snapshot where profitable.
2. Fast macOS development: one libkrun, Microsandbox, BoxLite, or Apple
   Containerization VM per attempt.
3. Maximum-throughput experimental mode: a warm libkrun VM running namespace-
   and cgroup-isolated attempts, recycled aggressively.

The third mode is fastest in principle, but weakens the security boundary from
"VM per attempt" to "shared guest kernel." That is a material architectural
change, not merely an optimization.

This review is based on repository source, architecture documentation, issues,
releases, and current commits rather than marketing pages alone. Terms used
below:

- **Fact**: verified in source or current GitHub state.
- **Claim**: a project-published benchmark or security assertion that was not
  independently reproduced here.
- **Inference**: a conclusion about Nanoeval drawn from those facts.

## Maintenance and adoption signal

GitHub stars are only a rough adoption proxy. Recent releases, commits,
downstream use, and issue activity are stronger maintenance signals.

| Project | Signal on July 21, 2026 | License | Nanoeval relevance |
|---|---:|---|---|
| [Firecracker](https://github.com/firecracker-microvm/firecracker) | About 35.6k stars; active July 21; v1.16.1 July 2 | Apache-2.0 | Best mature Linux snapshot baseline |
| [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor) | About 6.0k; active July 21; v53.0 July 12 | Apache-2.0/BSD-3-Clause | Capable Linux VMM; broader device set |
| [crosvm](https://github.com/google/crosvm) | About 1.3k mirror stars; active July 21; Gerrit is authoritative | BSD-3-Clause | Excellent process-per-device isolation reference |
| [rust-vmm](https://github.com/rust-vmm) | Component ecosystem actively maintained | Mostly Apache-2.0/BSD-3-Clause | Building blocks, not a runner |
| [Kata Containers](https://github.com/kata-containers/kata-containers) | About 8.4k; active July 21; 4.0.0 July 20 | Apache-2.0 | Best full container-in-VM control-plane reference |
| [libkrun](https://github.com/libkrun/libkrun) | About 2.5k; active July 17; v1.19.4 July 3 | Apache-2.0 | Best cross-platform embedded VMM substrate |
| [krunkit](https://github.com/libkrun/krunkit) | About 330; v1.3.2 July 3 | Apache-2.0 | macOS lifecycle and signing reference |
| [krunvm](https://github.com/libkrun/krunvm) | About 1.7k; v0.2.6 February 9 | Apache-2.0 | Simple OCI-to-libkrun reference |
| [E2B infra](https://github.com/e2b-dev/infra) | About 1.3k; active July 21; 2026.28 July 9 | Apache-2.0 | Best complete warm-snapshot sandbox architecture |
| [Microsandbox](https://github.com/superradcompany/microsandbox) | About 7.0k; active July 21; v0.6.6 July 7 | Apache-2.0 | Closest libkrun-based Nanoeval analogue |
| [BoxLite](https://github.com/boxlite-ai/boxlite) | About 2.2k; active July 21; v0.9.7 July 1 | Apache-2.0 | Strong OCI/libkrun/jailer reference |
| [Apple Containerization](https://github.com/apple/containerization) | About 8.8k; active July 20; 0.33.3 June 1 | Apache-2.0 | Strongest macOS one-VM-per-container alternative |
| [Gondolin](https://github.com/earendil-works/gondolin) | About 1.8k; active July 6; v0.12 May 19 | Apache-2.0 | Coding-agent-specific network and secret design |
| [Clone](https://github.com/unixshells/clone) | About 350; active July 20; no stable release | MIT | New experimental KVM VM-fork implementation |
| [Hyperlight](https://github.com/hyperlight-dev/hyperlight) | About 4.5k; active July 21; pre-1.0 | Apache-2.0 | Excellent embedded/cancellation design; wrong guest model |
| [gVisor](https://github.com/google/gvisor) | About 18.8k; active July 21 | Apache-2.0 | Still-relevant alternative to a conventional VM |
| [vHive](https://github.com/vhive-serverless/vHive) | About 340; active July 20; v1.8.2 February 13 | MIT | Valuable snapshot research with operational caveats |
| [Unikraft](https://github.com/unikraft/unikraft) / [KraftKit](https://github.com/unikraft/kraftkit) | About 3.8k / 430; both active; Unikraft 0.21 May 20 | BSD-3-Clause default, per-file exceptions | Poor fit for arbitrary Terminal-Bench Linux workloads |
| [Ignite](https://github.com/weaveworks/ignite) | About 3.5k; archived; last substantive work 2023 | Apache-2.0 | Historical reference only |

## Project findings

### Firecracker

**Topology and control (fact).** One Firecracker VMM process owns each microVM,
normally launched through `jailer`. The jailer establishes mount/chroot,
UID/GID, cgroup, optional PID/network namespaces, environment cleanup, and FD
cleanup; Firecracker applies seccomp. Host control is an HTTP API over a Unix
socket. There is no prescribed guest agent. See the
[jailer documentation](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md)
and [VMM design](https://github.com/firecracker-microvm/firecracker/blob/main/docs/design.md).

**Snapshot semantics (fact).** Snapshots contain guest RAM plus KVM vCPU and
emulated-device state. Disks are external and are not automatically flushed
into the snapshot. Full and differential memory snapshots exist. Restore can
`MAP_PRIVATE` the memory file so clones share clean pages and diverge through
COW. Open vsock and network connections do not survive reliably. See
[snapshot support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md).

**Control, storage, and networking (fact).** Firecracker maps guest AF_VSOCK
ports to host Unix sockets, making a custom agent straightforward. Root drives
are raw block devices. OCI extraction, read-only bases, writable overlays,
TAP/netns/NAT, and credentials are orchestration responsibilities. See
[vsock design](https://github.com/firecracker-microvm/firecracker/blob/main/docs/vsock.md).

**Cancellation and security (inference).** VM failure containment is excellent
when the jailer and one-VM-per-attempt model are used. Per-command process-tree
cancellation still belongs in Nanoeval's guest agent. Final containment should
kill the Firecracker cgroup.

**Fit.** Rust, Linux/KVM only, and an executable rather than a stable embeddable
library. It is the strongest canonical Linux backend and snapshot reference.

### Cloud Hypervisor

**Topology and control (fact).** One Rust VMM process, optional
vhost-user/virtiofsd helpers, and REST or D-Bus host control over Unix sockets.
Linux KVM is the relevant backend. It has Firecracker-derived hybrid vsock
support. See [vsock](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/vsock.md).

**Snapshots (fact).** A snapshot directory contains configuration, full guest
memory ranges, and component state including vCPUs and devices. Disks remain
external. Restore can eagerly copy memory or fault it on demand with
`userfaultfd`; network FDs must be supplied again. Snapshots are not guaranteed
compatible across Cloud Hypervisor versions. See
[snapshot and restore](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/snapshot_restore.md).

**Storage and isolation (fact).** Supports raw/qcow2 block, virtio-fs,
vhost-user, seccomp, and optional Landlock confinement. It has no OCI/rootfs
policy or credential broker. See [Landlock](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/landlock.md)
and [seccomp](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/seccomp.md).

**Fit.** Strong Linux VMM, Rust, and no macOS/HVF. It offers more devices and
configuration than Nanoeval needs. Apple Containerization's Linux backend now
makes it more relevant as an indirect dependency.

### crosvm

**Topology (fact).** crosvm's defining mechanism is process-per-device. The
main process owns vCPU/control responsibilities while device processes are
forked and constrained through Minijail, namespaces, pivot-root, capability
removal, and seccomp. See its
[architecture](https://github.com/google/crosvm/blob/main/ARCHITECTURE.md).

**Snapshots (fact).** Snapshotting freezes vCPUs and device backends and
serializes RAM, vCPU state, and supported-device state. Restore occurs while
creating a new VM. The project explicitly calls this highly experimental,
unsupported, and format-unstable. See
[snapshotting](https://github.com/google/crosvm/blob/main/docs/book/src/architecture/snapshotting.md).

**Control, storage, and networking (fact).** Host control uses internal Unix
sockets and a `crosvm_control` library. It supports raw/qcow2/sparse block,
virtio-fs/9p, TAP/slirp, and native vhost-vsock. OCI preparation and guest
execution protocols are out of scope. See
[vsock](https://github.com/google/crosvm/blob/main/docs/book/src/devices/vsock.md).

**Fit.** Rust and security-rich, but not HVF/macOS and not a ready evaluation
runner. Its device-process isolation is worth studying if Nanoeval grows a
larger device surface.

### rust-vmm

rust-vmm is an ecosystem of crates such as `vm-memory`, `linux-loader`,
`kvm-ioctls`, `vm-virtio`, `vhost`, and `seccompiler`, not a sandbox runtime.
See the [repository overview](https://github.com/rust-vmm/rust-vmm/blob/main/README.md).

It supplies no topology, snapshot format, rootfs policy, guest protocol,
networking policy, or cleanup contract. Firecracker, Cloud Hypervisor,
libkrun, and other VMMs consume its components. It is appropriate only if
Nanoeval eventually needs a custom minimal KVM VMM; choosing it means building
nearly everything reviewed here.

### Kata Containers

**Topology (fact).** Kata ordinarily places each Kubernetes pod inside a VM.
`containerd-shim-kata-v2` controls an external VMM or the Rust Dragonball VMM;
`kata-agent` inside the guest performs container lifecycle RPCs. Runtime 4.0
increasingly consolidates components in Rust. See the
[4.0 architecture](https://github.com/kata-containers/kata-containers/blob/main/docs/design/architecture_4.0/architecture.md).

**Snapshots and templates (fact).** Its QEMU template mode boots and pauses a
prepared VM, saves memory and device state, and lets new VMs share read-only
memory. Writable container disks remain independent. This is VM templating,
not general OCI workload checkpoint/restore. See
[runtime-rs templates](https://github.com/kata-containers/kata-containers/blob/main/docs/how-to/how-to-use-template-in-runtime-rs.md).

**Guest control and storage (fact).** Host-agent communication is ttrpc over
vsock/hybrid-vsock. The agent exposes create, exec, signal, wait, remove, and
destroy operations. Rootfs and volumes come from containerd snapshotters
through virtio-fs, block devices, Nydus, or EROFS. See the
[vsock design](https://github.com/kata-containers/kata-containers/blob/main/docs/design/VSocks.md)
and [agent RPC implementation](https://github.com/kata-containers/kata-containers/blob/main/src/agent/src/rpc.rs).

**Isolation and cleanup (fact).** Pod VM failure destroys its containers.
Within the guest, the agent and runtime explicitly manage container processes
and cgroups. Networking and secrets are inherited from Kubernetes/containerd.
Confidential Containers can add TDX/SEV-backed isolation.

**Fit.** Mature security and lifecycle reference, but much heavier than
Nanoeval and operationally coupled to containerd/CRI. Linux only.

### libkrun

**Topology (fact).** libkrun is an embeddable Rust VMM exposed through a C API.
VMM, vCPU, and device threads live in the embedding process.
`krun_start_enter` consumes the context, blocks for the VM lifetime, and
normally causes the process to exit with the guest workload's status. See the
[public API](https://github.com/libkrun/libkrun/blob/main/include/libkrun.h)
and [Rust implementation](https://github.com/libkrun/libkrun/blob/main/src/libkrun/src/lib.rs).

That makes Nanoeval's current in-main-process spike safe only as a bring-up
experiment. Production use needs a child worker/shim process, as BoxLite and
Microsandbox already use.

**Platform and control (fact).** KVM is supported on Linux and HVF on Apple
Silicon. libkrun provides block, virtio-fs, console, network, and vsock devices.
Vsock ports can be bridged to host Unix sockets. It does not provide a guest
supervisor or higher-level execution protocol. See the
[README and security limitations](https://github.com/libkrun/libkrun/blob/main/README.md).

**Storage, network, and security (fact).** Rootfs can be a virtio-fs directory
or block image. TSI proxies guest sockets through the VMM's host network
context. libkrun explicitly warns that the guest and VMM share a host security
context, virtio-fs does not protect unrelated host paths, and non-raw disk
metadata can reference host files. Nanoeval therefore must jail the shim,
validate images, restrict mounts, and constrain networking outside libkrun.

**Snapshot status on July 21, 2026 (fact).** Stable v1.19.4 has no memory
snapshot/restore. `main` is the unreleased libkrun 2.0 line.
[Issue #748](https://github.com/libkrun/libkrun/issues/748) and
[PR #762](https://github.com/libkrun/libkrun/pull/762) contain a working HVF
capture/restore prototype. The PR is open and blocked with changes requested,
although its CI checks pass. It captures RAM, vCPU/GIC, RTC, and virtio
transport/device state and resumes mid-workload. Current limitations include
HVF only, one vCPU, eager RAM loading instead of lazy COW, no nested
virtualization, incomplete virtio-fs state after I/O, and no GPU/TSI restore.

This is the most important new development for Nanoeval: libkrun snapshot
restore is plausible, but not yet a usable dependency contract.

### krunkit

krunkit is a macOS CLI wrapper around libkrun. It runs a configured VM in its
own process, handles entitlements/signing, status and shutdown plumbing,
block/virtio-fs/network configuration, and optional QEMU guest-agent
integration. See its [context lifecycle](https://github.com/libkrun/krunkit/blob/main/src/context.rs)
and [status/shutdown handling](https://github.com/libkrun/krunkit/blob/main/src/status.rs).

It has no OCI materializer, warm pool, memory snapshot, or multi-tenant policy.
Its value is concrete macOS packaging and shutdown behavior.

### krunvm

krunvm turns OCI images into libkrun VMs using Buildah. `create` invokes
Buildah; `start` mounts the Buildah container rootfs, maps volumes, configures
networking, and enters libkrun. The writable layer is Buildah/container
storage, not a Nanoeval-native materializer. See
[create](https://github.com/libkrun/krunvm/blob/main/src/commands/create.rs)
and [start](https://github.com/libkrun/krunvm/blob/main/src/commands/start.rs).

One krunvm process owns one VM. There is no guest supervisor, pool, snapshot,
command-cancellation protocol, or tenant-grade jailer. It remains a useful
minimal OCI/libkrun reference.

### E2B open infrastructure

**Topology (fact).** Each sandbox is a Firecracker process in its own cgroup
and network namespace. A Go orchestrator manages the node. The guest `envd`
agent exposes file and process operations over Connect RPC on the VM network
rather than vsock. See the
[architecture](https://github.com/e2b-dev/infra/blob/main/docs/ARCHITECTURE.md).

**Warm state and fork (fact).** E2B stores memory, Firecracker state, and rootfs
artifacts. Restore uses `userfaultfd` to fetch memory pages lazily. A read-only
template rootfs is overlaid by an in-process NBD COW layer; pauses export dirty
memory and rootfs differences. Its fork endpoint checkpoints a running sandbox
once, resumes the parent, and starts multiple independent forks from the
immutable snapshot. See the
[fork handler](https://github.com/e2b-dev/infra/blob/main/packages/api/internal/handlers/sandbox_fork.go)
and [rootfs/NBD implementation](https://github.com/e2b-dev/infra/blob/main/packages/orchestrator/pkg/sandbox/rootfs/nbd.go).

**Networking and credentials (fact).** Sandboxes receive netns/veth/TAP slots
and nftables rules. The system implements domain/IP egress policies and passes
authentication metadata through MMDS. Forks receive fresh identities and do
not blindly inherit every runtime credential.

**Failure containment (fact).** Firecracker is atomically placed into its
cgroup. Shutdown escalates SIGTERM to SIGKILL, and cgroup v2 `cgroup.kill`
removes descendants. See the
[Firecracker process](https://github.com/e2b-dev/infra/blob/main/packages/orchestrator/pkg/sandbox/fc/process.go)
and [cgroup manager](https://github.com/e2b-dev/infra/blob/main/packages/orchestrator/pkg/sandbox/cgroup/manager.go).

**Cancellation caveat (fact and inference).** `envd`'s command signal handler
calls `Process.Signal` on one PID. It does not obviously kill a command-specific
process group or cgroup. Child processes can therefore survive individual
command cancellation until the sandbox is destroyed. See the
[process signal implementation](https://github.com/e2b-dev/infra/blob/main/packages/envd/internal/services/process/handler/handler.go).

**Fit.** The strongest end-to-end design reference, but Linux/Firecracker/cloud
orchestration rather than an embeddable runner.

### Microsandbox

**Topology (fact).** Microsandbox is a Rust-first, daemonless SDK. Each sandbox
gets a subprocess that owns a libkrun-derived VMM and a guest `agentd` PID 1.
Its control channel uses a custom virtio-console shared ring and a host Unix
socket relay rather than vsock. See the
[VM runtime](https://github.com/superradcompany/microsandbox/blob/main/crates/runtime/lib/vm.rs)
and [relay](https://github.com/superradcompany/microsandbox/blob/main/crates/runtime/lib/relay.rs).

**Storage and snapshots (fact).** OCI images are represented as shared
read-only image data plus a private ext4 upper layer mounted through overlayfs.
Current snapshots are disk/filesystem snapshots of the writable layer, not
RAM/vCPU/device snapshots. Forks use reflink/sparse copying where available and
cold-boot independently. See the
[filesystem model](https://github.com/superradcompany/microsandbox/blob/main/docs/security/filesystem.mdx)
and [snapshot documentation](https://github.com/superradcompany/microsandbox/blob/main/docs/sandboxes/snapshots.mdx).

Memory snapshotting remains open in
[issue #250](https://github.com/superradcompany/microsandbox/issues/250),
although the maintainer now labels it WIP following the libkrun work.

**Networking and secrets (fact).** A host userspace network stack enforces
destination rules and blocks private, loopback, link-local, and metadata
targets by default. Optional TLS interception keeps real credentials on the
host and substitutes guest-visible placeholders only for approved
destinations. See [network isolation](https://github.com/superradcompany/microsandbox/blob/main/docs/security/network.mdx)
and [secrets](https://github.com/superradcompany/microsandbox/blob/main/docs/security/secrets.mdx).

**Cancellation and cleanup (fact).** Sessions create their own process groups.
Cancellation sends SIGTERM followed by SIGKILL to the group; disconnects kill
active sessions. Teardown kills and reaps remaining processes,
syncs/unmounts filesystems, and has a host-side hard fallback. See
[session management](https://github.com/superradcompany/microsandbox/blob/main/crates/agentd/lib/session.rs)
and [teardown](https://github.com/superradcompany/microsandbox/blob/main/crates/agentd/lib/teardown.rs).

**Security and platform (fact).** Hardware VM per sandbox, restricted device
model, and an unprivileged host process; Linux has stronger controls than
macOS. Rust, KVM/HVF, Apache-2.0.

**Performance (claim).** The project advertises sub-100-ms boots, but the
published figure is not a sufficiently specified independent benchmark.

**Fit.** This is the closest existing implementation to Nanoeval's
one-VM-per-attempt design. Nanoeval should benchmark it before building
equivalent plumbing from raw libkrun.

### BoxLite

**Topology (fact).** The Rust SDK/runtime spawns one `boxlite-shim` subprocess
per VM because `krun_start_enter` takes over the calling process. The shim is
constrained by a Linux jailer using namespaces, chroot/pivot, seccomp,
privilege dropping, cgroup v2, and Landlock/bubblewrap options. macOS uses
Seatbelt and rlimits. See the
[threat model](https://github.com/boxlite-ai/boxlite/blob/main/src/boxlite/src/jailer/THREAT_MODEL.md).

**Guest control (fact).** A guest agent exposes tonic/gRPC over a Unix-socket to
vsock bridge. The guest launches an OCI container through `libcontainer`, using
a pre-threaded zygote to avoid unsafe fork-after-Tokio behavior. See the
[VMM/vsock configuration](https://github.com/boxlite-ai/boxlite/blob/main/src/boxlite/src/vmm/krun/engine.rs).

**Snapshots (fact).** Current local snapshots quiesce the guest filesystem,
SIGSTOP the shim, move the live container QCOW2 disk into an immutable snapshot
path, create a COW child, and resume. This is disk-only: no RAM, vCPU, or device
state, and restore requires the box to be stopped. See the
[snapshot backend](https://github.com/boxlite-ai/boxlite/blob/main/src/boxlite/src/litebox/local_snapshot.rs)
and [snapshot manager](https://github.com/boxlite-ai/boxlite/blob/main/src/boxlite/src/litebox/snapshot_mgr.rs).
Maintainers describe memory-state snapshots as future work in
[issue #205](https://github.com/boxlite-ai/boxlite/issues/205).

**Networking and secrets (fact).** gvisor-tap-vsock/gvproxy supplies guest
networking, destination filtering, and TLS placeholder substitution. One
documented caveat is that enabled host-loopback access can bypass the normal
`allow_net` destination policy.

**Cancellation (fact).** Timeout handling escalates SIGTERM to SIGKILL, but
individual exec signaling targets one PID. Full OCI-container teardown invokes
the all-process kill path, and host VM teardown uses the cgroup. Pipe-mode child
trees deserve explicit Nanoeval testing. See the
[exec signal path](https://github.com/boxlite-ai/boxlite/blob/main/src/guest/src/service/exec/exec_handle.rs)
and [container kill](https://github.com/boxlite-ai/boxlite/blob/main/src/guest/src/container/kill.rs).

**Fit.** A highly relevant libkrun integration and jailer reference, though
still young.

### Apple Containerization

**Topology (fact).** Each Linux container runs in its own VM. On macOS the
project uses Virtualization.framework directly; on Linux it now runs one Cloud
Hypervisor subprocess per VM. `vminitd` is PID 1 and provides gRPC process,
signal, event, and I/O services over vsock/hybrid-vsock. See the
[architecture and backends](https://github.com/apple/containerization/blob/main/README.md).

**Storage (fact).** The library pulls OCI images, builds ext4 filesystems in
Swift, and can use a read-only root with a separate writable ext4 overlay. This
is a real Docker-free OCI path.

**Networking (fact).** macOS supports per-container IPs through
Virtualization.framework networking. Linux expects the caller to prepare
TAP/bridge/NAT plumbing. Registry credentials stay on the host, but the project
does not implement Gondolin/Microsandbox-style request-time credential
substitution.

**Snapshots (fact).** Pause/resume exists, but there is no persisted VM memory
snapshot or VM-fork mechanism in the checked source.

**Cleanup (fact).** `vminitd` acts as a subreaper. OCI processes live in
cgroups; teardown uses recursive `cgroup.kill`, and the host reaps Cloud
Hypervisor/virtiofsd helpers with escalation. See
[Linux process control](https://github.com/apple/containerization/blob/main/Sources/Containerization/LinuxProcess.swift)
and [guest cgroups](https://github.com/apple/containerization/blob/main/vminitd/Sources/Cgroup/Cgroup2Manager.swift).

**Fit.** The strongest "do not build the macOS container VM layer yourself"
alternative. Costs are Swift rather than Rust, macOS 26/Xcode 26, no saved-state
snapshots, and a less direct VMM API.

### Gondolin

**Topology and control (fact).** A TypeScript host library starts QEMU by
default or an experimental libkrun backend. Guest `sandboxd`, `sandboxfs`, SSH,
and ingress components communicate through virtio-serial, not vsock. See the
[architecture](https://github.com/earendil-works/gondolin/blob/main/docs/architecture.md)
and [backends](https://github.com/earendil-works/gondolin/blob/main/docs/backends.md).

**Snapshots and storage (fact).** Writable modes include memory, QCOW2 COW,
and read-only roots. Gondolin checkpoints are disk-only QCOW2 state: the source
VM stops, and resumed or cloned sandboxes boot fresh. RAM, processes, and
network connections are not captured. See
[snapshots](https://github.com/earendil-works/gondolin/blob/main/docs/snapshots.md).

**Network and credential isolation (fact).** This is Gondolin's standout
mechanic. The guest connects to a host userspace network stack rather than
ordinary NAT. The host classifies HTTP/TLS and selected TCP/SSH traffic,
applies allowlists, and substitutes host-held secret placeholders only for
permitted destinations. See the
[network design](https://github.com/earendil-works/gondolin/blob/main/docs/network.md).

**Cleanup and security (fact).** VM shutdown escalates SIGTERM to SIGKILL.
Individual exec control does not expose a complete process-group cancellation
protocol, so VM teardown is the reliable boundary. Its security document
explicitly excludes side channels, host same-account attacks, and some
denial-of-service threats. See the
[security model](https://github.com/earendil-works/gondolin/blob/main/docs/security.md).

**Fit.** Excellent agent-specific network, secret, and programmable-filesystem
reference. QEMU is currently more mature than its libkrun backend.

### Clone

**Topology (fact).** A new, approximately 25k-line Rust KVM VMM. Its daemon
starts one child VMM process per VM. A guest agent communicates through
userspace virtio-vsock. Linux/KVM only. See the
[README](https://github.com/unixshells/clone/blob/main/README.md).

**VM fork (fact).** It snapshots RAM, vCPU registers, KVM clock, irqchip/PIT,
and virtio-MMIO transport state. Fork maps the memory snapshot `MAP_PRIVATE`,
injects fresh entropy, MAC, IP, hostname, and vsock CID, resets transports, and
resumes without rebooting. See
[snapshot capture](https://github.com/unixshells/clone/blob/main/src/control/sync_server.rs)
and the [technical specification](https://github.com/unixshells/clone/blob/main/docs/SPEC.md).

**Storage and networking (fact).** Rootfs can be raw/qcow2, read-write, or a
read-only base with tmpfs/QCOW2 overlay. Networking is TAP/bridge/NAT; forked
VMs use a userspace virtio-net path. No credential broker is present.

**Cancellation and security (fact).** The guest agent's exec protocol runs one
synchronous command, has a fixed 30-second timeout, and kills the direct child
PID only. Jailer and seccomp modes are optional. See the
[guest agent](https://github.com/unixshells/clone/blob/main/crates/guest-agent/src/main.rs)
and [jailer](https://github.com/unixshells/clone/blob/main/src/control/jailer.rs).

**Performance (claim).** The repository reports under 20 ms for a minimal fork
and roughly 160 ms to exec in a forked 4 GB Ubuntu VM. Those are project
measurements on one named bare-metal host, not independent results.

**Fit (inference).** This is important to watch and benchmark, but its low
adoption, absence of a stable release, explicit service-restart hacks after
fork, and incomplete per-exec lifecycle make it research code rather than
Nanoeval's initial production substrate.

### Hyperlight

Hyperlight embeds minimal function guests rather than Linux VMs. There is no
general guest OS, filesystem, OCI runtime, or process tree. Host and guest
exchange typed calls through shared memory and FlatBuffers. See its
[security model](https://github.com/hyperlight-dev/hyperlight/blob/main/docs/security.md).

Snapshots persist guest memory and descriptor data including architecture,
hypervisor, CPU vendor, and vCPU registers in an OCI image layout. There are no
disks or normal virtio devices. See the
[snapshot format](https://github.com/hyperlight-dev/hyperlight/blob/main/docs/snapshot-oci-format.md).

Its `InterruptHandle` is an excellent cancellation reference: Linux
cancellation interrupts the vCPU's KVM ioctl with a dedicated signal and
carefully handles races. See
[cancellation](https://github.com/hyperlight-dev/hyperlight/blob/main/docs/cancellation.md).

Rust and Linux/Windows virtualization are supported; no macOS/HVF. It is not
directly suitable for Terminal-Bench, but Nanoeval should copy its explicit
out-of-band interruption model.

### gVisor KVM platform

gVisor's KVM platform remains relevant, but it is not a conventional VM. The
Sentry is a Go application kernel that also acts as the VMM, with no emulated
hardware or separate Linux guest. See the
[platform architecture](https://github.com/google/gvisor/blob/master/g3doc/architecture_guide/platforms.md).

A runsc sandbox normally comprises launcher, Sentry, and filesystem gofer
processes. OCI roots can use gofer/LISAFS or EROFS lower layers with in-memory
or host-backed upper overlays. Control is host Unix RPC, not vsock.

Checkpoint/restore serializes gVisor kernel state, application pages, and
filesystem state; it is not a vCPU/device/RAM image. Background restore and
leave-running workflows exist. Host-connected sockets are not transparently
preserved. See
[checkpoint and restore](https://github.com/google/gvisor/blob/master/g3doc/user_guide/checkpoint_restore.md).

Process cleanup is unusually good: `runsc kill` supports a PID, process group,
or all sandbox processes. See the
[kill implementation](https://github.com/google/gvisor/blob/master/runsc/cmd/kill.go).

It is mature, multi-tenant, Linux-only, and implemented in Go. Compatibility
gaps arise because gVisor reimplements Linux syscalls. It deserves a
Terminal-Bench compatibility experiment; if compatibility is high enough, it
may beat the complexity of maintaining a guest kernel plus VMM.

### vHive

vHive layers serverless orchestration over Firecracker,
firecracker-containerd, containerd, and Knative. Snapshots contain Firecracker
RAM/device state plus container-disk differences produced as patches. A
networking pool pre-creates namespaces and devices to reduce restore cost. See
the [snapshot design](https://github.com/vhive-serverless/vHive/blob/main/docs/snapshots.md)
and [network manager](https://github.com/vhive-serverless/vHive/blob/main/networking/networkManager.go).

Its source and issues expose important operational failure modes: remote
snapshots can restore a healthy-looking container whose disk later corrupts;
device renaming and container disk state require special handling; and network
namespace and device-mapper creation can dominate restore latency. The remote
snapshot problem remains open in
[issue #823](https://github.com/vhive-serverless/vHive/issues/823).

vHive is a useful warning against treating "Firecracker restored successfully"
as proof that the workload filesystem is consistent.

### Ignite

Ignite is archived and explicitly deprecated. It ran one Firecracker VM from
an OCI/Docker image, used device-mapper/ext4 storage, CNI networking, and
ordinary SSH/systemd in the guest. See its
[README](https://github.com/weaveworks/ignite/blob/main/README.md).

It has no current snapshot/fork implementation or specialized guest process
protocol. Its historical lesson is that OCI-backed microVM UX is approachable;
it should not be selected as a dependency.

### Unikraft and KraftKit

Unikraft combines the application and selected OS libraries into a
single-purpose unikernel. KraftKit builds, packages OCI artifacts, and launches
them through QEMU, Firecracker, or Xen. See
[Unikraft](https://github.com/unikraft/unikraft) and
[KraftKit](https://github.com/unikraft/kraftkit).

Rootfs inputs can become initrd/cpio/EROFS artifacts. Networking and volumes
are runtime/driver-specific. There is no general-purpose guest agent, arbitrary
Linux process tree, or mature local memory-fork mechanism in the inspected
paths.

The small attack surface is attractive, and killing the VM normally kills the
one application. However, arbitrary Terminal-Bench workloads expect a normal
Linux userspace, package manager, shell, dynamic binaries, and broad syscall
compatibility. Rebuilding every task as a unikernel conflicts with Nanoeval's
workload model.

## Decision matrix

Scores are relative to Nanoeval: **H** high, **M** medium, **L** low, and **--**
absent.

| Candidate | Linux | macOS | Saved RAM fork | OCI/rootfs | Guest process API | Tenant containment | Embed/Rust | Recommendation |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| Firecracker plus Nanoeval agent | H | -- | H, mature | Build it | Build it over vsock | H | Rust executable | Canonical Linux backend |
| Raw libkrun | H | H | Experimental PR only | Build it | Build it | M until jailed | H | Keep, but use worker subprocess |
| Microsandbox | H | H | Disk-only today | H | H | M/H, young | H | Benchmark immediately |
| BoxLite | H | H | Disk-only today | H | H, PID-tree caveat | H, good jailer | H | Benchmark immediately |
| Apple Containerization | M/H | H | -- | H | H | H, one VM/container | Swift | Best macOS alternative baseline |
| E2B infra | H | -- | H | H | H, cancellation caveat | H | Go/service | Copy architecture, not stack |
| Clone | H | -- | H, experimental | M | L/M | M, immature | Rust executable | Research benchmark |
| Cloud Hypervisor | H | -- | H | Build it | Build it | M/H | Rust executable | Secondary Linux VMM |
| crosvm | H | -- | L/experimental | Build it | Build it | H host-process isolation | Rust | Architecture reference |
| Kata | H | -- | M, QEMU templates | H through containerd | H | H | Much Rust | Too heavy; copy agent semantics |
| gVisor/KVM | H | -- | Application checkpoint | H | H | H | Go | Run compatibility experiment |
| Gondolin | H | H | Disk-only | M | M | M | TypeScript | Copy network and secrets |
| Hyperlight | H | -- | H for function guests | -- | Typed calls only | H | H | Not Terminal-Bench compatible |
| vHive | H | -- | H but fragile orchestration | containerd | M | M | Go | Research reference |
| Unikraft/KraftKit | H | Via QEMU | -- | Build-time OCI | L for arbitrary exec | H for compatible apps | C/Go | Poor workload fit |
| Ignite | H | -- | -- | M | SSH | Historical | Go | Do not adopt |

## Five mechanics Nanoeval should copy

1. **Separate base, memory state, and writable state.** Use immutable,
   content-addressed OCI lower layers, a private per-attempt writable
   disk/layer, and, where applicable, a separately versioned RAM/vCPU/device
   snapshot. E2B and Firecracker demonstrate why these cannot be treated as
   one artifact.

2. **Make the attempt a killable process envelope.** Every exec should get a
   process group and preferably a dedicated cgroup. Cancellation should be
   SIGTERM, deadline, `cgroup.kill`/SIGKILL, then wait for process exit and
   stdout/stderr EOF. Microsandbox is the strongest implementation; Hyperlight
   has the strongest vCPU-interrupt race handling.

3. **Keep credentials in the host network broker.** Copy Gondolin,
   Microsandbox, and BoxLite's placeholder substitution: task code receives a
   non-secret token and the host injects the real credential only for an
   approved hostname and protocol. Never copy broad host credentials into a
   warm worker.

4. **Use a small typed guest protocol.** At minimum: `prepare_attempt`, `exec`,
   `signal_process_group`, `wait`, `stream_output`, `collect_result`,
   `kill_attempt`, `verify_empty`, `reset`, and health/epoch reporting. Carry
   stable attempt and execution IDs so retries cannot attach to stale
   processes.

5. **Treat clone identity as mutable state.** Every restored VM needs new
   entropy/vmgenid, machine-identity policy, MAC/IP/vsock CID, credential token,
   monotonic-clock correction, and connection reset. Snapshot artifacts must be
   authenticated before restore. Clone and the libkrun snapshot RFC explicitly
   expose how easy this is to under-build.

## Five repositories and files to read deeply

1. [E2B architecture](https://github.com/e2b-dev/infra/blob/main/docs/ARCHITECTURE.md):
   the most complete integration of memory paging, disk COW, networking,
   tokens, and lifecycle.
2. [Microsandbox session management](https://github.com/superradcompany/microsandbox/blob/main/crates/agentd/lib/session.rs):
   directly applicable process groups, timeouts, streaming, and cancellation.
3. [Firecracker snapshot support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md):
   exact state boundary, COW restore, and network/vsock limitations.
4. [libkrun snapshot PR #762](https://github.com/libkrun/libkrun/pull/762)
   together with [libkrun.h](https://github.com/libkrun/libkrun/blob/main/include/libkrun.h):
   determines whether macOS warm restore becomes practical and exposes the
   process-takeover API problem.
5. [Gondolin network design](https://github.com/earendil-works/gondolin/blob/main/docs/network.md):
   the best coding-agent-specific network and credential isolation design.

Close runners-up are BoxLite's
[jailer threat model](https://github.com/boxlite-ai/boxlite/blob/main/src/boxlite/src/jailer/THREAT_MODEL.md)
and Apple's
[vminitd cgroup manager](https://github.com/apple/containerization/blob/main/vminitd/Sources/Cgroup/Cgroup2Manager.swift).

## Recommended experiments

### Common harness

Use the same kernel, architecture-specific OCI rootfs, guest-agent contract,
CPU/memory cap, and task set for every topology. Do not compare Apple Silicon
ARM64 numbers directly with x86_64 Linux scoring numbers.

Record:

- cold and warm p50/p95/p99 exec-ready latency;
- steady-state attempts per second at concurrency 1, 4, 16, and saturation;
- host RSS/PSS, major/minor faults, disk bytes written, and snapshot-cache hit
  rate;
- first-output and output-drain latency;
- cancellation-to-no-descendants latency;
- VM/worker crash recovery;
- cross-attempt file, PID, mount, network, environment, and credential leakage;
- result determinism and Terminal-Bench pass rate.

Use hostile lifecycle cases: fork bombs, double-fork daemons, ignored SIGTERM,
inherited stdout FDs, processes holding deleted working directories, mount
creation, loopback listeners, open network connections, OOM, guest-kernel
panic, and VMM crash.

### A. One VM per attempt

Test raw libkrun in a worker subprocess, Microsandbox, BoxLite, Apple
Containerization on macOS, and Firecracker on Linux.

Measure fresh VM boot plus exec and a pool of pre-created but unused VMs.
Verify that killing the VMM leaves no process, TAP, cgroup, socket, or writable
disk. This is the correctness and security baseline.

### B. One warm VM with many namespace-isolated attempts

Implement Nanoeval's proposed design with:

- immutable lower root plus fresh overlay;
- mount, PID, network, IPC, and UTS namespaces;
- per-attempt cgroup v2;
- trusted control namespace;
- mandatory process-group and cgroup cleanup;
- worker epoch and recycling.

Run both sequential and concurrent attempts. After each attempt, inspect
`/proc`, cgroups, mounts, Unix sockets, network listeners, open deleted files,
tmpfs, shared memory, and writable-layer hashes.

The most important gate is deliberately testing the shared guest-kernel
boundary. Even without a real kernel exploit, test privileged syscalls,
namespace escape attempts, kernel resource exhaustion, and attacks on the
trusted agent. If the threat model treats task code as malicious, this mode
cannot be considered equivalent to per-attempt VMs.

### C. Snapshot-restored VM per attempt

On Linux, use Firecracker first. On macOS, evaluate a temporary libkrun build
from PR #762 only as research. Test Clone separately as an independent KVM
implementation.

Take the snapshot only after the guest agent reports:

- filesystem sync/quiesce complete;
- no in-flight exec;
- device queues quiet;
- network connections intentionally closed;
- clone-safe initialization point reached.

Restore 100 to 1,000 clones and verify:

- unique entropy, machine identity, MAC/IP/CID, and access token;
- correct wall and monotonic time;
- no inherited sockets or RPC requests;
- clean overlay and immutable base;
- no device-ring stalls or lost interrupts;
- no corruption under snapshot-during-I/O tests;
- stable memory sharing as clones dirty pages.

Compare eager restore, demand-paged restore, and prefetched working-set restore
separately.

### Decision gates

- Choose warm shared workers only if they deliver a substantial throughput
  advantage after including reset and verification costs.
- Choose snapshot restore only if p95 restore plus identity reset beats fresh
  boot without increasing failure or corruption rates.
- Keep canonical scoring one-VM-per-attempt until the shared-worker threat
  model and contamination tests are explicitly accepted.

## What materially challenges the libkrun-first direction

1. **The direct-embedding model is unsafe for the runner process.** Nanoeval
   currently embeds libkrun directly and runs one VM to completion.
   `krun_start_enter` takes over the process and normally exits it. A production
   runner needs a subprocess boundary even if libkrun remains the VMM.

2. **Stable libkrun still lacks saved-state restore.** The new HVF
   implementation is promising, but it is an unmerged, review-blocked,
   single-vCPU, eager-memory prototype with device gaps. Nanoeval should not
   base scheduling or artifact formats on its provisional API.

3. **A warm multi-attempt VM discards per-attempt hardware isolation.**
   Namespace and cgroup isolation inside one guest is still shared-kernel
   isolation. One compromised attempt can attack the worker's kernel, agent,
   harness, and other attempts. That is the strongest reason to retain
   one-VM-per-attempt as canonical.

4. **libkrun is not a complete sandbox boundary.** TSI, virtio-fs, and disk
   image parsing happen in a VMM process with Nanoeval's host privileges unless
   separately jailed. BoxLite and Microsandbox demonstrate how much machinery
   is required around the VMM.

5. **Existing projects already implement much of Nanoeval's missing layer.**
   Microsandbox and BoxLite supply OCI images, guest agents, process RPCs,
   network policy, writable layers, jailers, and cleanup. Apple
   Containerization supplies a polished one-VM-per-container macOS API.
   Building directly on raw libkrun is justified only if Nanoeval's narrower
   scope produces materially better latency, simplicity, or correctness.

6. **Apple Silicon is not the canonical x86_64 scoring environment.** Native
   libkrun/HVF executes ARM64 guests. Terminal-Bench images or behavior that
   depend on x86_64 still require an x86_64 KVM scoring worker or explicit
   translation, with separate performance and fidelity results.

The resulting recommendation is **libkrun-compatible, not
libkrun-exclusive**: keep the shared Nanoeval guest protocol and artifact model
independent of the VMM; use libkrun for macOS and selected Linux paths;
establish Firecracker snapshot restore as the Linux reference; and benchmark
Microsandbox, BoxLite, Apple Containerization, and gVisor before implementing
their surrounding machinery from scratch.
