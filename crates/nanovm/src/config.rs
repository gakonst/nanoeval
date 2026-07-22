use std::path::{Path, PathBuf};

/// Network access supplied to the guest by libkrun.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Network {
    /// Do not attach a virtio-vsock device or proxy guest internet sockets.
    Disabled,
    /// Proxy guest internet sockets through libkrun TSI.
    #[default]
    Internet,
}

/// Immutable configuration for one libkrun VM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmConfig {
    root: PathBuf,
    cpus: u8,
    memory_mib: u32,
    network: Network,
}

impl VmConfig {
    /// Creates a VM configuration with two vCPUs, 1 GiB RAM, and internet
    /// socket proxying enabled.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cpus: 2,
            memory_mib: 1_024,
            network: Network::Internet,
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
    pub fn root(&self) -> &Path {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_suitable_for_a_small_worker() {
        let config = VmConfig::new("rootfs");

        assert_eq!(config.root(), Path::new("rootfs"));
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
}
