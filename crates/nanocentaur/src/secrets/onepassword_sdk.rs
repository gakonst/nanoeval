use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use extism::{
    CurrentPlugin, Function, Manifest, Plugin, PluginBuilder, UserData, Val, ValType, Wasm,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};

use super::{SecretError, SecretManager, SecretRef, onepassword_connect::OpReference};

pub const ONEPASSWORD_CORE_VERSION: &str = "v0.4.0";
pub const ONEPASSWORD_CORE_URL: &str =
    "https://raw.githubusercontent.com/1Password/onepassword-sdk-go/v0.4.0/internal/wasm/core.wasm";
pub const ONEPASSWORD_CORE_SHA256: &str =
    "ee73572134c6cda202703cfa41c9c9223180bd7affba88f749261ea277657099";
const ONEPASSWORD_SDK_VERSION: &str = "0040003";
const MAX_WASM_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RANDOM_BYTES: usize = 1024 * 1024;
const MAX_SECRET_BYTES: usize = 4 * 1024 * 1024;
const MAX_WASM_MEMORY_PAGES: u32 = 4_096;
const PLUGIN_CALL_TIMEOUT: Duration = Duration::from_secs(60);
const SDK_REQUEST_TIMEOUT: Duration = Duration::from_secs(65);
const SDK_REQUEST_CAPACITY: usize = 64;

/// Resolves service-account references in-process through 1Password's official
/// SDK core WASM and the Rust Extism runtime.
pub struct OnePasswordSdkSecretManager {
    sender: Option<mpsc::Sender<SdkRequest>>,
    worker: Option<std::thread::JoinHandle<()>>,
    request_timeout: Duration,
}

impl OnePasswordSdkSecretManager {
    /// Loads a pinned 1Password SDK core and initializes one reusable client.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is empty, the core is missing, too
    /// large, does not match the pinned digest, or cannot initialize.
    pub fn new(
        core_path: impl AsRef<Path>,
        token: impl AsRef<str>,
    ) -> Result<Self, OnePasswordSdkConfigError> {
        if token.as_ref().is_empty() {
            return Err(OnePasswordSdkConfigError::InvalidToken);
        }
        let metadata =
            std::fs::metadata(core_path.as_ref()).map_err(OnePasswordSdkConfigError::CoreIo)?;
        if !metadata.is_file() || metadata.len() > MAX_WASM_BYTES {
            return Err(OnePasswordSdkConfigError::InvalidCore);
        }
        let core = std::fs::read(core_path.as_ref()).map_err(OnePasswordSdkConfigError::CoreIo)?;
        let digest = format!("{:x}", Sha256::digest(&core));
        if digest != ONEPASSWORD_CORE_SHA256 {
            return Err(OnePasswordSdkConfigError::CoreDigest {
                expected: ONEPASSWORD_CORE_SHA256,
                actual: digest,
            });
        }
        let client = OnePasswordSdkClient::new(core, token.as_ref())?;
        let (sender, receiver) = mpsc::channel(SDK_REQUEST_CAPACITY);
        let worker = std::thread::Builder::new()
            .name("nanocentaur-onepassword".to_owned())
            .spawn(move || run_sdk_client(client, receiver))
            .map_err(OnePasswordSdkConfigError::ClientThread)?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            request_timeout: SDK_REQUEST_TIMEOUT,
        })
    }
}

impl Drop for OnePasswordSdkSecretManager {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            drop(worker.join());
        }
    }
}

