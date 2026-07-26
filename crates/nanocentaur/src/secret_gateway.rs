use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
        header::{
            AUTHORIZATION, CONNECTION, CONTENT_LENGTH, HOST, PROXY_AUTHORIZATION, TRANSFER_ENCODING,
        },
    },
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::TryStreamExt;
use hudsucker::{
    Body as ProxyBody, HttpContext, HttpHandler, Proxy, RequestOrResponse,
    certificate_authority::CertificateAuthority,
    hyper::{Request as ProxyRequest, Response as ProxyResponse},
    rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
        KeyUsagePurpose,
    },
    rustls::{
        ServerConfig,
        crypto::{CryptoProvider, aws_lc_rs},
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};
use nanovm::Network;
use reqwest::redirect::Policy;
use serde::Serialize;
use tempfile::TempDir;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use url::Url;
use uuid::Uuid;

use crate::{
    CapabilityName, EgressContext, EgressError, EgressLease, EgressMount, EgressProvider,
    PolicyError, PolicyStore, SecretDelivery, SecretManager, SecretView,
};

const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const GUEST_CA_DIRECTORY: &str = "/run/nanocentaur-ca";
const GUEST_CA_PATH: &str = "/run/nanocentaur-ca/ca.pem";
const CA_MOUNT_TAG: &str = "nanocentaur-ca";

#[derive(Clone, Debug)]
struct LeaseIdentity {
    agent_id: String,
    principal_id: String,
}

#[derive(Clone, Debug)]
struct ClientBinding {
    token: String,
    identity: LeaseIdentity,
    authority: String,
}

struct LeaseGuard {
    token: String,
    leases: mpsc::UnboundedSender<LeaseCommand>,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        drop(self.leases.send(LeaseCommand::Remove {
            token: self.token.clone(),
        }));
    }
}

enum LeaseCommand {
    Insert {
        token: String,
        identity: LeaseIdentity,
    },
    Resolve {
        token: String,
        reply: oneshot::Sender<Option<LeaseIdentity>>,
    },
    BindClient {
        token: String,
        client: SocketAddr,
        authority: String,
        reply: oneshot::Sender<Option<LeaseIdentity>>,
    },
    ResolveClient {
        client: SocketAddr,
        reply: oneshot::Sender<Option<ClientBinding>>,
    },
    RemoveClient {
        client: SocketAddr,
    },
    Remove {
        token: String,
    },
}

/// Host-side credential gateway. The VM receives only a scoped route and
/// optional placeholder; secret material is resolved and injected on the host.
pub struct SecretGateway {
    policy: Arc<PolicyStore>,
    manager: Arc<dyn SecretManager>,
    inner: Arc<dyn EgressProvider>,
    public_base_url: Url,
    proxy_base_url: Url,
    client: reqwest::Client,
    leases: mpsc::UnboundedSender<LeaseCommand>,
    ca_directory: TempDir,
    proxy_stop: Option<oneshot::Sender<()>>,
}

