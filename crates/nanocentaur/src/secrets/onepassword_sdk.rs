use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    io::Write,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use extism::{
    CompiledPlugin, CurrentPlugin, Function, Manifest, Plugin, PluginBuilder, UserData, Val,
    ValType, Wasm,
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
const MAX_SDK_WORKERS: usize = 8;

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
        Self::build(core_path.as_ref(), token.as_ref(), None, 1)
    }

    /// Loads the pinned core with a persistent host-owned Wasmtime compilation
    /// cache.
    ///
    /// # Errors
    ///
    /// Returns an error when the cache directory cannot be created securely,
    /// or when core verification or client initialization fails.
    pub fn with_cache_directory(
        core_path: impl AsRef<Path>,
        token: impl AsRef<str>,
        cache_directory: impl AsRef<Path>,
    ) -> Result<Self, OnePasswordSdkConfigError> {
        Self::with_cache_directory_and_workers(core_path, token, cache_directory, 1)
    }

    /// Loads the pinned core once, then creates a bounded pool of independent
    /// SDK clients from that compiled module. Identical concurrent references
    /// are coalesced globally while distinct references can resolve in
    /// parallel.
    ///
    /// # Errors
    ///
    /// Returns an error when `workers` is zero or exceeds the defensive limit,
    /// when the cache directory cannot be created securely, or when any SDK
    /// client cannot initialize.
    pub fn with_cache_directory_and_workers(
        core_path: impl AsRef<Path>,
        token: impl AsRef<str>,
        cache_directory: impl AsRef<Path>,
        workers: usize,
    ) -> Result<Self, OnePasswordSdkConfigError> {
        Self::build(
            core_path.as_ref(),
            token.as_ref(),
            Some(cache_directory.as_ref()),
            workers,
        )
    }

    fn build(
        core_path: &Path,
        token: &str,
        cache_directory: Option<&Path>,
        workers: usize,
    ) -> Result<Self, OnePasswordSdkConfigError> {
        if token.is_empty() {
            return Err(OnePasswordSdkConfigError::InvalidToken);
        }
        if !(1..=MAX_SDK_WORKERS).contains(&workers) {
            return Err(OnePasswordSdkConfigError::InvalidWorkerCount {
                maximum: MAX_SDK_WORKERS,
            });
        }
        let metadata = std::fs::metadata(core_path).map_err(OnePasswordSdkConfigError::CoreIo)?;
        if !metadata.is_file() || metadata.len() > MAX_WASM_BYTES {
            return Err(OnePasswordSdkConfigError::InvalidCore);
        }
        let core = std::fs::read(core_path).map_err(OnePasswordSdkConfigError::CoreIo)?;
        let digest = format!("{:x}", Sha256::digest(&core));
        if digest != ONEPASSWORD_CORE_SHA256 {
            return Err(OnePasswordSdkConfigError::CoreDigest {
                expected: ONEPASSWORD_CORE_SHA256,
                actual: digest,
            });
        }
        let cache_config = cache_directory.map(prepare_cache_config).transpose()?;
        let compiled = OnePasswordSdkClient::compile(
            core,
            cache_config.as_ref().map(tempfile::NamedTempFile::path),
        )?;
        let (sender, receiver) = mpsc::channel(SDK_REQUEST_CAPACITY);
        let (ready, initialized) = std::sync::mpsc::sync_channel(1);
        let token = token.to_owned();
        let worker = std::thread::Builder::new()
            .name("nanocentaur-onepassword-pool".to_owned())
            .spawn(move || {
                run_sdk_pool(&Arc::new(compiled), token, workers, receiver, &ready);
            })
            .map_err(OnePasswordSdkConfigError::ClientThread)?;
        match initialized.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                drop(sender);
                drop(worker.join());
                return Err(error);
            }
            Err(_) => {
                drop(sender);
                drop(worker.join());
                return Err(OnePasswordSdkConfigError::CoreInitialization);
            }
        }
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

struct SdkJob {
    reference: String,
}