#[async_trait]
impl SecretManager for OnePasswordSdkSecretManager {
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError> {
        OpReference::parse(&reference.key).map_err(|()| SecretError::InvalidReference {
            provider: reference.provider.clone(),
            key: reference.key.clone(),
        })?;
        let (reply, response) = oneshot::channel();
        self.sender
            .as_ref()
            .ok_or_else(|| SecretError::Provider("1Password SDK client is unavailable".to_owned()))?
            .try_send(SdkRequest {
                reference: reference.key.clone(),
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    SecretError::Provider("1Password SDK client is overloaded".to_owned())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    SecretError::Provider("1Password SDK client is unavailable".to_owned())
                }
            })?;
        let provider = reference.provider.clone();
        let value = timeout(self.request_timeout, response)
            .await
            .map_err(|_| SecretError::Provider(format!("{provider} SDK request timed out")))?
            .map_err(|_| SecretError::Provider(format!("{provider} SDK task failed")))?
            .map_err(|()| SecretError::Provider("1Password SDK resolution failed".to_owned()))?;
        if value.is_empty() {
            return Err(SecretError::Provider(
                "1Password SDK resolved an empty value".to_owned(),
            ));
        }
        if value.len() > MAX_SECRET_BYTES {
            return Err(SecretError::Provider(
                "1Password SDK response is too large".to_owned(),
            ));
        }
        Ok(value)
    }
}

struct SdkRequest {
    reference: String,
    reply: oneshot::Sender<Result<String, ()>>,
}

fn run_sdk_client(mut client: OnePasswordSdkClient, mut receiver: mpsc::Receiver<SdkRequest>) {
    run_sdk_requests(&mut receiver, |reference| client.resolve(reference));
}

fn run_sdk_requests(
    receiver: &mut mpsc::Receiver<SdkRequest>,
    mut resolve: impl FnMut(&str) -> Result<String, ()>,
) {
    while let Some(request) = receiver.blocking_recv() {
        if request.reply.is_closed() {
            continue;
        }
        let result = resolve(&request.reference);
        drop(request.reply.send(result));
    }
}

struct OnePasswordSdkClient {
    plugin: Plugin,
    client_id: u64,
}

impl OnePasswordSdkClient {
    fn new(core: Vec<u8>, token: &str) -> Result<Self, OnePasswordSdkConfigError> {
        let manifest = Manifest::new([Wasm::data(core)])
            .with_memory_max(MAX_WASM_MEMORY_PAGES)
            .with_timeout(PLUGIN_CALL_TIMEOUT)
            .with_allowed_hosts(
                ["*.1password.com", "*.1password.ca", "*.1password.eu"]
                    .into_iter()
                    .map(str::to_owned),
            );
        let mut builder = PluginBuilder::new(manifest).with_wasi(true);
        for function in host_functions() {
            builder = builder.with_functions([function]);
        }
        let mut plugin = builder
            .build()
            .map_err(|_| OnePasswordSdkConfigError::CoreInitialization)?;
        let config = ClientConfig {
            service_account_token: token,
            programming_language: "Rust",
            sdk_version: ONEPASSWORD_SDK_VERSION,
            integration_name: "nanocentaur",
            integration_version: env!("CARGO_PKG_VERSION"),
            request_library_name: "Extism HTTP",
            request_library_version: env!("CARGO_PKG_VERSION"),
            os: normalized_os(),
            os_version: "0.0.0",
            architecture: normalized_architecture(),
        };
        let request = serde_json::to_vec(&config)
            .map_err(|_| OnePasswordSdkConfigError::CoreInitialization)?;
        let response = plugin
            .call::<_, String>("init_client", request)
            .map_err(|_| OnePasswordSdkConfigError::Authentication)?;
        let client_id = serde_json::from_str(&response)
            .map_err(|_| OnePasswordSdkConfigError::Authentication)?;
        Ok(Self { plugin, client_id })
    }

    fn resolve(&mut self, reference: &str) -> Result<String, ()> {
        let request = serde_json::to_vec(&Invocation {
            invocation: InvocationBody {
                client_id: self.client_id,
                parameters: InvocationParameters {
                    name: "SecretsResolve",
                    parameters: ResolveParameters {
                        secret_reference: reference,
                    },
                },
            },
        })
        .map_err(|_| ())?;
        let response = self
            .plugin
            .call::<_, String>("invoke", request)
            .map_err(|_| ())?;
        serde_json::from_str(&response).map_err(|_| ())
    }
}