impl SecretGateway {
    /// Creates a gateway whose public URL must be reachable from the guest VM.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL is not a safe absolute HTTP(S) URL or the
    /// redirect-disabled upstream client cannot be constructed, or there is
    /// no Tokio runtime in which to own the lease registry task.
    pub fn new(
        policy: Arc<PolicyStore>,
        manager: Arc<dyn SecretManager>,
        inner: Arc<dyn EgressProvider>,
        public_base_url: &str,
    ) -> Result<Self, SecretGatewayError> {
        let public_base_url =
            Url::parse(public_base_url).map_err(|_| SecretGatewayError::InvalidPublicUrl)?;
        if !matches!(public_base_url.scheme(), "http" | "https")
            || public_base_url.host_str().is_none()
            || !public_base_url.username().is_empty()
            || public_base_url.password().is_some()
            || public_base_url.query().is_some()
            || public_base_url.fragment().is_some()
        {
            return Err(SecretGatewayError::InvalidPublicUrl);
        }
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(SecretGatewayError::Client)?;
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| SecretGatewayError::NoRuntime)?;
        let (leases, receiver) = mpsc::unbounded_channel();
        runtime.spawn(run_lease_store(receiver));
        let (authority, ca_directory) = certificate_authority()?;
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").map_err(SecretGatewayError::ProxyBind)?;
        listener
            .set_nonblocking(true)
            .map_err(SecretGatewayError::ProxyBind)?;
        let proxy_port = listener
            .local_addr()
            .map_err(SecretGatewayError::ProxyBind)?
            .port();
        let listener =
            tokio::net::TcpListener::from_std(listener).map_err(SecretGatewayError::ProxyBind)?;
        let proxy_base_url = Url::parse(&format!("http://127.0.0.1:{proxy_port}"))
            .map_err(|_| SecretGatewayError::InvalidPublicUrl)?;
        let handler = TransparentProxyHandler {
            policy: Arc::clone(&policy),
            manager: Arc::clone(&manager),
            leases: leases.clone(),
        };
        let (proxy_stop, proxy_done) = oneshot::channel();
        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(authority)
            .with_rustls_client(aws_lc_rs::default_provider())
            .with_http_handler(handler)
            .with_graceful_shutdown(async move {
                drop(proxy_done.await);
            })
            .build()
            .map_err(SecretGatewayError::Proxy)?;
        runtime.spawn(async move {
            if let Err(error) = proxy.start().await {
                tracing::error!(%error, "transparent secret proxy stopped");
            }
        });
        Ok(Self {
            policy,
            manager,
            inner,
            public_base_url,
            proxy_base_url,
            client,
            leases,
            ca_directory,
            proxy_stop: Some(proxy_stop),
        })
    }

    /// Proxies one request through a lease-scoped secret route.
    pub async fn handle(
        &self,
        lease_token: &str,
        secret_id: &str,
        path: &str,
        request: Request,
    ) -> Response {
        match self.forward(lease_token, secret_id, path, request).await {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(
                    status = error.status().as_u16(),
                    kind = error.kind(),
                    "secret gateway request rejected"
                );
                error.into_response()
            }
        }
    }

    async fn forward(
        &self,
        lease_token: &str,
        secret_id: &str,
        path: &str,
        request: Request,
    ) -> Result<Response, SecretGatewayError> {
        let (reply, response) = oneshot::channel();
        self.leases
            .send(LeaseCommand::Resolve {
                token: lease_token.to_owned(),
                reply,
            })
            .map_err(|_| SecretGatewayError::Unavailable)?;
        let identity = response
            .await
            .map_err(|_| SecretGatewayError::Unavailable)?
            .ok_or(SecretGatewayError::InvalidLease)?;
        let secret = self
            .policy
            .agent_effective_secret(&identity.agent_id, &identity.principal_id, secret_id)
            .map_err(SecretGatewayError::Policy)?;
        if !allows_request(&secret, request.method(), path) {
            return Err(SecretGatewayError::RequestDenied);
        }

        let (parts, body) = request.into_parts();
        let body = to_bytes(body, MAX_REQUEST_BYTES)
            .await
            .map_err(|_| SecretGatewayError::RequestTooLarge)?;
        let destination = destination(&secret, path, parts.uri.query())?;
        let mut headers = filtered_request_headers(&parts.headers, &secret.delivery);
        let value = self
            .manager
            .resolve(&secret.source)
            .await
            .map_err(|_| SecretGatewayError::Resolution)?;
        apply_delivery(&mut headers, &secret.delivery, &value)?;

        let upstream = self
            .client
            .request(parts.method, destination)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|_| SecretGatewayError::Upstream)?;
        let status = upstream.status();
        let headers = filtered_response_headers(upstream.headers());
        let mut bytes = Vec::new();
        let mut stream = upstream.bytes_stream();
        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|_| SecretGatewayError::Upstream)?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(SecretGatewayError::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }

        let mut response = Response::new(Body::from(bytes));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        tracing::debug!(
            agent_id = %identity.agent_id,
            secret_id,
            status = status.as_u16(),
            "secret gateway request completed"
        );
        Ok(response)
    }

    fn lease_url(&self, token: &str, secret_id: &str) -> Result<String, SecretGatewayError> {
        let mut url = self.public_base_url.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| SecretGatewayError::InvalidPublicUrl)?;
            segments.pop_if_empty();
            segments.extend(["internal", "v1", "secret-egress", token, secret_id]);
        }
        Ok(url.into())
    }
}

impl Drop for SecretGateway {
    fn drop(&mut self) {
        if let Some(stop) = self.proxy_stop.take() {
            let _ = stop.send(());
        }
    }
}

struct HostMitmAuthority {
    signing_key: KeyPair,
    certificate: hudsucker::rcgen::Certificate,
    private_key: PrivateKeyDer<'static>,
    provider: Arc<CryptoProvider>,
}

