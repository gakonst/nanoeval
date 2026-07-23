use std::{
    ffi::{CString, NulError, OsStr, c_char},
    io,
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::PathBuf,
    ptr,
};

use thiserror::Error;

use crate::{BlockDevice, GuestCommand, Network, RootFilesystem, SharedDirectory, VmConfig};

const ROOT_TAG: &std::ffi::CStr = c"/dev/root";
const ROOT_BLOCK_ID: &std::ffi::CStr = c"root";
const ROOT_BLOCK_DEVICE: &std::ffi::CStr = c"/dev/vda";
const EXT4_FILESYSTEM: &std::ffi::CStr = c"ext4";
const ROOT_MOUNT_OPTIONS: &std::ffi::CStr = c"rw";
const TSI_HIJACK_INET: u32 = 1;

#[derive(Debug, Error)]
pub enum VmError {
    #[error("invalid VM configuration: {0}")]
    InvalidConfig(&'static str),

    #[error("failed to resolve root filesystem {path}: {source}")]
    ResolveRoot { path: PathBuf, source: io::Error },

    #[error("root filesystem is not a directory: {0}")]
    RootNotDirectory(PathBuf),

    #[error("root disk is not a file: {0}")]
    RootNotFile(PathBuf),

    #[error("block device is not a file: {0}")]
    BlockDeviceNotFile(PathBuf),

    #[error("shared directory is not a directory: {0}")]
    SharedDirectoryNotDirectory(PathBuf),

    #[error("{field} contains a NUL byte")]
    Nul {
        field: &'static str,
        source: NulError,
    },

    #[error("libkrun {operation} failed with errno {errno}")]
    Libkrun { operation: &'static str, errno: i32 },

    #[error("libkrun returned after starting the VM")]
    UnexpectedReturn,

    #[error("the libkrun context has already entered the VM")]
    ContextConsumed,
}

/// A configured libkrun VM which has not entered its blocking event loop yet.
///
/// This is the low-level VMM primitive. [`Self::run`] does not return after a
/// successful boot because libkrun owns the calling process until the guest
/// exits. `NanoVM`'s durable process-backed handle will build on this primitive.
pub struct KrunVm {
    context: Option<u32>,
}

impl KrunVm {
    /// Creates a libkrun configuration context.
    ///
    /// # Errors
    ///
    /// Returns an error when the root filesystem or VM configuration is
    /// invalid, or when libkrun rejects a requested device.
    pub fn new(config: &VmConfig) -> Result<Self, VmError> {
        if config.cpus_value() == 0 {
            return Err(VmError::InvalidConfig("CPU count must be nonzero"));
        }
        if config.memory_mib_value() == 0 {
            return Err(VmError::InvalidConfig("memory must be nonzero"));
        }

        let root = config
            .root()
            .canonicalize()
            .map_err(|source| VmError::ResolveRoot {
                path: config.root().to_path_buf(),
                source,
            })?;
        match config.root_filesystem() {
            RootFilesystem::Directory(_) if !root.is_dir() => {
                return Err(VmError::RootNotDirectory(root));
            }
            RootFilesystem::Ext4(_) if !root.is_file() => {
                return Err(VmError::RootNotFile(root));
            }
            RootFilesystem::Directory(_) | RootFilesystem::Ext4(_) => {}
        }

        let context = positive_context(krun::krun_create_ctx(), "create context")?;
        let vm = Self {
            context: Some(context),
        };

        check(
            krun::krun_set_vm_config(context, config.cpus_value(), config.memory_mib_value()),
            "configure VM",
        )?;

        let root = c_string(root.as_os_str(), "root filesystem path")?;
        match config.root_filesystem() {
            RootFilesystem::Directory(_) => {
                // SAFETY: both C strings live through the call and libkrun copies their
                // contents into the context before returning.
                check(
                    unsafe {
                        krun::krun_add_virtiofs3(
                            context,
                            ROOT_TAG.as_ptr(),
                            root.as_ptr(),
                            0,
                            false,
                        )
                    },
                    "attach root filesystem",
                )?;
            }
            RootFilesystem::Ext4(_) => {
                // SAFETY: the path and block ID remain alive through each call;
                // libkrun copies them into its context before returning.
                check(
                    unsafe {
                        krun::krun_add_disk(context, ROOT_BLOCK_ID.as_ptr(), root.as_ptr(), false)
                    },
                    "attach root disk",
                )?;
                // SAFETY: all strings are static and NUL terminated.
                check(
                    unsafe {
                        krun::krun_set_root_disk_remount(
                            context,
                            ROOT_BLOCK_DEVICE.as_ptr(),
                            EXT4_FILESYSTEM.as_ptr(),
                            ROOT_MOUNT_OPTIONS.as_ptr(),
                        )
                    },
                    "select root disk",
                )?;
            }
        }

        attach_block_devices(context, config.block_devices())?;
        attach_shared_directories(context, config.shared_directories())?;

        let stdin = io::stdin();
        let stdout = io::stdout();
        let stderr = io::stderr();
        // SAFETY: the standard descriptors remain owned by this process for
        // the VM lifetime; libkrun duplicates the descriptors it needs.
        check(
            unsafe {
                krun::krun_add_virtio_console_default(
                    context,
                    stdin.as_raw_fd(),
                    stdout.as_raw_fd(),
                    stderr.as_raw_fd(),
                )
            },
            "attach console",
        )?;

        if config.network_value() == Network::Internet {
            check(
                krun::krun_add_vsock(context, TSI_HIJACK_INET),
                "enable TSI networking",
            )?;
        }
        check(
            krun::krun_split_irqchip(context, false),
            "configure interrupt controller",
        )?;

        Ok(vm)
    }

    /// Returns a thread-safe out-of-band pause/resume capability.
    ///
    /// # Errors
    ///
    /// Returns an error after this context has entered the VMM loop.
    pub fn control(&self) -> Result<KrunVmControl, VmError> {
        self.context
            .map(|context| KrunVmControl { context })
            .ok_or(VmError::ContextConsumed)
    }

    /// Configures the guest command and enters libkrun's blocking VMM loop.
    ///
    /// On successful boot this function does not return: libkrun exits the VMM
    /// process when the guest shuts down. Call this only in a dedicated VMM
    /// process.
    ///
    /// # Errors
    ///
    /// Returns an error when a command value contains a NUL byte, libkrun
    /// rejects the command, or the VMM loop unexpectedly returns.
    pub fn run(mut self, command: &GuestCommand) -> Result<(), VmError> {
        let executable = c_string(command.program().as_os_str(), "guest executable")?;
        let arguments = command
            .arguments()
            .iter()
            .map(|argument| c_string(argument, "guest argument"))
            .collect::<Result<Vec<_>, _>>()?;
        let mut argument_pointers = arguments
            .iter()
            .map(|argument| argument.as_ptr())
            .collect::<Vec<_>>();
        argument_pointers.push(ptr::null());
        let environment = command
            .environment()
            .iter()
            .map(|(name, value)| {
                let mut entry = name.clone();
                entry.push("=");
                entry.push(value);
                c_string(&entry, "guest environment")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut environment_pointers = environment
            .iter()
            .map(|entry| entry.as_ptr())
            .collect::<Vec<_>>();
        environment_pointers.push(ptr::null::<c_char>());
        let context = self.context.ok_or(VmError::ContextConsumed)?;

        // SAFETY: executable, argument, and environment storage remains alive
        // through the call; each pointer list is NUL terminated. libkrun copies
        // the values into its owned context.
        check(
            unsafe {
                krun::krun_set_exec(
                    context,
                    executable.as_ptr(),
                    argument_pointers.as_ptr(),
                    environment_pointers.as_ptr(),
                )
            },
            "configure guest command",
        )?;
        let current_dir = c_string(
            command.current_directory().as_os_str(),
            "guest working directory",
        )?;
        // SAFETY: the C string remains valid for the call and libkrun copies it
        // into the context.
        check(
            unsafe { krun::krun_set_workdir(context, current_dir.as_ptr()) },
            "configure guest working directory",
        )?;

        self.context = None;
        check(krun::krun_start_enter(context), "start VM")?;
        Err(VmError::UnexpectedReturn)
    }
}

fn attach_block_devices(context: u32, devices: &[BlockDevice]) -> Result<(), VmError> {
    for device in devices {
        let path = device
            .path()
            .canonicalize()
            .map_err(|source| VmError::ResolveRoot {
                path: device.path().to_path_buf(),
                source,
            })?;
        if !path.is_file() {
            return Err(VmError::BlockDeviceNotFile(path));
        }
        let id = c_string(OsStr::new(device.id()), "block device ID")?;
        let path = c_string(path.as_os_str(), "block device path")?;
        // SAFETY: both strings remain valid through the call and libkrun
        // copies their contents into the context before returning.
        check(
            unsafe {
                krun::krun_add_disk(context, id.as_ptr(), path.as_ptr(), device.is_read_only())
            },
            "attach block device",
        )?;
    }
    Ok(())
}

fn attach_shared_directories(context: u32, directories: &[SharedDirectory]) -> Result<(), VmError> {
    for directory in directories {
        let path = directory
            .path()
            .canonicalize()
            .map_err(|source| VmError::ResolveRoot {
                path: directory.path().to_path_buf(),
                source,
            })?;
        if !path.is_dir() {
            return Err(VmError::SharedDirectoryNotDirectory(path));
        }
        let tag = c_string(OsStr::new(directory.tag()), "shared directory tag")?;
        let path = c_string(path.as_os_str(), "shared directory path")?;
        // SAFETY: both C strings remain valid through the call and libkrun
        // copies their contents into the context before returning.
        check(
            unsafe {
                krun::krun_add_virtiofs3(
                    context,
                    tag.as_ptr(),
                    path.as_ptr(),
                    0,
                    directory.is_read_only(),
                )
            },
            "attach shared directory",
        )?;
    }
    Ok(())
}

/// Out-of-band control for a VM running in libkrun's event loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KrunVmControl {
    context: u32,
}

impl KrunVmControl {
    /// Requests that every guest vCPU pause at an instruction boundary.
    ///
    /// libkrun currently implements this operation on macOS. The request is
    /// idempotent and completes asynchronously in the VMM event loop.
    ///
    /// # Errors
    ///
    /// Returns an OS error reported by libkrun, including unsupported-platform
    /// and not-yet-running errors.
    pub fn pause(self) -> Result<(), VmError> {
        check(krun::krun_vm_pause(self.context), "pause VM")
    }

    /// Resumes a VM previously paused with [`Self::pause`].
    ///
    /// # Errors
    ///
    /// Returns an OS error reported by libkrun.
    pub fn resume(self) -> Result<(), VmError> {
        check(krun::krun_vm_resume(self.context), "resume VM")
    }
}

impl Drop for KrunVm {
    fn drop(&mut self) {
        if let Some(context) = self.context.take() {
            let _ = krun::krun_free_ctx(context);
        }
    }
}

fn c_string(value: &OsStr, field: &'static str) -> Result<CString, VmError> {
    CString::new(value.as_bytes()).map_err(|source| VmError::Nul { field, source })
}

fn positive_context(status: i32, operation: &'static str) -> Result<u32, VmError> {
    u32::try_from(status).map_err(|_| VmError::Libkrun {
        operation,
        errno: status.saturating_neg(),
    })
}

fn check(status: i32, operation: &'static str) -> Result<(), VmError> {
    if status < 0 {
        Err(VmError::Libkrun {
            operation,
            errno: status.saturating_neg(),
        })
    } else {
        Ok(())
    }
}
