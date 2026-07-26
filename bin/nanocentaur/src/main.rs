#[cfg(feature = "mpp")]
mod mpp_gate;

use std::{
    collections::BTreeSet,
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use eyre::{Result, bail};
use futures_util::{StreamExt, stream};
use nanocentaur::{
    AdminAuthorizer, AgentManager, ApiState, AuthenticatedClient, CapabilityEgress, CapabilityName,
    CompositeSecretManager, ContentBlock, CreateAgent, CreateAgentResponse, CreateSecret,
    CreateTurn, EgressContext, EgressProvider, EnvironmentSecretManager, FileSecretManager,
    FreePaymentGate, ManagedAgentFactory, MockAgentFactory, NanocodexAgentFactory,
    ONEPASSWORD_CORE_SHA256, ONEPASSWORD_CORE_URL, ONEPASSWORD_CORE_VERSION,
    OnePasswordConnectSecretManager, OnePasswordSdkSecretManager, PaymentGate, PolicyStore,
    ProxyProfile, SecretDelivery, SecretGateway, SecretGuestConfig, SecretHttpMethod,
    SecretManager, SecretRef, SecretRequestRule, TurnDelivery, TurnStatus, TurnView, app,
    run_guest_command, run_vmm, run_vmm_command,
};
use nanocodex::OpenAiAuth;
use reqwest::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;

const MAX_ONEPASSWORD_CORE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Parser)]
#[command(name = "nanocentaur")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the hosted-agent REST API.
    Serve(Box<Serve>),
    /// Exercise N independent agents through the real HTTP/SSE API.
    Bench(Bench),
    /// Benchmark the real 1Password SDK provider without VM startup.
    BenchOnePassword(BenchOnePassword),
    /// Run a command in a VM through a real 1Password-backed secret egress lease.
    SmokeEgress(SmokeEgress),
    /// Download and verify the pinned 1Password SDK core.
    #[command(name = "onepassword-core")]
    OnePasswordCore(OnePasswordCore),
    #[command(hide = true)]
    Vmm {
        #[arg(long)]
        config: PathBuf,
    },
    #[command(hide = true)]
    OneShotVmm {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Args)]
struct Serve {
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    #[arg(long, env = "NANOCENTAUR_API_KEY")]
    api_key: String,
    #[arg(long, env = "NANOCENTAUR_ADMIN_TOKEN")]
    admin_token: String,
    #[arg(long, default_value = ".nanocentaur")]
    state_directory: PathBuf,
    #[arg(long, value_enum, default_value_t = Backend::Mock)]
    backend: Backend,
    #[arg(long, default_value_t = 25)]
    mock_delay_ms: u64,
    #[arg(long, requires = "openai_api_key")]
    rootfs: Option<PathBuf>,
    #[arg(long, env = "OPENAI_API_KEY", requires = "rootfs")]
    openai_api_key: Option<String>,
    #[arg(long = "allow-capability")]
    allowed_capabilities: Vec<String>,
    /// All non-tool capabilities use this server-owned proxy URL.
    #[arg(long)]
    egress_proxy_url: Option<String>,
    /// Resolve the proxy URL from `NANOCENTAUR_SECRET_<KEY>` on the host.
    #[arg(long, conflicts_with = "egress_proxy_url")]
    egress_proxy_secret: Option<String>,
    /// Host directory exposed as the `file` secret provider.
    #[arg(long)]
    secret_directory: Option<PathBuf>,
    /// 1Password Connect origin exposed as the `1password_connect` provider.
    #[arg(long, env = "OP_CONNECT_HOST", requires = "onepassword_connect_token")]
    onepassword_connect_host: Option<String>,
    /// 1Password Connect bearer token. Prefer the `OP_CONNECT_TOKEN` environment variable.
    #[arg(
        long,
        env = "OP_CONNECT_TOKEN",
        hide_env_values = true,
        requires = "onepassword_connect_host"
    )]
    onepassword_connect_token: Option<String>,
    /// 1Password service-account token exposed as the `1password` provider.
    #[arg(
        long,
        env = "OP_SERVICE_ACCOUNT_TOKEN",
        hide_env_values = true,
        requires = "onepassword_core_wasm"
    )]
    onepassword_service_account_token: Option<String>,
    /// Pinned 1Password SDK core WASM exposed as the `1password` provider.
    #[arg(
        long,
        env = "ONEPASSWORD_CORE_WASM",
        requires = "onepassword_service_account_token"
    )]
    onepassword_core_wasm: Option<PathBuf>,
    /// Parallel 1Password SDK clients. The core is compiled only once.
    #[arg(long, default_value_t = 3, requires = "onepassword_core_wasm")]
    onepassword_workers: usize,
    /// API base URL reachable from the guest VM.
    #[arg(long)]
    secret_gateway_url: Option<String>,
    #[arg(long, value_enum, default_value_t = Payments::Free)]
    payments: Payments,
    #[cfg(feature = "mpp")]
    #[command(flatten)]
    mpp: MppArgs,
}