impl CertificateAuthority for HostMitmAuthority {
    async fn gen_server_config(&self, authority: &axum::http::uri::Authority) -> Arc<ServerConfig> {
        let mut parameters = CertificateParams::new(vec![authority.host().to_owned()])
            .expect("validated HTTP authority is a valid certificate name");
        parameters.serial_number = Some(Uuid::new_v4().as_bytes().to_vec().into());
        parameters.use_authority_key_identifier_extension = true;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, authority.host());
        parameters.distinguished_name = name;
        let certificate = parameters
            .signed_by(&self.signing_key, &self.certificate, &self.signing_key)
            .expect("ephemeral authority can sign a leaf certificate");
        let mut server = ServerConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()
            .expect("AWS-LC supports safe default TLS versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certificate)],
                self.private_key.clone_key(),
            )
            .expect("generated leaf certificate and private key match");
        server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Arc::new(server)
    }
}

fn certificate_authority() -> Result<(HostMitmAuthority, TempDir), SecretGatewayError> {
    let signing_key = KeyPair::generate().map_err(SecretGatewayError::Certificate)?;
    let mut parameters =
        CertificateParams::new(Vec::<String>::new()).map_err(SecretGatewayError::Certificate)?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "NanoCentaur Runtime CA");
    parameters.distinguished_name = name;
    parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    parameters.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    parameters.use_authority_key_identifier_extension = true;
    let certificate = parameters
        .self_signed(&signing_key)
        .map_err(SecretGatewayError::Certificate)?;
    let directory = tempfile::tempdir().map_err(SecretGatewayError::CertificateIo)?;
    std::fs::write(directory.path().join("ca.pem"), certificate.pem())
        .map_err(SecretGatewayError::CertificateIo)?;
    let private_key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let authority = HostMitmAuthority {
        signing_key,
        certificate,
        private_key,
        provider: Arc::new(aws_lc_rs::default_provider()),
    };
    Ok((authority, directory))
}

#[derive(Clone)]
struct TransparentProxyHandler {
    policy: Arc<PolicyStore>,
    manager: Arc<dyn SecretManager>,
    leases: mpsc::UnboundedSender<LeaseCommand>,
}

impl HttpHandler for TransparentProxyHandler {
    async fn handle_request(
        &mut self,
        context: &HttpContext,
        mut request: ProxyRequest<ProxyBody>,
    ) -> RequestOrResponse {
        if request.method() == Method::CONNECT {
            return self.handle_connect(context, request).await;
        }
        let identity = match self.request_identity(context, &request).await {
            Ok(identity) => identity,
            Err(status) => return proxy_error(status),
        };
        let scheme = request.uri().scheme_str().unwrap_or("http");
        let Some(authority) = request.uri().authority().map(ToString::to_string) else {
            return proxy_error(StatusCode::BAD_REQUEST);
        };
        let path = request.uri().path();
        let Ok(secrets) = self
            .policy
            .agent_effective_secrets(&identity.agent_id, &identity.principal_id)
        else {
            return proxy_error(StatusCode::FORBIDDEN);
        };
        let Some(secret) = secrets.into_iter().find(|secret| {
            origin_matches(secret, scheme, &authority)
                && allows_request(secret, request.method(), path.trim_start_matches('/'))
        }) else {
            return proxy_error(StatusCode::FORBIDDEN);
        };
        let Ok(value) = self.manager.resolve(&secret.source).await else {
            return proxy_error(StatusCode::BAD_GATEWAY);
        };
        request.headers_mut().remove(PROXY_AUTHORIZATION);
        if apply_delivery(request.headers_mut(), &secret.delivery, &value).is_err() {
            return proxy_error(StatusCode::BAD_REQUEST);
        }
        tracing::debug!(
            agent_id = %identity.agent_id,
            secret_id = %secret.id,
            host = authority,
            "transparent secret proxy request authorized"
        );
        request.into()
    }
}

impl TransparentProxyHandler {
    async fn handle_connect(
        &self,
        context: &HttpContext,
        mut request: ProxyRequest<ProxyBody>,
    ) -> RequestOrResponse {
        let Some(authority) = request.uri().authority().map(ToString::to_string) else {
            return proxy_error(StatusCode::BAD_REQUEST);
        };
        let Some(token) = proxy_token(request.headers()) else {
            return proxy_error(StatusCode::PROXY_AUTHENTICATION_REQUIRED);
        };
        let (reply, response) = oneshot::channel();
        if self
            .leases
            .send(LeaseCommand::BindClient {
                token,
                client: context.client_addr,
                authority: authority.clone(),
                reply,
            })
            .is_err()
        {
            return proxy_error(StatusCode::BAD_GATEWAY);
        }
        let identity = match response.await {
            Ok(Some(identity)) => identity,
            Ok(None) => return proxy_error(StatusCode::PROXY_AUTHENTICATION_REQUIRED),
            Err(_) => return proxy_error(StatusCode::BAD_GATEWAY),
        };
        let allowed = self
            .policy
            .agent_effective_secrets(&identity.agent_id, &identity.principal_id)
            .is_ok_and(|secrets| {
                secrets
                    .iter()
                    .any(|secret| origin_matches(secret, "https", &authority))
            });
        if !allowed {
            drop(self.leases.send(LeaseCommand::RemoveClient {
                client: context.client_addr,
            }));
            return proxy_error(StatusCode::FORBIDDEN);
        }
        request.headers_mut().remove(PROXY_AUTHORIZATION);
        request.into()
    }