enum SdkWorkerEvent {
    Ready {
        worker: usize,
    },
    Failed {
        worker: usize,
        error: OnePasswordSdkConfigError,
    },
    Completed {
        worker: usize,
        reference: String,
        result: Result<String, ()>,
    },
}

fn run_sdk_pool(
    compiled: &Arc<CompiledPlugin>,
    token: String,
    worker_count: usize,
    receiver: mpsc::Receiver<SdkRequest>,
    ready: &std::sync::mpsc::SyncSender<Result<(), OnePasswordSdkConfigError>>,
) {
    let (events, mut worker_events) = mpsc::unbounded_channel();
    let mut senders = Vec::with_capacity(worker_count);
    let mut workers = Vec::with_capacity(worker_count);
    let mut startup_error = None;
    let mut idle = Vec::new();
    for index in 0..worker_count {
        let (sender, jobs) = std::sync::mpsc::sync_channel(1);
        let compiled = Arc::clone(compiled);
        let token = token.clone();
        let events = events.clone();
        match std::thread::Builder::new()
            .name(format!("nanocentaur-onepassword-{index}"))
            .spawn(move || {
                let client = OnePasswordSdkClient::from_compiled(&compiled, &token);
                drop(token);
                match client {
                    Ok(client) => {
                        if events.send(SdkWorkerEvent::Ready { worker: index }).is_ok() {
                            run_sdk_worker(index, client, &jobs, &events);
                        }
                    }
                    Err(error) => drop(events.send(SdkWorkerEvent::Failed {
                        worker: index,
                        error,
                    })),
                }
            }) {
            Ok(worker) => {
                senders.push(sender);
                workers.push(worker);
                // Bring the first usable client online without competing with
                // extra plugin instantiations. Once it is ready, the remaining
                // workers can initialize concurrently in the background.
                if idle.is_empty()
                    && let Some(worker) =
                        wait_for_initial_sdk_worker(&mut worker_events, &mut startup_error)
                {
                    idle.push(worker);
                }
            }
            Err(error) => {
                startup_error = Some(OnePasswordSdkConfigError::ClientThread(error));
                break;
            }
        }
    }
    drop(events);
    drop(token);
    if workers.len() != worker_count {
        startup_error.get_or_insert(OnePasswordSdkConfigError::CoreInitialization);
    }
    if let Some(error) = startup_error {
        drop(senders);
        for worker in workers {
            drop(worker.join());
        }
        drop(ready.send(Err(error)));
        return;
    }
    if idle.is_empty() {
        let error = startup_error.unwrap_or(OnePasswordSdkConfigError::CoreInitialization);
        drop(senders);
        for worker in workers {
            drop(worker.join());
        }
        drop(ready.send(Err(error)));
        return;
    }
    if ready.send(Ok(())).is_err() {
        drop(senders);
        for worker in workers {
            drop(worker.join());
        }
        return;
    }

    let runtime = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "could not start 1Password SDK dispatcher");
            drop(senders);
            for worker in workers {
                drop(worker.join());
            }
            return;
        }
    };
    runtime.block_on(run_sdk_dispatcher(receiver, &senders, worker_events, idle));
    drop(senders);
    for worker in workers {
        drop(worker.join());
    }
}

fn wait_for_initial_sdk_worker(
    worker_events: &mut mpsc::UnboundedReceiver<SdkWorkerEvent>,
    startup_error: &mut Option<OnePasswordSdkConfigError>,
) -> Option<usize> {
    match worker_events.blocking_recv() {
        Some(SdkWorkerEvent::Ready { worker }) => Some(worker),
        Some(SdkWorkerEvent::Failed { worker, error }) => {
            tracing::warn!(worker, %error, "1Password SDK worker could not initialize");
            startup_error.get_or_insert(error);
            None
        }
        Some(SdkWorkerEvent::Completed { .. }) => {
            unreachable!("a worker cannot complete a job before becoming ready");
        }
        None => {
            startup_error.get_or_insert(OnePasswordSdkConfigError::CoreInitialization);
            None
        }
    }
}