#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    Mock,
    Nanocodex,
}

#[derive(Clone, Copy, ValueEnum)]
enum Payments {
    Free,
    Mpp,
}

#[cfg(feature = "mpp")]
#[derive(Args)]
struct MppArgs {
    #[arg(long = "mpp-rpc-url", env = "NANOCENTAUR_MPP_RPC_URL")]
    rpc_url: Option<String>,
    #[arg(long = "mpp-currency", env = "NANOCENTAUR_MPP_CURRENCY")]
    currency: Option<String>,
    #[arg(long = "mpp-recipient", env = "NANOCENTAUR_MPP_RECIPIENT")]
    recipient: Option<String>,
    #[arg(long = "mpp-escrow", env = "NANOCENTAUR_MPP_ESCROW")]
    escrow: Option<String>,
    #[arg(long = "mpp-chain-id", env = "NANOCENTAUR_MPP_CHAIN_ID")]
    chain_id: Option<u64>,
    #[arg(long = "mpp-close-key", env = "NANOCENTAUR_MPP_CLOSE_KEY")]
    close_key: Option<String>,
    #[arg(
        long = "mpp-challenge-secret",
        env = "NANOCENTAUR_MPP_CHALLENGE_SECRET"
    )]
    challenge_secret: Option<String>,
    #[arg(long = "mpp-unit-price", env = "NANOCENTAUR_MPP_UNIT_PRICE")]
    unit_price: Option<u128>,
    #[arg(
        long = "mpp-suggested-deposit",
        env = "NANOCENTAUR_MPP_SUGGESTED_DEPOSIT"
    )]
    suggested_deposit: Option<String>,
    #[arg(long = "mpp-fee-payer", default_value_t = false)]
    fee_payer: bool,
}

#[derive(Args)]
struct Bench {
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    url: String,
    #[arg(long, env = "NANOCENTAUR_API_KEY")]
    api_key: String,
    #[arg(long, default_value_t = 32)]
    agents: usize,
    #[arg(long)]
    concurrency: Option<usize>,
}

#[derive(Args)]
struct BenchOnePassword {
    /// Server-side reference to resolve. Repeat to benchmark distinct secrets.
    /// Resolved values are never printed.
    #[arg(long, required = true)]
    secret_reference: Vec<String>,
    #[arg(long, default_value_t = 20)]
    requests: usize,
    #[arg(long)]
    concurrency: Option<usize>,
    #[arg(long, env = "OP_SERVICE_ACCOUNT_TOKEN", hide_env_values = true)]
    onepassword_service_account_token: String,
    #[arg(long, env = "ONEPASSWORD_CORE_WASM")]
    onepassword_core_wasm: PathBuf,
    /// Host-owned persistent Wasmtime cache directory.
    #[arg(long)]
    cache_directory: Option<PathBuf>,
    /// Parallel SDK clients. The core is compiled only once.
    #[arg(long, default_value_t = 1)]
    workers: usize,
}