    async fn request_identity(
        &self,
        context: &HttpContext,
        request: &ProxyRequest<ProxyBody>,
    ) -> Result<LeaseIdentity, StatusCode> {
        if let Some(token) = proxy_token(request.headers()) {
            let (reply, response) = oneshot::channel();
            self.leases
                .send(LeaseCommand::Resolve { token, reply })
                .map_err(|_| StatusCode::BAD_GATEWAY)?;
            return response
                .await
                .map_err(|_| StatusCode::BAD_GATEWAY)?
                .ok_or(StatusCode::PROXY_AUTHENTICATION_REQUIRED);
        }
        // Only requests decrypted from an authenticated CONNECT tunnel may use
        // the per-connection binding. Plain HTTP must authenticate every
        // request, avoiding reuse of a stale client socket address.
        if request.uri().scheme_str() != Some("https") {
            return Err(StatusCode::PROXY_AUTHENTICATION_REQUIRED);
        }
        let (reply, response) = oneshot::channel();
        self.leases
            .send(LeaseCommand::ResolveClient {
                client: context.client_addr,
                reply,
            })
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        let binding = response
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?
            .ok_or(StatusCode::PROXY_AUTHENTICATION_REQUIRED)?;
        let request_authority = request
            .uri()
            .authority()
            .ok_or(StatusCode::BAD_REQUEST)?
            .as_str();
        if !same_authority(&binding.authority, request_authority) {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(binding.identity)
    }
}

fn proxy_token(headers: &HeaderMap) -> Option<String> {
    let encoded = headers
        .get(PROXY_AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    let decoded = STANDARD.decode(encoded).ok()?;
    let decoded = std::str::from_utf8(&decoded).ok()?;
    let (username, token) = decoded.split_once(':')?;
    (username == "nanocentaur" && !token.is_empty()).then(|| token.to_owned())
}

fn proxy_error(status: StatusCode) -> RequestOrResponse {
    ProxyResponse::builder()
        .status(status)
        .body(ProxyBody::from("secret proxy request rejected"))
        .expect("static proxy response is valid")
        .into()
}

fn origin_matches(secret: &SecretView, scheme: &str, authority: &str) -> bool {
    let Ok(upstream) = Url::parse(&secret.upstream) else {
        return false;
    };
    let Ok(authority) = authority.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    upstream.scheme() == scheme
        && upstream
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(authority.host()))
        && upstream.port_or_known_default()
            == authority
                .port_u16()
                .or_else(|| (scheme == "https").then_some(443))
                .or_else(|| (scheme == "http").then_some(80))
}

fn same_authority(left: &str, right: &str) -> bool {
    let Ok(left) = left.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    let Ok(right) = right.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    left.host().eq_ignore_ascii_case(right.host())
        && left.port_u16().unwrap_or(443) == right.port_u16().unwrap_or(443)
}

#[async_trait]
impl EgressProvider for SecretGateway {
    async fn acquire(
        &self,
        context: &EgressContext,
        requested: &BTreeSet<CapabilityName>,
    ) -> Result<EgressLease, EgressError> {
        let mut lease = self.inner.acquire(context, requested).await?;
        let secrets = self
            .policy
            .effective_secrets(&context.principal)
            .map_err(|error| EgressError::Provider(error.to_string()))?;
        if secrets.is_empty() {
            return Ok(lease);
        }

        let token = Uuid::new_v4().to_string();
        let mut environment = BTreeMap::new();
        for secret in &secrets {
            let url = self
                .lease_url(&token, &secret.id)
                .map_err(|error| EgressError::Provider(error.to_string()))?;
            insert_environment(&mut environment, &secret.guest.base_url_env, url)?;
            if let Some(placeholder) = &secret.guest.placeholder_env {
                insert_environment(&mut environment, placeholder, placeholder.clone())?;
            }
        }
        let mut proxy_url = self.proxy_base_url.clone();
        proxy_url
            .set_username("nanocentaur")
            .map_err(|()| EgressError::Provider("secret proxy URL is invalid".to_owned()))?;
        proxy_url
            .set_password(Some(&token))
            .map_err(|()| EgressError::Provider("secret proxy URL is invalid".to_owned()))?;
        let proxy_url: String = proxy_url.into();
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            insert_environment(&mut environment, name, proxy_url.clone())?;
        }
        for name in [
            "NODE_EXTRA_CA_CERTS",
            "REQUESTS_CA_BUNDLE",
            "CURL_CA_BUNDLE",
            "SSL_CERT_FILE",
            "GIT_SSL_CAINFO",
        ] {
            insert_environment(&mut environment, name, GUEST_CA_PATH.to_owned())?;
        }
        insert_environment(
            &mut environment,
            "FIREWALL_HOST",
            self.proxy_base_url
                .host_str()
                .unwrap_or_default()
                .to_owned(),
        )?;
        insert_environment(
            &mut environment,
            "FIREWALL_PROXY_PORT",
            self.proxy_base_url
                .port_or_known_default()
                .unwrap_or_default()
                .to_string(),
        )?;
        for (name, _) in lease.guest_environment() {
            if environment.contains_key(name) {
                return Err(EgressError::Provider(format!(
                    "secret guest environment conflicts with `{name}`"
                )));
            }
        }