impl Drop for OnePasswordSdkClient {
    fn drop(&mut self) {
        if let Ok(request) = serde_json::to_vec(&self.client_id) {
            drop(self.plugin.call::<_, Vec<u8>>("release_client", request));
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientConfig<'a> {
    service_account_token: &'a str,
    programming_language: &'static str,
    sdk_version: &'static str,
    integration_name: &'static str,
    integration_version: &'static str,
    request_library_name: &'static str,
    request_library_version: &'static str,
    os: &'static str,
    os_version: &'static str,
    architecture: &'static str,
}

#[derive(Serialize)]
struct Invocation<'a> {
    invocation: InvocationBody<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InvocationBody<'a> {
    client_id: u64,
    parameters: InvocationParameters<'a>,
}

#[derive(Serialize)]
struct InvocationParameters<'a> {
    name: &'static str,
    parameters: ResolveParameters<'a>,
}

#[derive(Serialize)]
struct ResolveParameters<'a> {
    secret_reference: &'a str,
}

fn host_functions() -> Vec<Function> {
    let random = Function::new(
        "random_fill_imported",
        [ValType::I32],
        [ValType::I64],
        UserData::new(()),
        |plugin: &mut CurrentPlugin, inputs: &[Val], outputs: &mut [Val], _: UserData<()>| {
            let length = usize::try_from(inputs[0].unwrap_i32()).unwrap_or(usize::MAX);
            if length > MAX_RANDOM_BYTES {
                return Err(extism::Error::msg(
                    "1Password core requested too much random data",
                ));
            }
            let mut bytes = vec![0; length];
            getrandom::fill(&mut bytes)
                .map_err(|_| extism::Error::msg("secure random source is unavailable"))?;
            let memory = plugin.memory_new(&bytes)?;
            outputs[0] = Val::I64(
                i64::try_from(memory.offset())
                    .map_err(|_| extism::Error::msg("WASM memory offset overflow"))?,
            );
            Ok(())
        },
    )
    .with_namespace("op-extism-core");
    vec![
        random,
        time_function("op-now"),
        time_function("zxcvbn"),
        Function::new(
            "utc_offset_seconds",
            [],
            [ValType::I64],
            UserData::new(()),
            |_plugin, _inputs, outputs, _user_data| {
                outputs[0] = Val::I64(i64::from(chrono::Local::now().offset().local_minus_utc()));
                Ok(())
            },
        )
        .with_namespace("op-time"),
    ]
}

fn time_function(namespace: &str) -> Function {
    Function::new(
        "unix_time_milliseconds_imported",
        [],
        [ValType::I64],
        UserData::new(()),
        |_plugin, _inputs, outputs, _user_data| {
            let milliseconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            outputs[0] = Val::I64(i64::try_from(milliseconds).unwrap_or(i64::MAX));
            Ok(())
        },
    )
    .with_namespace(namespace)
}

fn normalized_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        value => value,
    }
}

fn normalized_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        value => value,
    }
}

