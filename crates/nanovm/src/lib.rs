#![allow(
    unsafe_code,
    reason = "the libkrun binding requires a small audited FFI boundary"
)]

mod capabilities;
mod command;
mod config;
mod egress;
mod gvproxy;
mod krun;
mod process;

pub use capabilities::{Capabilities, KrunFeature};
pub use command::GuestCommand;
pub use config::{BlockDevice, Network, RootFilesystem, SharedDirectory, VmConfig};
pub use egress::{EgressError, EgressLease, EgressMount};
pub use gvproxy::{Gvproxy, GvproxyError};
pub use krun::{KrunVm, KrunVmControl, VmError};
pub use process::{PrivateVmProcessConfig, VmProcessConfig, VmProcessError};

/// The complete upstream libkrun API pinned by this workspace's lockfile.
///
/// Prefer `NanoVM`'s typed API. This escape hatch makes new or specialized
/// libkrun functionality available without waiting for a typed wrapper.
pub use ::krun as raw;