fn run_sdk_worker(
    index: usize,
    mut client: OnePasswordSdkClient,
    jobs: &std::sync::mpsc::Receiver<SdkJob>,
    events: &mpsc::UnboundedSender<SdkWorkerEvent>,
) {
    while let Ok(job) = jobs.recv() {
        let result = client.resolve(&job.reference);
        if events
            .send(SdkWorkerEvent::Completed {
                worker: index,
                reference: job.reference,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}

async fn run_sdk_dispatcher(
    mut receiver: mpsc::Receiver<SdkRequest>,
    workers: &[std::sync::mpsc::SyncSender<SdkJob>],
    mut worker_events: mpsc::UnboundedReceiver<SdkWorkerEvent>,
    mut idle: Vec<usize>,
) {
    let mut waiting: HashMap<String, Vec<oneshot::Sender<Result<String, ()>>>> = HashMap::new();
    let mut queued = VecDeque::new();
    let mut accepting = true;
    let mut workers_available = true;

    loop {
        dispatch_sdk_jobs(workers, &mut waiting, &mut queued, &mut idle);
        if (!accepting || !workers_available) && waiting.is_empty() {
            break;
        }
        tokio::select! {
            request = receiver.recv(), if accepting => {
                match request {
                    Some(request) if !request.reply.is_closed() => {
                        match waiting.entry(request.reference) {
                            Entry::Occupied(mut entry) => entry.get_mut().push(request.reply),
                            Entry::Vacant(entry) => {
                                queued.push_back(entry.key().clone());
                                entry.insert(vec![request.reply]);
                            }
                        }
                    }
                    Some(_) => {}
                    None => accepting = false,
                }
            }
            event = worker_events.recv(), if workers_available => {
                match event {
                    Some(SdkWorkerEvent::Ready { worker }) => idle.push(worker),
                    Some(SdkWorkerEvent::Failed { worker, error }) => {
                        tracing::warn!(worker, %error, "1Password SDK worker could not initialize");
                    }
                    Some(SdkWorkerEvent::Completed { worker, reference, result }) => {
                        idle.push(worker);
                        if let Some(replies) = waiting.remove(&reference) {
                            for reply in replies {
                                drop(reply.send(result.clone()));
                            }
                        }
                    }
                    None => {
                    workers_available = false;
                    fail_waiting_requests(&mut waiting);
                    queued.clear();
                    }
                }
            }
        }
    }
}

fn dispatch_sdk_jobs(
    workers: &[std::sync::mpsc::SyncSender<SdkJob>],
    waiting: &mut HashMap<String, Vec<oneshot::Sender<Result<String, ()>>>>,
    queued: &mut VecDeque<String>,
    idle: &mut Vec<usize>,
) {
    while !idle.is_empty() && !queued.is_empty() {
        let worker = idle.pop().expect("idle worker checked above");
        let reference = queued.pop_front().expect("queued job checked above");
        let Some(replies) = waiting.get_mut(&reference) else {
            idle.push(worker);
            continue;
        };
        replies.retain(|reply| !reply.is_closed());
        if replies.is_empty() {
            waiting.remove(&reference);
            idle.push(worker);
            continue;
        }
        if workers[worker]
            .try_send(SdkJob {
                reference: reference.clone(),
            })
            .is_err()
            && let Some(replies) = waiting.remove(&reference)
        {
            for reply in replies {
                drop(reply.send(Err(())));
            }
        }
    }
}

fn fail_waiting_requests(waiting: &mut HashMap<String, Vec<oneshot::Sender<Result<String, ()>>>>) {
    for (_, replies) in waiting.drain() {
        for reply in replies {
            drop(reply.send(Err(())));
        }
    }
}

#[cfg(test)]
fn run_sdk_requests(
    receiver: &mut mpsc::Receiver<SdkRequest>,
    mut resolve: impl FnMut(&str) -> Result<String, ()>,
) {
    let mut pending = VecDeque::new();
    while let Some(request) = pending.pop_front().or_else(|| receiver.blocking_recv()) {
        if request.reply.is_closed() {
            continue;
        }
        let reference = request.reference;
        let mut replies = vec![request.reply];
        drain_requests(receiver, &mut pending, &reference, &mut replies);
        let result = resolve(&reference);
        // Requests that arrived during the blocking SDK call share that exact
        // in-flight result. Nothing is retained after these replies are sent.
        drain_requests(receiver, &mut pending, &reference, &mut replies);
        for reply in replies {
            drop(reply.send(result.clone()));
        }
    }
}

#[cfg(test)]
fn drain_requests(
    receiver: &mut mpsc::Receiver<SdkRequest>,
    pending: &mut VecDeque<SdkRequest>,
    reference: &str,
    replies: &mut Vec<oneshot::Sender<Result<String, ()>>>,
) {
    while let Ok(request) = receiver.try_recv() {
        if request.reply.is_closed() {
            continue;
        }
        if request.reference == reference {
            replies.push(request.reply);
        } else {
            pending.push_back(request);
        }
    }
}

fn prepare_cache_config(
    cache_directory: &Path,
) -> Result<tempfile::NamedTempFile, OnePasswordSdkConfigError> {
    std::fs::create_dir_all(cache_directory).map_err(OnePasswordSdkConfigError::CacheIo)?;
    let cache_directory =
        std::fs::canonicalize(cache_directory).map_err(OnePasswordSdkConfigError::CacheIo)?;
    if !cache_directory.is_dir() {
        return Err(OnePasswordSdkConfigError::InvalidCacheDirectory);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata =
            std::fs::metadata(&cache_directory).map_err(OnePasswordSdkConfigError::CacheIo)?;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(OnePasswordSdkConfigError::InsecureCacheDirectory);
        }
    }
    let cache_directory = cache_directory
        .to_str()
        .ok_or(OnePasswordSdkConfigError::InvalidCacheDirectory)?;
    let mut config = tempfile::NamedTempFile::new_in(cache_directory)
        .map_err(OnePasswordSdkConfigError::CacheIo)?;
    writeln!(
        config,
        "[cache]\ndirectory = {}",
        serde_json::to_string(cache_directory)
            .map_err(|_| OnePasswordSdkConfigError::InvalidCacheDirectory)?
    )
    .map_err(OnePasswordSdkConfigError::CacheIo)?;
    config
        .as_file()
        .sync_all()
        .map_err(OnePasswordSdkConfigError::CacheIo)?;
    Ok(config)
}

struct OnePasswordSdkClient {
    plugin: Plugin,
    client_id: u64,
}

impl OnePasswordSdkClient {
    fn compile(
        core: Vec<u8>,
        cache_config: Option<&Path>,
    ) -> Result<CompiledPlugin, OnePasswordSdkConfigError> {
        let manifest = Manifest::new([Wasm::data(core)])
            .with_memory_max(MAX_WASM_MEMORY_PAGES)
            .with_timeout(PLUGIN_CALL_TIMEOUT)
            .with_allowed_hosts(
                ["*.1password.com", "*.1password.ca", "*.1password.eu"]
                    .into_iter()
                    .map(str::to_owned),
            );
        let mut builder = PluginBuilder::new(manifest).with_wasi(true);
        if let Some(cache_config) = cache_config {
            builder = builder.with_cache_config(cache_config);
        }
        for function in host_functions() {
            builder = builder.with_functions([function]);
        }
        builder
            .compile()
            .map_err(|_| OnePasswordSdkConfigError::CoreInitialization)
    }

    fn from_compiled(
        compiled: &CompiledPlugin,
        token: &str,
    ) -> Result<Self, OnePasswordSdkConfigError> {
        let mut plugin = Plugin::new_from_compiled(compiled)
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
    #[error("1Password SDK worker count must be between 1 and {maximum}")]
    InvalidWorkerCount { maximum: usize },
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
    #[error("1Password SDK cache directory is not a directory or valid UTF-8 path")]
    InvalidCacheDirectory,
    #[error("1Password SDK cache directory must not be group- or world-writable")]
    InsecureCacheDirectory,
    #[error("1Password SDK cache setup failed")]
    CacheIo(#[source] std::io::Error),
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
    fn rejects_unbounded_worker_counts_before_loading_the_core() {
        let Err(error) = OnePasswordSdkSecretManager::with_cache_directory_and_workers(
            "/missing",
            "token",
            "/missing-cache",
            MAX_SDK_WORKERS + 1,
        ) else {
            panic!("unbounded worker pool must be rejected");
        };
        assert!(matches!(
            error,
            OnePasswordSdkConfigError::InvalidWorkerCount { .. }
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
    fn worker_coalesces_concurrent_identical_resolutions_without_caching() {
        let (sender, mut receiver) = mpsc::channel(4);
        let mut responses = Vec::new();
        for reference in [
            "op://vault/item/a",
            "op://vault/item/a",
            "op://vault/item/b",
        ] {
            let (reply, response) = oneshot::channel();
            sender
                .blocking_send(SdkRequest {
                    reference: reference.to_owned(),
                    reply,
                })
                .unwrap();
            responses.push(response);
        }
        drop(sender);
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let worker = std::thread::spawn(move || {
            run_sdk_requests(&mut receiver, |reference| {
                worker_calls.fetch_add(1, Ordering::Relaxed);
                Ok(reference.to_owned())
            });
        });

        assert_eq!(
            responses.remove(0).blocking_recv().unwrap().unwrap(),
            "op://vault/item/a"
        );
        assert_eq!(
            responses.remove(0).blocking_recv().unwrap().unwrap(),
            "op://vault/item/a"
        );
        assert_eq!(
            responses.remove(0).blocking_recv().unwrap().unwrap(),
            "op://vault/item/b"
        );
        worker.join().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn dispatcher_coalesces_identical_references_and_parallelizes_distinct_ones() {
        let (requests, receiver) = mpsc::channel(4);
        let (events, worker_events) = mpsc::unbounded_channel();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        let mut handles = Vec::new();
        for index in 0..2 {
            let (sender, jobs) = std::sync::mpsc::sync_channel::<SdkJob>(1);
            workers.push(sender);
            let events = events.clone();
            let barrier = Arc::clone(&barrier);
            let calls = Arc::clone(&calls);
            handles.push(std::thread::spawn(move || {
                while let Ok(job) = jobs.recv() {
                    calls.fetch_add(1, Ordering::Relaxed);
                    barrier.wait();
                    events
                        .send(SdkWorkerEvent::Completed {
                            worker: index,
                            result: Ok(job.reference.clone()),
                            reference: job.reference,
                        })
                        .unwrap();
                }
            }));
        }
        drop(events);

        let mut responses = Vec::new();
        for reference in [
            "op://vault/item/a",
            "op://vault/item/a",
            "op://vault/item/b",
        ] {
            let (reply, response) = oneshot::channel();
            requests
                .try_send(SdkRequest {
                    reference: reference.to_owned(),
                    reply,
                })
                .unwrap();
            responses.push(response);
        }
        drop(requests);

        run_sdk_dispatcher(receiver, &workers, worker_events, vec![1, 0]).await;
        assert_eq!(
            responses.remove(0).await.unwrap().unwrap(),
            "op://vault/item/a"
        );
        assert_eq!(
            responses.remove(0).await.unwrap().unwrap(),
            "op://vault/item/a"
        );
        assert_eq!(
            responses.remove(0).await.unwrap().unwrap(),
            "op://vault/item/b"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        drop(workers);
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn creates_an_ephemeral_config_for_a_persistent_cache_directory() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("wasmtime");
        let config = prepare_cache_config(&cache).unwrap();
        let contents = std::fs::read_to_string(config.path()).unwrap();
        let canonical = std::fs::canonicalize(cache).unwrap();

        assert!(contents.starts_with("[cache]\ndirectory = "));
        assert!(contents.contains(canonical.to_str().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_cache_directory_writable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("wasmtime");
        std::fs::create_dir(&cache).unwrap();
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o777)).unwrap();

        let error = prepare_cache_config(&cache).unwrap_err();
        assert!(matches!(
            error,
            OnePasswordSdkConfigError::InsecureCacheDirectory
        ));
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