#[derive(Debug, Error)]
pub enum OnePasswordSdkConfigError {
    #[error("1Password service-account token must not be empty")]
    InvalidToken,
    #[error("1Password SDK core is missing, not a regular file, or too large")]
    InvalidCore,
    #[error("1Password SDK core could not be read")]
    CoreIo(#[source] std::io::Error),
    #[error("1Password SDK core digest mismatch: expected {expected}, got {actual}")]
    CoreDigest {
        expected: &'static str,
        actual: String,
    },
    #[error("1Password SDK core could not initialize")]
    CoreInitialization,
    #[error("1Password SDK rejected the service-account configuration")]
    Authentication,
    #[error("1Password SDK client thread could not start")]
    ClientThread(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    fn test_reference() -> SecretRef {
        SecretRef {
            provider: "1password".to_owned(),
            key: "op://vault/item/credential".to_owned(),
        }
    }

    #[test]
    fn rejects_empty_tokens_before_loading_the_core() {
        let Err(error) = OnePasswordSdkSecretManager::new("/missing", "") else {
            panic!("empty token must be rejected");
        };
        assert!(matches!(error, OnePasswordSdkConfigError::InvalidToken));
    }

    #[test]
    fn rejects_unpinned_core_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("core.wasm");
        std::fs::write(&path, b"not the pinned core").unwrap();
        let Err(error) = OnePasswordSdkSecretManager::new(path, "token") else {
            panic!("untrusted core must be rejected");
        };
        assert!(matches!(
            error,
            OnePasswordSdkConfigError::CoreDigest { .. }
        ));
    }

    #[test]
    fn exposes_exactly_the_official_core_host_functions() {
        let functions = host_functions();
        assert_eq!(functions.len(), 4);
        assert_eq!(
            functions
                .iter()
                .map(|function| (function.namespace(), function.name()))
                .collect::<Vec<_>>(),
            [
                (Some("op-extism-core"), "random_fill_imported"),
                (Some("op-now"), "unix_time_milliseconds_imported"),
                (Some("zxcvbn"), "unix_time_milliseconds_imported"),
                (Some("op-time"), "utc_offset_seconds"),
            ]
        );
    }

    #[tokio::test]
    async fn rejects_requests_when_the_bounded_worker_queue_is_full() {
        let (sender, _receiver) = mpsc::channel(1);
        let (reply, _response) = oneshot::channel();
        sender
            .try_send(SdkRequest {
                reference: test_reference().key,
                reply,
            })
            .unwrap();
        let manager = OnePasswordSdkSecretManager {
            sender: Some(sender),
            worker: None,
            request_timeout: Duration::from_secs(1),
        };

        let error = manager.resolve(&test_reference()).await.unwrap_err();
        assert!(matches!(error, SecretError::Provider(message) if message.contains("overloaded")));
    }

    #[tokio::test]
    async fn bounds_total_request_time_while_waiting_for_the_worker() {
        let (sender, _receiver) = mpsc::channel(1);
        let manager = OnePasswordSdkSecretManager {
            sender: Some(sender),
            worker: None,
            request_timeout: Duration::from_millis(10),
        };

        let error = manager.resolve(&test_reference()).await.unwrap_err();
        assert!(matches!(error, SecretError::Provider(message) if message.contains("timed out")));
    }

    #[test]
    fn worker_skips_requests_whose_callers_already_timed_out() {
        let (sender, mut receiver) = mpsc::channel(2);
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let worker = std::thread::spawn(move || {
            run_sdk_requests(&mut receiver, |_| {
                worker_calls.fetch_add(1, Ordering::Relaxed);
                Ok("resolved".to_owned())
            });
        });

        let (cancelled_reply, cancelled_response) = oneshot::channel();
        drop(cancelled_response);
        sender
            .blocking_send(SdkRequest {
                reference: test_reference().key,
                reply: cancelled_reply,
            })
            .unwrap();
        let (active_reply, active_response) = oneshot::channel();
        sender
            .blocking_send(SdkRequest {
                reference: test_reference().key,
                reply: active_reply,
            })
            .unwrap();
        drop(sender);

        assert_eq!(
            active_response.blocking_recv().unwrap().unwrap(),
            "resolved"
        );
        worker.join().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dropping_the_manager_joins_its_worker() {
        let (sender, mut receiver) = mpsc::channel(1);
        let exited = Arc::new(AtomicUsize::new(0));
        let worker_exited = Arc::clone(&exited);
        let worker = std::thread::spawn(move || {
            while receiver.blocking_recv().is_some() {}
            worker_exited.store(1, Ordering::Relaxed);
        });
        let manager = OnePasswordSdkSecretManager {
            sender: Some(sender),
            worker: Some(worker),
            request_timeout: Duration::from_secs(1),
        };

        drop(manager);
        assert_eq!(exited.load(Ordering::Relaxed), 1);
    }
}