        self.leases
            .send(LeaseCommand::Insert {
                token: token.clone(),
                identity: LeaseIdentity {
                    agent_id: context.agent_id.clone(),
                    principal_id: context.principal.clone(),
                },
            })
            .map_err(|_| EgressError::Provider("secret lease store is unavailable".to_owned()))?;
        let guard = Arc::new(LeaseGuard {
            token,
            leases: self.leases.clone(),
        });
        lease = lease
            .with_network(Network::Internet)
            .with_environment(environment)
            .with_mount(EgressMount {
                tag: CA_MOUNT_TAG.to_owned(),
                host_path: self.ca_directory.path().to_owned(),
                guest_path: PathBuf::from(GUEST_CA_DIRECTORY),
            })
            .with_guard(guard);
        Ok(lease)
    }
}

async fn run_lease_store(mut receiver: mpsc::UnboundedReceiver<LeaseCommand>) {
    let mut leases = HashMap::new();
    let mut clients = HashMap::<SocketAddr, ClientBinding>::new();
    while let Some(command) = receiver.recv().await {
        match command {
            LeaseCommand::Insert { token, identity } => {
                leases.insert(token, identity);
            }
            LeaseCommand::Resolve { token, reply } => {
                drop(reply.send(leases.get(&token).cloned()));
            }
            LeaseCommand::BindClient {
                token,
                client,
                authority,
                reply,
            } => {
                let identity = leases.get(&token).cloned();
                if let Some(identity) = &identity {
                    clients.insert(
                        client,
                        ClientBinding {
                            token,
                            identity: identity.clone(),
                            authority,
                        },
                    );
                }
                drop(reply.send(identity));
            }
            LeaseCommand::ResolveClient { client, reply } => {
                drop(reply.send(clients.get(&client).cloned()));
            }
            LeaseCommand::RemoveClient { client } => {
                clients.remove(&client);
            }
            LeaseCommand::Remove { token } => {
                leases.remove(&token);
                clients.retain(|_, binding| binding.token != token);
            }
        }
    }
}

fn insert_environment(
    environment: &mut BTreeMap<String, String>,
    name: &str,
    value: String,
) -> Result<(), EgressError> {
    if environment.insert(name.to_owned(), value).is_some() {
        return Err(EgressError::Provider(format!(
            "duplicate secret guest environment `{name}`"
        )));
    }
    Ok(())
}

fn allows_request(secret: &SecretView, method: &Method, path: &str) -> bool {
    let supported_method = [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::HEAD,
        Method::OPTIONS,
    ]
    .iter()
    .any(|supported| supported == method);
    let Some(path) = safe_request_path(path) else {
        return false;
    };
    if !supported_method {
        return false;
    }
    secret.rules.is_empty()
        || secret.rules.iter().any(|rule| {
            (rule.methods.is_empty() || rule.methods.iter().any(|allowed| allowed.matches(method)))
                && (rule.path_prefixes.is_empty()
                    || rule
                        .path_prefixes
                        .iter()
                        .any(|prefix| path.starts_with(prefix)))
        })
}

fn safe_request_path(path: &str) -> Option<String> {
    if path.contains('\\')
        || path.split('/').any(|segment| segment == "..")
        || contains_ambiguous_path_escape(path)
    {
        return None;
    }
    Some(format!("/{}", path.trim_start_matches('/')))
}

