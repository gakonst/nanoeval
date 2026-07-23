use std::path::{Path, PathBuf};

/// Root filesystem exposed to one guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RootFilesystem {
    /// A host directory shared through virtiofs.
    Directory(PathBuf),
    /// A raw ext4 image attached as the guest's writable root block device.
    Ext4(PathBuf),
}

/// Network access supplied to the guest by libkrun.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Network {
    /// Do not attach a virtio-vsock device or proxy guest internet sockets.
    Disabled,
    /// Proxy guest internet sockets through libkrun TSI.
    #[default]
    Internet,
}

/// One additional block device attached after the root disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockDevice {
    id: String,
    path: PathBuf,
    read_only: bool,
}

impl BlockDevice {
    /// Creates a writable block device.
    pub fn read_write(id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            read_only: false,
        }
    }

    /// Creates an immutable block device.
    pub fn read_only(id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            read_only: true,
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// One narrowly scoped host directory exposed to the guest through virtiofs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SharedDirectory {
    tag: String,
    path: PathBuf,
    read_only: bool,
}

impl SharedDirectory {
    /// Creates a read-only share identified by `tag` inside the guest.
    pub fn read_only(tag: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            tag: tag.into(),
            path: path.into(),
            read_only: true,
        }
    }

    /// Creates a writable share identified by `tag` inside the guest.
    pub fn read_write(tag: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            tag: tag.into(),
            path: path.into(),
            read_only: false,
        }
    }

    #[must_use]
    pub fn tag(&self) -> &str {
        &self.tag
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// Immutable configuration for one libkrun VM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmConfig {
    root: RootFilesystem,
    cpus: u8,
    memory_mib: u32,
    network: Network,
    block_devices: Vec<BlockDevice>,
    shared_directories: Vec<SharedDirectory>,
}

impl VmConfig {
    /// Creates a VM configuration with two vCPUs, 1 GiB RAM, and internet
    /// socket proxying enabled.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: RootFilesystem::Directory(root.into()),
            cpus: 2,
            memory_mib: 1_024,
            network: Network::Internet,
            block_devices: Vec::new(),
            shared_directories: Vec::new(),
        }
    }

    /// Creates a VM backed by a raw ext4 root disk.
    pub fn ext4(root: impl Into<PathBuf>) -> Self {
        Self {
            root: RootFilesystem::Ext4(root.into()),
            cpus: 2,
            memory_mib: 1_024,
            network: Network::Internet,
            block_devices: Vec::new(),
            shared_directories: Vec::new(),
        }
    }

    #[must_use]
    pub fn cpus(mut self, cpus: u8) -> Self {
        self.cpus = cpus;
        self
    }

    #[must_use]
    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = memory_mib;
        self
    }

    #[must_use]
    pub fn network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }

    #[must_use]
    pub fn shared_directory(mut self, directory: SharedDirectory) -> Self {
        self.shared_directories.push(directory);
        self
    }

    #[must_use]
    pub fn block_device(mut self, device: BlockDevice) -> Self {
        self.block_devices.push(device);
        self
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        match &self.root {
            RootFilesystem::Directory(path) | RootFilesystem::Ext4(path) => path,
        }
    }

    #[must_use]
    pub const fn root_filesystem(&self) -> &RootFilesystem {
        &self.root
    }

    #[must_use]
    pub const fn cpus_value(&self) -> u8 {
        self.cpus
    }

    #[must_use]
    pub const fn memory_mib_value(&self) -> u32 {
        self.memory_mib
    }

    #[must_use]
    pub const fn network_value(&self) -> Network {
        self.network
    }

    #[must_use]
    pub fn shared_directories(&self) -> &[SharedDirectory] {
        &self.shared_directories
    }

    #[must_use]
    pub fn block_devices(&self) -> &[BlockDevice] {
        &self.block_devices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_suitable_for_a_small_worker() {
        let config = VmConfig::new("rootfs");

        assert_eq!(config.root(), Path::new("rootfs"));
        assert_eq!(
            config.root_filesystem(),
            &RootFilesystem::Directory(PathBuf::from("rootfs"))
        );
        assert_eq!(config.cpus_value(), 2);
        assert_eq!(config.memory_mib_value(), 1_024);
        assert_eq!(config.network_value(), Network::Internet);
    }

    #[test]
    fn policy_is_explicitly_overridable() {
        let config = VmConfig::new("rootfs")
            .cpus(8)
            .memory_mib(4_096)
            .network(Network::Disabled);

        assert_eq!(config.cpus_value(), 8);
        assert_eq!(config.memory_mib_value(), 4_096);
        assert_eq!(config.network_value(), Network::Disabled);
    }

    #[test]
    fn raw_ext4_is_an_explicit_root_kind() {
        let config = VmConfig::ext4("rootfs.ext4");

        assert_eq!(config.root(), Path::new("rootfs.ext4"));
        assert_eq!(
            config.root_filesystem(),
            &RootFilesystem::Ext4(PathBuf::from("rootfs.ext4"))
        );
    }

    #[test]
    fn read_only_shares_are_explicit() {
        let config = VmConfig::ext4("rootfs.ext4").shared_directory(SharedDirectory::read_only(
            "nanoeval-tools",
            "target/guest-tools",
        ));

        assert_eq!(
            config.shared_directories(),
            &[SharedDirectory::read_only(
                "nanoeval-tools",
                "target/guest-tools"
            )]
        );
        assert!(config.shared_directories()[0].is_read_only());
    }

    #[test]
    fn block_device_mutability_is_explicit() {
        let config = VmConfig::ext4("rootfs.ext4")
            .block_device(BlockDevice::read_write("cache", "cache.ext4"))
            .block_device(BlockDevice::read_only("runtime", "runtime.ext4"));

        assert!(!config.block_devices()[0].is_read_only());
        assert!(config.block_devices()[1].is_read_only());
    }

    #[test]
    fn writable_shares_are_explicit() {
        let directory = SharedDirectory::read_write("nanoeval-cache", "cache");

        assert_eq!(directory.tag(), "nanoeval-cache");
        assert_eq!(directory.path(), Path::new("cache"));
        assert!(!directory.is_read_only());
    }

    #[test]
    fn read_only_block_devices_are_explicit() {
        let config = VmConfig::ext4("rootfs.ext4")
            .block_device(BlockDevice::read_only("runtime", "runtime.ext4"));

        assert_eq!(
            config.block_devices(),
            &[BlockDevice::read_only("runtime", "runtime.ext4")]
        );
        assert!(config.block_devices()[0].is_read_only());
    }
}