#[derive(Args)]
struct SmokeEgress {
    /// Prepared `NanoVM` rootfs containing the CLI to run.
    #[arg(long)]
    rootfs: PathBuf,
    /// Server-side 1Password reference; never mounted or passed into the VM.
    #[arg(long)]
    secret_reference: String,
    /// Exact HTTPS origin eligible to receive the injected secret.
    #[arg(long)]
    upstream: String,
    /// Header replaced when its value equals the placeholder.
    #[arg(long)]
    header: String,
    /// Non-secret value used by the guest CLI.
    #[arg(long)]
    placeholder: String,
    /// Allowed upstream path prefix.
    #[arg(long, default_value = "/")]
    path_prefix: String,
    #[arg(long, env = "OP_SERVICE_ACCOUNT_TOKEN", hide_env_values = true)]
    onepassword_service_account_token: String,
    #[arg(long, env = "ONEPASSWORD_CORE_WASM")]
    onepassword_core_wasm: PathBuf,
    /// Parallel SDK clients. One is sufficient for this single-secret smoke.
    #[arg(long, default_value_t = 1)]
    onepassword_workers: usize,
    /// Guest program followed by its arguments.
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct OnePasswordCore {
    #[arg(
        long,
        default_value = ".cache/onepassword/core-v0.4.0.wasm",
        env = "ONEPASSWORD_CORE_WASM"
    )]
    output: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let command = Cli::parse().command;
    if let Command::Vmm { config } = command {
        return run_vmm(&config).map_err(Into::into);
    }
    if let Command::OneShotVmm { config } = command {
        return run_vmm_command(&config).map_err(Into::into);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            match command {
                Command::Serve(arguments) => serve(*arguments).await,
                Command::Bench(arguments) => bench(arguments).await,
                Command::BenchOnePassword(arguments) => bench_onepassword(arguments).await,
                Command::SmokeEgress(arguments) => smoke_egress(arguments).await,
                Command::OnePasswordCore(arguments) => install_onepassword_core(arguments).await,
                Command::Vmm { .. } | Command::OneShotVmm { .. } => {
                    unreachable!("VMM mode returned before starting Tokio")
                }
            }
        })
}

async fn install_onepassword_core(arguments: OnePasswordCore) -> Result<()> {
    match std::fs::read(&arguments.output) {
        Ok(existing) => {
            if existing.len() <= MAX_ONEPASSWORD_CORE_BYTES
                && sha256_hex(&existing) == ONEPASSWORD_CORE_SHA256
            {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&OnePasswordCoreReport {
                        version: ONEPASSWORD_CORE_VERSION,
                        sha256: ONEPASSWORD_CORE_SHA256,
                        path: arguments.output,
                        downloaded: false,
                    })?
                );
                return Ok(());
            }
            bail!(
                "{} already exists but is not the pinned 1Password core",
                arguments.output.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let response = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get(ONEPASSWORD_CORE_URL)
        .send()
        .await?
        .error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ONEPASSWORD_CORE_BYTES as u64)
    {
        bail!("1Password core exceeds the download limit");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > MAX_ONEPASSWORD_CORE_BYTES {
            bail!("1Password core exceeds the download limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    let actual = sha256_hex(&bytes);
    if actual != ONEPASSWORD_CORE_SHA256 {
        bail!(
            "1Password core digest mismatch: expected {}, got {}",
            ONEPASSWORD_CORE_SHA256,
            actual
        );
    }

    let parent = arguments
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(&arguments.output)
        .map_err(|error| error.error)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&OnePasswordCoreReport {
            version: ONEPASSWORD_CORE_VERSION,
            sha256: ONEPASSWORD_CORE_SHA256,
            path: arguments.output,
            downloaded: true,
        })?
    );
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Serialize)]
struct OnePasswordCoreReport {
    version: &'static str,
    sha256: &'static str,
    path: PathBuf,
    downloaded: bool,
}

async fn smoke_egress(arguments: SmokeEgress) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let policy = Arc::new(PolicyStore::open(directory.path().join("policy.sqlite"))?);
    policy.bootstrap(
        "smoke",
        "Egress smoke test",
        "unused-smoke-key",
        "smoke",
        [],
    )?;
    policy.create_secret(CreateSecret {
        id: Some("smoke".to_owned()),
        name: "Egress smoke test".to_owned(),
        source: SecretRef {
            provider: "1password".to_owned(),
            key: arguments.secret_reference,
        },
        upstream: arguments.upstream,
        rules: vec![SecretRequestRule {
            methods: BTreeSet::from([SecretHttpMethod::Get]),
            path_prefixes: vec![arguments.path_prefix],
        }],
        delivery: SecretDelivery::ReplaceHeader {
            header: arguments.header,
            placeholder: arguments.placeholder.clone(),
        },
        guest: SecretGuestConfig {
            base_url_env: "NANOCENTAUR_SMOKE_BASE_URL".to_owned(),
            placeholder_env: Some(arguments.placeholder),
        },
    })?;
    policy.set_principal_secret("smoke", "smoke", true)?;
    let (identity, _) = policy.create_or_resolve_agent(
        &AuthenticatedClient {
            id: "smoke".to_owned(),
            default_principal_id: "smoke".to_owned(),
        },
        Some("egress-smoke"),
    )?;
    let manager: Arc<dyn SecretManager> = Arc::new(CompositeSecretManager::new().provider(
        "1password",
        Arc::new(
            OnePasswordSdkSecretManager::with_cache_directory_and_workers(
                &arguments.onepassword_core_wasm,
                arguments.onepassword_service_account_token,
                onepassword_cache_directory(&arguments.onepassword_core_wasm),
                arguments.onepassword_workers,
            )?,
        ),
    ));
    let gateway = SecretGateway::new(
        Arc::clone(&policy),
        manager,
        Arc::new(CapabilityEgress::new()),
        "http://127.0.0.1",
    )?;
    let lease = gateway
        .acquire(
            &EgressContext {
                agent_id: identity.id,
                principal: "smoke".to_owned(),
            },
            &BTreeSet::new(),
        )
        .await?;
    let output = run_guest_command(
        std::env::current_exe()?,
        arguments.rootfs,
        &lease,
        arguments.command,
    )
    .await?;
    if !output.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    if !output.status.success() {
        bail!("guest command failed with {}", output.status);
    }
    Ok(())
}