fn contains_ambiguous_path_escape(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }
        let Some(encoded) = bytes
            .get(index + 1..index + 3)
            .and_then(|digits| decode_hex_byte(digits[0], digits[1]))
        else {
            return true;
        };
        if matches!(encoded, b'%' | b'.' | b'/' | b'\\' | b'?' | b'#') {
            return true;
        }
        index += 3;
    }
    false
}

fn decode_hex_byte(high: u8, low: u8) -> Option<u8> {
    Some(hex_value(high)? << 4 | hex_value(low)?)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

fn destination(
    secret: &SecretView,
    path: &str,
    query: Option<&str>,
) -> Result<Url, SecretGatewayError> {
    let mut destination =
        Url::parse(&secret.upstream).map_err(|_| SecretGatewayError::InvalidConfiguration)?;
    let base = destination.path().trim_end_matches('/');
    destination.set_path(&format!("{base}/{}", path.trim_start_matches('/')));
    destination.set_query(query);
    Ok(destination)
}

fn filtered_request_headers(headers: &HeaderMap, delivery: &SecretDelivery) -> HeaderMap {
    let delivery_header = delivery.header();
    let connection_headers = connection_headers(headers);
    headers
        .iter()
        .filter(|(name, _)| {
            !is_hop_by_hop(name)
                && !connection_headers.contains(name.as_str())
                && *name != HOST
                && *name != CONTENT_LENGTH
                && *name != PROXY_AUTHORIZATION
                && (*name != AUTHORIZATION || name.as_str().eq_ignore_ascii_case(delivery_header))
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn filtered_response_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_headers = connection_headers(headers);
    headers
        .iter()
        .filter(|(name, _)| {
            !is_hop_by_hop(name)
                && !connection_headers.contains(name.as_str())
                && *name != CONTENT_LENGTH
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    *name == CONNECTION
        || *name == TRANSFER_ENCODING
        || matches!(
            name.as_str(),
            "keep-alive" | "proxy-authenticate" | "te" | "trailer" | "upgrade"
        )
}

fn connection_headers(headers: &HeaderMap) -> BTreeSet<String> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn apply_delivery(
    headers: &mut HeaderMap,
    delivery: &SecretDelivery,
    secret: &str,
) -> Result<(), SecretGatewayError> {
    match delivery {
        SecretDelivery::InjectHeader { header, prefix } => {
            let name = HeaderName::from_bytes(header.as_bytes())
                .map_err(|_| SecretGatewayError::InvalidConfiguration)?;
            let value = HeaderValue::from_str(&format!("{prefix}{secret}"))
                .map_err(|_| SecretGatewayError::Resolution)?;
            headers.insert(name, value);
        }
        SecretDelivery::ReplaceHeader {
            header,
            placeholder,
        } => {
            let name = HeaderName::from_bytes(header.as_bytes())
                .map_err(|_| SecretGatewayError::InvalidConfiguration)?;
            let value = headers
                .get(&name)
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.contains(placeholder))
                .ok_or(SecretGatewayError::MissingPlaceholder)?;
            let value = HeaderValue::from_str(&value.replace(placeholder, secret))
                .map_err(|_| SecretGatewayError::Resolution)?;
            headers.insert(name, value);
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum SecretGatewayError {
    #[error("secret gateway public URL is invalid")]
    InvalidPublicUrl,
    #[error("secret gateway HTTP client could not be built")]
    Client(#[source] reqwest::Error),
    #[error("secret gateway requires a Tokio runtime")]
    NoRuntime,
    #[error("secret proxy listener could not be bound")]
    ProxyBind(#[source] std::io::Error),
    #[error("secret proxy could not be built")]
    Proxy(#[source] hudsucker::Error),
    #[error("secret proxy certificate could not be generated")]
    Certificate(#[source] hudsucker::rcgen::Error),
    #[error("secret proxy certificate could not be stored")]
    CertificateIo(#[source] std::io::Error),
    #[error("secret gateway lease is invalid")]
    InvalidLease,
    #[error("secret gateway policy denied the request")]
    Policy(#[source] PolicyError),
    #[error("secret gateway request is outside the configured rules")]
    RequestDenied,
    #[error("secret gateway request body is too large")]
    RequestTooLarge,
    #[error("secret gateway upstream response is too large")]
    ResponseTooLarge,
    #[error("secret gateway configuration is invalid")]
    InvalidConfiguration,
    #[error("secret gateway placeholder is missing")]
    MissingPlaceholder,
    #[error("secret could not be resolved")]
    Resolution,
    #[error("secret upstream request failed")]
    Upstream,
    #[error("secret gateway is unavailable")]
    Unavailable,
}

impl SecretGatewayError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidLease | Self::Policy(_) | Self::RequestDenied => StatusCode::FORBIDDEN,
            Self::MissingPlaceholder | Self::InvalidConfiguration => StatusCode::BAD_REQUEST,
            Self::RequestTooLarge | Self::ResponseTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::InvalidPublicUrl
            | Self::Client(_)
            | Self::NoRuntime
            | Self::ProxyBind(_)
            | Self::Proxy(_)
            | Self::Certificate(_)
            | Self::CertificateIo(_)
            | Self::Resolution
            | Self::Upstream
            | Self::Unavailable => StatusCode::BAD_GATEWAY,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::InvalidPublicUrl => "invalid_public_url",
            Self::Client(_) => "client",
            Self::NoRuntime => "no_runtime",
            Self::ProxyBind(_) => "proxy_bind",
            Self::Proxy(_) => "proxy",
            Self::Certificate(_) => "certificate",
            Self::CertificateIo(_) => "certificate_io",
            Self::InvalidLease => "invalid_lease",
            Self::Policy(_) => "policy",
            Self::RequestDenied => "request_denied",
            Self::RequestTooLarge => "request_too_large",
            Self::ResponseTooLarge => "response_too_large",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::MissingPlaceholder => "missing_placeholder",
            Self::Resolution => "resolution",
            Self::Upstream => "upstream",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Serialize)]
struct GatewayErrorBody {
    error: GatewayErrorCode,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum GatewayErrorCode {
    SecretGatewayRequestFailed,
}

impl IntoResponse for SecretGatewayError {
    fn into_response(self) -> Response {
        (
            self.status(),
            axum::Json(GatewayErrorBody {
                error: GatewayErrorCode::SecretGatewayRequestFailed,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, RwLock};

    use axum::{Router, routing::any};

    use super::*;
    use crate::{
        CapabilityEgress, CreateSecret, SecretGuestConfig, SecretHttpMethod, SecretRef,
        SecretRequestRule,
    };

    struct RotatingSecret {
        value: RwLock<String>,
    }

    #[async_trait]
    impl SecretManager for RotatingSecret {
        async fn resolve(&self, _reference: &SecretRef) -> Result<String, crate::SecretError> {
            Ok(self.value.read().unwrap().clone())
        }
    }

    fn path_scoped_secret() -> SecretView {
        SecretView {
            id: "path-test".to_owned(),
            name: "Path test".to_owned(),
            source: SecretRef {
                provider: "test".to_owned(),
                key: "test".to_owned(),
            },
            upstream: "https://example.com".to_owned(),
            rules: vec![SecretRequestRule {
                methods: BTreeSet::from([SecretHttpMethod::Get]),
                path_prefixes: vec!["/allowed/".to_owned()],
            }],
            delivery: SecretDelivery::InjectHeader {
                header: "authorization".to_owned(),
                prefix: "Bearer ".to_owned(),
            },
            guest: SecretGuestConfig {
                base_url_env: "TEST_BASE_URL".to_owned(),
                placeholder_env: None,
            },
            enabled: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn rejects_ambiguous_path_encodings_before_policy_matching() {
        let secret = path_scoped_secret();
        assert!(allows_request(&secret, &Method::GET, "/allowed/resource"));
        for path in [
            "/allowed/../admin",
            "/allowed/%2e%2e/admin",
            "/allowed/%2E%2E/admin",
            "/allowed/%252e%252e/admin",
            "/allowed%2f..%2fadmin",
            "/allowed%5c..%5cadmin",
            "/allowed\\..\\admin",
            "/allowed/%",
            "/allowed/%zz",
        ] {
            assert!(
                !allows_request(&secret, &Method::GET, path),
                "ambiguous path was accepted: {path}"
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn values_stay_host_side_and_rotation_and_revocation_are_immediate() {
        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let upstream_observed = Arc::clone(&observed);
        let upstream = Router::new().fallback(any(move |request: Request| {
            let observed = Arc::clone(&upstream_observed);
            async move {
                observed.lock().unwrap().push(
                    request
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned(),
                );
                "ok"
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

        let policy = Arc::new(PolicyStore::in_memory().unwrap());
        policy
            .bootstrap("client", "Client", "key", "principal", [])
            .unwrap();
        policy
            .create_secret(CreateSecret {
                id: Some("openai".to_owned()),
                name: "OpenAI".to_owned(),
                source: SecretRef {
                    provider: "test".to_owned(),
                    key: "openai".to_owned(),
                },
                upstream: format!("http://{address}"),
                rules: vec![SecretRequestRule {
                    methods: BTreeSet::from([SecretHttpMethod::Get]),
                    path_prefixes: vec!["/v1/".to_owned()],
                }],
                delivery: SecretDelivery::InjectHeader {
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                },
                guest: SecretGuestConfig {
                    base_url_env: "OPENAI_BASE_URL".to_owned(),
                    placeholder_env: None,
                },
            })
            .unwrap();
        policy
            .create_secret(CreateSecret {
                id: Some("tls-denied".to_owned()),
                name: "TLS interception".to_owned(),
                source: SecretRef {
                    provider: "test".to_owned(),
                    key: "tls".to_owned(),
                },
                upstream: "https://localhost:44443".to_owned(),
                rules: vec![SecretRequestRule {
                    methods: BTreeSet::from([SecretHttpMethod::Post]),
                    path_prefixes: vec!["/allowed".to_owned()],
                }],
                delivery: SecretDelivery::InjectHeader {
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                },
                guest: SecretGuestConfig {
                    base_url_env: "TLS_TEST_BASE_URL".to_owned(),
                    placeholder_env: None,
                },
            })
            .unwrap();
        policy
            .set_principal_secret("principal", "openai", true)
            .unwrap();
        policy
            .set_principal_secret("principal", "tls-denied", true)
            .unwrap();
        let client = crate::AuthenticatedClient {
            id: "client".to_owned(),
            default_principal_id: "principal".to_owned(),
        };
        let (identity, _) = policy.create_or_resolve_agent(&client, None).unwrap();
        let manager = Arc::new(RotatingSecret {
            value: RwLock::new("alpha-secret".to_owned()),
        });
        let gateway = SecretGateway::new(
            Arc::clone(&policy),
            manager.clone(),
            Arc::new(CapabilityEgress::new()),
            "http://127.0.0.1",
        )
        .unwrap();
        let lease = gateway
            .acquire(
                &EgressContext {
                    agent_id: identity.id,
                    principal: "principal".to_owned(),
                },
                &BTreeSet::new(),
            )
            .await
            .unwrap();
        assert!(lease.guest_environment().iter().all(|(name, value)| {
            !name.starts_with("NANOCENTAUR_SECRET_") && !value.contains("alpha-secret")
        }));
        assert_eq!(lease.guest_mounts().len(), 1);
        let proxy_route = lease
            .guest_environment()
            .iter()
            .find(|(name, _)| name == "HTTPS_PROXY")
            .unwrap()
            .1
            .clone();
        let ca = std::fs::read(gateway.ca_directory.path().join("ca.pem")).unwrap();
        let tls_client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
            .proxy(reqwest::Proxy::all(&proxy_route).unwrap())
            .build()
            .unwrap();
        let intercepted = tls_client
            .get("https://localhost:44443/denied")
            .send()
            .await
            .unwrap();
        assert_eq!(intercepted.status(), StatusCode::FORBIDDEN);
        let proxied = tls_client
            .get(format!("http://{address}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(proxied.status(), StatusCode::OK);

        let route = lease
            .guest_environment()
            .iter()
            .find(|(name, _)| name == "OPENAI_BASE_URL")
            .unwrap()
            .1
            .clone();
        let token = Url::parse(&route)
            .unwrap()
            .path_segments()
            .unwrap()
            .nth(3)
            .unwrap()
            .to_owned();

        let response = gateway
            .handle(
                &token,
                "openai",
                "v1/models",
                Request::get("/").body(Body::empty()).unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        *manager.value.write().unwrap() = "beta-secret".to_owned();
        let response = gateway
            .handle(
                &token,
                "openai",
                "v1/models",
                Request::get("/").body(Body::empty()).unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            observed.lock().unwrap().as_slice(),
            [
                "Bearer alpha-secret",
                "Bearer alpha-secret",
                "Bearer beta-secret"
            ]
        );

        policy
            .set_principal_secret("principal", "openai", false)
            .unwrap();
        let response = gateway
            .handle(
                &token,
                "openai",
                "v1/models",
                Request::get("/").body(Body::empty()).unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let proxied = tls_client
            .get(format!("http://{address}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(proxied.status(), StatusCode::FORBIDDEN);
        assert_eq!(observed.lock().unwrap().len(), 3);

        policy
            .set_principal_secret("principal", "openai", true)
            .unwrap();
        policy.disable_principal("principal").unwrap();
        let response = gateway
            .handle(
                &token,
                "openai",
                "v1/models",
                Request::get("/").body(Body::empty()).unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(observed.lock().unwrap().len(), 3);

        drop(lease);
        server.abort();
    }
}