async fn serve(arguments: Serve) -> Result<()> {
    let capabilities = arguments
        .allowed_capabilities
        .iter()
        .map(|name| CapabilityName::new(name.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    let proxy_url = match (
        arguments.egress_proxy_url.clone(),
        arguments.egress_proxy_secret.clone(),
    ) {
        (Some(url), None) => Some(url),
        (None, Some(key)) => EnvironmentSecretManager::new("NANOCENTAUR_SECRET_")
            .resolve(&SecretRef {
                provider: "environment".to_owned(),
                key,
            })
            .await
            .map(Some)?,
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap rejects conflicting proxy options"),
    };
    let proxy = proxy_url
        .map(|url| ProxyProfile::new("configured", url).map(Arc::new))
        .transpose()?;
    let mut egress = CapabilityEgress::new();
    for capability in capabilities
        .iter()
        .filter(|capability| !capability.as_str().starts_with("tools."))
    {
        egress = if let Some(profile) = &proxy {
            egress.proxy(capability.clone(), Arc::clone(profile))
        } else {
            egress.direct(capability.clone())
        };
    }

    let payments = payment_gate(&arguments)?;
    let policy = Arc::new(PolicyStore::open(
        arguments.state_directory.join("policy.sqlite"),
    )?);
    policy.bootstrap(
        "standalone",
        "Standalone API client",
        &arguments.api_key,
        "standalone",
        capabilities,
    )?;
    let secrets = secret_gateway(&arguments, Arc::clone(&policy), egress)?;
    let managed_egress: Arc<dyn EgressProvider> = secrets.clone();

    let factory: Arc<dyn ManagedAgentFactory> = match arguments.backend {
        Backend::Mock => Arc::new(MockAgentFactory::new(Duration::from_millis(
            arguments.mock_delay_ms,
        ))),
        Backend::Nanocodex => {
            let rootfs = arguments
                .rootfs
                .clone()
                .ok_or_else(|| eyre::eyre!("--rootfs is required for nanocodex backend"))?;
            let api_key = arguments
                .openai_api_key
                .clone()
                .ok_or_else(|| eyre::eyre!("--openai-api-key is required for nanocodex backend"))?;
            Arc::new(NanocodexAgentFactory::new(
                OpenAiAuth::api_key(api_key),
                std::env::current_exe()?,
                rootfs,
                &arguments.state_directory,
                managed_egress,
            ))
        }
    };
    let manager = Arc::new(AgentManager::new(factory, &arguments.state_directory)?);
    let state = Arc::new(ApiState {
        manager,
        policy,
        admin: Arc::new(AdminAuthorizer::new(&arguments.admin_token)?),
        payments,
        secrets,
    });
    let listener = tokio::net::TcpListener::bind(arguments.listen).await?;
    tracing::info!(address = %arguments.listen, "nanocentaur listening");
    axum::serve(listener, app(state)).await?;
    Ok(())
}

fn secret_gateway(
    arguments: &Serve,
    policy: Arc<PolicyStore>,
    egress: CapabilityEgress,
) -> Result<Arc<SecretGateway>> {
    let mut manager = CompositeSecretManager::new().provider(
        "environment",
        Arc::new(EnvironmentSecretManager::new("NANOCENTAUR_SECRET_")),
    );
    if let Some(directory) = &arguments.secret_directory {
        manager = manager.provider("file", Arc::new(FileSecretManager::new(directory)?));
    }
    if let (Some(host), Some(token)) = (
        &arguments.onepassword_connect_host,
        &arguments.onepassword_connect_token,
    ) {
        manager = manager.provider(
            "1password_connect",
            Arc::new(OnePasswordConnectSecretManager::new(host, token)?),
        );
    }
    if let (Some(token), Some(core)) = (
        &arguments.onepassword_service_account_token,
        &arguments.onepassword_core_wasm,
    ) {
        manager = manager.provider(
            "1password",
            Arc::new(
                OnePasswordSdkSecretManager::with_cache_directory_and_workers(
                    core,
                    token,
                    arguments.state_directory.join("onepassword-wasmtime-cache"),
                    arguments.onepassword_workers,
                )?,
            ),
        );
    }
    let public_url = arguments
        .secret_gateway_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", arguments.listen));
    let egress: Arc<dyn EgressProvider> = Arc::new(egress);
    Ok(Arc::new(SecretGateway::new(
        policy,
        Arc::new(manager),
        egress,
        &public_url,
    )?))
}

fn onepassword_cache_directory(core: &std::path::Path) -> PathBuf {
    core.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("wasmtime-cache")
}

fn payment_gate(arguments: &Serve) -> Result<Arc<dyn PaymentGate>> {
    match arguments.payments {
        Payments::Free => Ok(Arc::new(FreePaymentGate)),
        Payments::Mpp => {
            #[cfg(feature = "mpp")]
            {
                use mpp_gate::{MppGateConfig, MppSessionGate};
                let required = |value: &Option<String>, name: &str| {
                    value
                        .clone()
                        .ok_or_else(|| eyre::eyre!("{name} is required with --payments mpp"))
                };
                let gate = MppSessionGate::new(MppGateConfig {
                    rpc_url: required(&arguments.mpp.rpc_url, "--mpp-rpc-url")?,
                    currency: required(&arguments.mpp.currency, "--mpp-currency")?,
                    recipient: required(&arguments.mpp.recipient, "--mpp-recipient")?,
                    escrow_contract: required(&arguments.mpp.escrow, "--mpp-escrow")?,
                    chain_id: arguments
                        .mpp
                        .chain_id
                        .ok_or_else(|| eyre::eyre!("--mpp-chain-id is required"))?,
                    close_key: required(&arguments.mpp.close_key, "--mpp-close-key")?,
                    challenge_secret: required(
                        &arguments.mpp.challenge_secret,
                        "--mpp-challenge-secret",
                    )?,
                    unit_price: arguments
                        .mpp
                        .unit_price
                        .ok_or_else(|| eyre::eyre!("--mpp-unit-price is required"))?,
                    suggested_deposit: arguments.mpp.suggested_deposit.clone(),
                    fee_payer: arguments.mpp.fee_payer,
                })?;
                Ok(Arc::new(gate))
            }
            #[cfg(not(feature = "mpp"))]
            {
                bail!("rebuild with `--features mpp` to enable MPP sessions")
            }
        }
    }
}

async fn bench(arguments: Bench) -> Result<()> {
    if arguments.agents == 0 {
        bail!("--agents must be greater than zero");
    }
    let concurrency = arguments.concurrency.unwrap_or(arguments.agents).max(1);
    let client = Client::new();
    let started = Instant::now();
    let results = stream::iter(0..arguments.agents)
        .map(|index| {
            benchmark_agent(
                client.clone(),
                arguments.url.clone(),
                arguments.api_key.clone(),
                index,
            )
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    let elapsed = started.elapsed();
    let mut latencies = results
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|duration| duration.as_secs_f64() * 1_000.0)
        .collect::<Vec<_>>();
    latencies.sort_by(f64::total_cmp);
    let output = BenchmarkReport {
        agents: arguments.agents,
        concurrency,
        wall_ms: elapsed.as_secs_f64() * 1_000.0,
        throughput_agents_per_second: count_as_f64(arguments.agents) / elapsed.as_secs_f64(),
        latency_ms: LatencyReport {
            p50: percentile(&latencies, 50, 100),
            p95: percentile(&latencies, 95, 100),
            max: latencies.last().copied().unwrap_or_default(),
        },
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

async fn bench_onepassword(arguments: BenchOnePassword) -> Result<()> {
    if arguments.requests == 0 {
        bail!("--requests must be greater than zero");
    }
    let concurrency = arguments
        .concurrency
        .unwrap_or(arguments.requests)
        .clamp(1, arguments.requests);
    let cache_directory = arguments
        .cache_directory
        .unwrap_or_else(|| onepassword_cache_directory(&arguments.onepassword_core_wasm));
    let initialization_started = Instant::now();
    let manager = Arc::new(
        OnePasswordSdkSecretManager::with_cache_directory_and_workers(
            arguments.onepassword_core_wasm,
            arguments.onepassword_service_account_token,
            cache_directory,
            arguments.workers,
        )?,
    );
    let initialization = initialization_started.elapsed();
    let references = arguments
        .secret_reference
        .into_iter()
        .map(|key| SecretRef {
            provider: "1password".to_owned(),
            key,
        })
        .collect::<Vec<_>>();
    let warmup_started = Instant::now();
    for reference in &references {
        drop(manager.resolve(reference).await?);
    }
    let warmup = warmup_started.elapsed();

    let started = Instant::now();
    let results = stream::iter(0..arguments.requests)
        .map(|index| {
            let manager = Arc::clone(&manager);
            let reference = references[index % references.len()].clone();
            async move {
                let started = Instant::now();
                drop(manager.resolve(&reference).await?);
                Ok::<Duration, nanocentaur::SecretError>(started.elapsed())
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    let elapsed = started.elapsed();
    let mut latencies = results
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|duration| duration.as_secs_f64() * 1_000.0)
        .collect::<Vec<_>>();
    latencies.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::to_string_pretty(&OnePasswordBenchmarkReport {
            initialization_ms: initialization.as_secs_f64() * 1_000.0,
            warmup_ms: warmup.as_secs_f64() * 1_000.0,
            workers: arguments.workers,
            distinct_references: references.len(),
            logical_resolutions: arguments.requests,
            concurrency,
            wall_ms: elapsed.as_secs_f64() * 1_000.0,
            logical_resolutions_per_second: count_as_f64(arguments.requests)
                / elapsed.as_secs_f64(),
            latency_ms: LatencyReport {
                p50: percentile(&latencies, 50, 100),
                p95: percentile(&latencies, 95, 100),
                max: latencies.last().copied().unwrap_or_default(),
            },
        })?
    );
    Ok(())
}

async fn benchmark_agent(
    client: Client,
    base_url: String,
    api_key: String,
    index: usize,
) -> Result<Duration> {
    let started = Instant::now();
    let agent: CreateAgentResponse = client
        .post(format!("{base_url}/v1/agent/new"))
        .header("x-api-key", &api_key)
        .json(&CreateAgent {
            context_key: Some(format!("benchmark:{index}")),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let agent_id = agent.agent_id;
    let turn: nanocentaur::TurnActionResponse = client
        .post(format!("{base_url}/v1/agent/{agent_id}/turn"))
        .header("x-api-key", &api_key)
        .header("idempotency-key", format!("benchmark-{index}"))
        .json(&CreateTurn {
            delivery: TurnDelivery::Steer,
            content: vec![ContentBlock::Text {
                text: format!("ping {index}"),
            }],
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let turn_id = turn.turn_id;
    loop {
        let terminal: TurnView = client
            .get(format!("{base_url}/v1/agent/{agent_id}/turn/{turn_id}"))
            .header("x-api-key", &api_key)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        match terminal.state {
            TurnStatus::Completed => break,
            TurnStatus::Failed | TurnStatus::Cancelled => {
                bail!("agent {index} ended in {:?}", terminal.state);
            }
            TurnStatus::Queued | TurnStatus::Running => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
    Ok(started.elapsed())
}

#[derive(Serialize)]
struct BenchmarkReport {
    agents: usize,
    concurrency: usize,
    wall_ms: f64,
    throughput_agents_per_second: f64,
    latency_ms: LatencyReport,
}

#[derive(Serialize)]
struct OnePasswordBenchmarkReport {
    initialization_ms: f64,
    warmup_ms: f64,
    workers: usize,
    distinct_references: usize,
    logical_resolutions: usize,
    concurrency: usize,
    wall_ms: f64,
    logical_resolutions_per_second: f64,
    latency_ms: LatencyReport,
}

#[derive(Serialize)]
struct LatencyReport {
    p50: f64,
    p95: f64,
    max: f64,
}

fn percentile(sorted: &[f64], numerator: usize, denominator: usize) -> f64 {
    let last = sorted.len().saturating_sub(1);
    let index = last
        .saturating_mul(numerator)
        .saturating_add(denominator / 2)
        / denominator;
    sorted.get(index).copied().unwrap_or_default()
}

fn count_as_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}
