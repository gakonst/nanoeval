use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use arcbox_ext4::{Formatter, Reader};
use flate2::read::GzDecoder;
use ignore::WalkBuilder;
use nanocodex_vm::{VmCommand, VmToolSession, VmToolSessionError};
use nanoeval::Task;
use oci_client::{
    Client, Reference, client::ClientConfig, config::ConfigFile, manifest::ImageIndexEntry,
    secrets::RegistryAuth,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tracing::info;

use crate::disk::reflink_or_sparse_copy;

const BLOCK_SIZE: u32 = 4_096;
const MINIMUM_DISK_BYTES: u64 = 512 * 1024 * 1024;
const CACHE_RECORD_VERSION: u32 = 2;
const TASK_BUILD_CACHE_VERSION: u32 = 3;
const PREPARED_DISK_RECORD_VERSION: u32 = 1;
const CONTEXT_DISK_BYTES: u64 = 128 * 1024 * 1024;
const BUILD_STEP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const BUILD_COPY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const BUILD_RUNTIME_ID: &str = "nanoeval-runtime";
const BUILD_CONTEXT_ID: &str = "nanoeval-context";
const BUILD_RUNTIME_DEVICE: &str = "/dev/vdb";
const BUILD_CONTEXT_DEVICE: &str = "/dev/vdc";
const BUILD_RUNTIME_MOUNT: &str = "/run/nanoeval";
const BUILD_CONTEXT_MOUNT: &str = "/mnt/nanoeval-context";
const DEFAULT_GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const BUILD_VM_MEMORY_MIB: u64 = 4_096;
const COPY_SCRIPT: &str = r#"set -eu
dest=$1
shift
if [ "$#" -gt 1 ]; then
  mkdir -p "$dest"
  for src do cp -a "$src" "$dest/"; done
elif [ -d "$1" ]; then
  mkdir -p "$dest"
  cp -a "$1/." "$dest/"
elif [ "${dest%/}" != "$dest" ]; then
  mkdir -p "$dest"
  cp -a "$1" "$dest/"
else
  mkdir -p "$(dirname "$dest")"
  cp -a "$1" "$dest"
fi"#;

#[derive(Clone)]
pub(crate) struct VmImageBuilder {
    vmm: PathBuf,
    runtime_image: PathBuf,
    firmware_directory: PathBuf,
}

impl VmImageBuilder {
    pub(crate) fn new(
        vmm: impl Into<PathBuf>,
        runtime_image: impl Into<PathBuf>,
        firmware_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            vmm: vmm.into(),
            runtime_image: runtime_image.into(),
            firmware_directory: firmware_directory.into(),
        }
    }
}

pub(crate) struct PreparedRootDisk {
    path: PathBuf,
    workdir: String,
    shell: String,
    environment: BTreeMap<String, String>,
    manifest_digest: String,
    manifest_source: ManifestSource,
    disk_status: DiskStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CachePolicy {
    Reuse,
    Refresh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManifestSource {
    Local,
    Registry,
}

impl ManifestSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Registry => "registry",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiskStatus {
    Hit,
    Created,
}

impl DiskStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Created => "created",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ImageError {
    #[error("failed to read image input: {0}")]
    Io(#[from] io::Error),

    #[error("invalid OCI image reference {image}: {source}")]
    Reference {
        image: String,
        #[source]
        source: oci_client::ParseError,
    },

    #[error("OCI registry operation failed: {0}")]
    Registry(#[from] oci_client::errors::OciDistributionError),

    #[error("failed to format ext4 root disk: {0}")]
    Ext4(#[from] arcbox_ext4::error::FormatError),

    #[error("failed to inspect ext4 root disk: {0}")]
    ReadExt4(#[from] arcbox_ext4::error::ReadError),

    #[error("root disk formatting task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("unsupported Dockerfile instruction in the first VM proof: {0}")]
    UnsupportedDockerfile(String),

    #[error("Dockerfile must contain exactly one FROM instruction")]
    InvalidFrom,

    #[error("unsupported OCI layer media type: {0}")]
    UnsupportedLayer(String),

    #[error("prepared image is missing required path {0}")]
    MissingPreparedPath(&'static str),

    #[error("failed to read VM cache metadata: {0}")]
    CacheMetadata(#[from] serde_json::Error),

    #[error("VM image build failed: {0}")]
    Vm(#[from] VmToolSessionError),

    #[error(
        "Dockerfile stage {stage} instruction {instruction} exited with {exit_code}\nstdout (tail):\n{stdout}\nstderr (tail):\n{stderr}"
    )]
    BuildStep {
        stage: usize,
        instruction: usize,
        exit_code: i32,
        stdout: String,
        stderr: String,
    },

    #[error("COPY source does not exist in the build context: {0}")]
    MissingCopySource(String),

    #[error("COPY --from refers to unknown stage or image: {0}")]
    UnknownCopySource(String),
}

impl PreparedRootDisk {
    pub(crate) async fn prepare(
        task: &Task,
        cache: &Path,
        policy: CachePolicy,
        builder: &VmImageBuilder,
    ) -> Result<Self, ImageError> {
        Self::prepare_directory(
            &task.environment_directory(),
            task.resources().storage_mb,
            cache,
            policy,
            builder,
        )
        .await
    }

    pub(crate) async fn prepare_verifier(
        task: &Task,
        cache: &Path,
        policy: CachePolicy,
        builder: &VmImageBuilder,
    ) -> Result<Self, ImageError> {
        Self::prepare_directory(
            &task.root().join("tests"),
            task.resources().storage_mb,
            cache,
            policy,
            builder,
        )
        .await
    }

    async fn prepare_directory(
        directory: &Path,
        storage_mb: u64,
        cache: &Path,
        policy: CachePolicy,
        builder: &VmImageBuilder,
    ) -> Result<Self, ImageError> {
        let dockerfile_path = directory.join("Dockerfile");
        let dockerfile = fs::read_to_string(&dockerfile_path)?;
        let recipe = DockerfileRecipe::parse(&dockerfile)?;
        let disk_bytes = storage_mb
            .saturating_mul(1024 * 1024)
            .max(MINIMUM_DISK_BYTES);
        let images = resolve_recipe_images(&recipe, cache, policy).await?;
        let final_stage = recipe.final_stage().ok_or(ImageError::InvalidFrom)?;
        let final_image = images
            .get(&final_stage.base_image)
            .ok_or_else(|| ImageError::UnknownCopySource(final_stage.base_image.clone()))?;
        let (path, disk_status, environment) = if recipe.requires_build() {
            prepare_built_disk(
                directory,
                &dockerfile,
                &recipe,
                &images,
                cache,
                disk_bytes,
                builder,
            )
            .await?
        } else {
            let (path, status) = prepare_flattened_disk(cache, final_image, disk_bytes).await?;
            (
                path,
                status,
                docker_process_environment(&final_image.config.environment),
            )
        };
        let shell = cached_prepared_shell(&path)?;
        Ok(Self {
            shell,
            path,
            workdir: recipe.final_workdir().to_owned(),
            environment,
            manifest_digest: final_image.manifest_digest.clone(),
            manifest_source: final_image.source,
            disk_status,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn workdir(&self) -> &str {
        &self.workdir
    }

    pub(crate) fn shell(&self) -> &str {
        &self.shell
    }

    pub(crate) fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub(crate) fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    pub(crate) const fn manifest_source(&self) -> ManifestSource {
        self.manifest_source
    }

    pub(crate) const fn disk_status(&self) -> DiskStatus {
        self.disk_status
    }
}

#[derive(Deserialize, Serialize)]
struct ReferenceRecord {
    version: u32,
    image_reference: String,
    manifest_digest: String,
    layers: Vec<LayerRecord>,
    #[serde(default)]
    config: ImageRuntimeConfig,
}

#[derive(Deserialize, Serialize)]
struct PreparedDiskRecord {
    version: u32,
    file_bytes: u64,
    modified_nanos: u128,
    shell: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct LayerRecord {
    digest: String,
    media_type: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct ImageRuntimeConfig {
    environment: BTreeMap<String, String>,
    working_directory: String,
}

fn read_cache_record<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, ImageError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_cache_record(path: &Path, record: &impl Serialize) -> Result<(), ImageError> {
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| io::Error::other("VM cache record path has no parent"))?,
    )?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, serde_json::to_vec(record)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct DockerfileRecipe {
    stages: Vec<DockerfileStage>,
}

#[derive(Debug, Eq, PartialEq)]
struct DockerfileStage {
    base_image: String,
    name: Option<String>,
    instructions: Vec<DockerfileInstruction>,
}

#[derive(Debug, Eq, PartialEq)]
enum DockerfileInstruction {
    Run(String),
    Copy(DockerfileCopy),
    Workdir(String),
    Env {
        name: String,
        value: String,
    },
    Arg {
        name: String,
        default: Option<String>,
    },
    Cmd(String),
}

#[derive(Debug, Eq, PartialEq)]
struct DockerfileCopy {
    from: Option<String>,
    sources: Vec<String>,
    destination: String,
}

impl DockerfileRecipe {
    fn parse(dockerfile: &str) -> Result<Self, ImageError> {
        let mut stages = Vec::<DockerfileStage>::new();
        for line in dockerfile_logical_lines(dockerfile)? {
            let (instruction, arguments) = line
                .split_once(char::is_whitespace)
                .ok_or_else(|| ImageError::UnsupportedDockerfile(line.clone()))?;
            match instruction.to_ascii_uppercase().as_str() {
                "FROM" => {
                    let fields = arguments.split_whitespace().collect::<Vec<_>>();
                    let (base_image, name) = match fields.as_slice() {
                        [base_image] => ((*base_image).to_owned(), None),
                        [base_image, keyword, name] if keyword.eq_ignore_ascii_case("AS") => {
                            ((*base_image).to_owned(), Some((*name).to_owned()))
                        }
                        _ => return Err(ImageError::InvalidFrom),
                    };
                    if base_image.is_empty() {
                        return Err(ImageError::InvalidFrom);
                    }
                    stages.push(DockerfileStage {
                        base_image,
                        name,
                        instructions: Vec::new(),
                    });
                }
                "WORKDIR" => {
                    if arguments.split_whitespace().count() != 1 || !valid_guest_workdir(arguments)
                    {
                        return Err(ImageError::UnsupportedDockerfile(line.clone()));
                    }
                    current_stage(&mut stages, &line)?
                        .instructions
                        .push(DockerfileInstruction::Workdir(arguments.to_owned()));
                }
                "RUN" if !arguments.trim().is_empty() => current_stage(&mut stages, &line)?
                    .instructions
                    .push(DockerfileInstruction::Run(arguments.to_owned())),
                "COPY" => current_stage(&mut stages, &line)?
                    .instructions
                    .push(DockerfileInstruction::Copy(parse_copy(arguments, &line)?)),
                "ENV" => {
                    let (name, value) = parse_assignment(arguments, &line)?;
                    current_stage(&mut stages, &line)?
                        .instructions
                        .push(DockerfileInstruction::Env { name, value });
                }
                "ARG" => {
                    let (name, default) = match arguments.split_once('=') {
                        Some((name, value)) => (name, Some(value.to_owned())),
                        None => (arguments, None),
                    };
                    if !valid_environment_name(name) {
                        return Err(ImageError::UnsupportedDockerfile(line));
                    }
                    current_stage(&mut stages, &line)?.instructions.push(
                        DockerfileInstruction::Arg {
                            name: name.to_owned(),
                            default,
                        },
                    );
                }
                "CMD" if !arguments.trim().is_empty() => current_stage(&mut stages, &line)?
                    .instructions
                    .push(DockerfileInstruction::Cmd(arguments.to_owned())),
                _ => return Err(ImageError::UnsupportedDockerfile(line.clone())),
            }
        }
        if stages.is_empty() {
            return Err(ImageError::InvalidFrom);
        }
        Ok(Self { stages })
    }

    fn final_stage(&self) -> Option<&DockerfileStage> {
        self.stages.last()
    }

    fn final_workdir(&self) -> &str {
        self.final_stage()
            .into_iter()
            .flat_map(|stage| stage.instructions.iter().rev())
            .find_map(|instruction| match instruction {
                DockerfileInstruction::Workdir(workdir) => Some(workdir.as_str()),
                DockerfileInstruction::Run(_)
                | DockerfileInstruction::Copy(_)
                | DockerfileInstruction::Env { .. }
                | DockerfileInstruction::Arg { .. }
                | DockerfileInstruction::Cmd(_) => None,
            })
            .unwrap_or("/")
    }

    fn requires_build(&self) -> bool {
        self.stages.len() != 1
            || self.stages.iter().any(|stage| {
                stage
                    .instructions
                    .iter()
                    .any(|instruction| !matches!(instruction, DockerfileInstruction::Workdir(_)))
            })
    }
}

fn dockerfile_logical_lines(dockerfile: &str) -> Result<Vec<String>, ImageError> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for raw_line in dockerfile.lines() {
        let trimmed = raw_line.trim();
        if current.is_empty() && (trimmed.is_empty() || trimmed.starts_with('#')) {
            continue;
        }
        let continuation = raw_line.trim_end().ends_with('\\');
        let fragment = if continuation {
            raw_line.trim_end().strip_suffix('\\').unwrap_or(raw_line)
        } else {
            raw_line
        };
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(fragment.trim());
        if !continuation {
            let logical = std::mem::take(&mut current);
            if !logical.is_empty() {
                lines.push(logical);
            }
        }
    }
    if !current.is_empty() {
        return Err(ImageError::UnsupportedDockerfile(current));
    }
    Ok(lines)
}

fn current_stage<'a>(
    stages: &'a mut [DockerfileStage],
    line: &str,
) -> Result<&'a mut DockerfileStage, ImageError> {
    stages
        .last_mut()
        .ok_or_else(|| ImageError::UnsupportedDockerfile(line.to_owned()))
}

fn parse_copy(arguments: &str, line: &str) -> Result<DockerfileCopy, ImageError> {
    let mut fields = arguments.split_whitespace();
    let first = fields
        .next()
        .ok_or_else(|| ImageError::UnsupportedDockerfile(line.to_owned()))?;
    let (from, first_source) = match first.strip_prefix("--from=") {
        Some(from) if !from.is_empty() => (Some(from.to_owned()), fields.next()),
        Some(_) => return Err(ImageError::UnsupportedDockerfile(line.to_owned())),
        None => (None, Some(first)),
    };
    let mut paths = first_source
        .into_iter()
        .chain(fields)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if paths.len() < 2 {
        return Err(ImageError::UnsupportedDockerfile(line.to_owned()));
    }
    let destination = paths
        .pop()
        .ok_or_else(|| ImageError::UnsupportedDockerfile(line.to_owned()))?;
    Ok(DockerfileCopy {
        from,
        sources: paths,
        destination,
    })
}

fn parse_assignment(arguments: &str, line: &str) -> Result<(String, String), ImageError> {
    let (name, value) = arguments
        .split_once('=')
        .ok_or_else(|| ImageError::UnsupportedDockerfile(line.to_owned()))?;
    if !valid_environment_name(name) {
        return Err(ImageError::UnsupportedDockerfile(line.to_owned()));
    }
    Ok((name.to_owned(), value.to_owned()))
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

fn valid_guest_workdir(workdir: &str) -> bool {
    let path = Path::new(workdir);
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

async fn resolve_recipe_images(
    recipe: &DockerfileRecipe,
    cache: &Path,
    policy: CachePolicy,
) -> Result<BTreeMap<String, PulledImage>, ImageError> {
    let stage_names = recipe
        .stages
        .iter()
        .filter_map(|stage| stage.name.as_deref())
        .collect::<BTreeSet<_>>();
    let mut references = recipe
        .stages
        .iter()
        .map(|stage| stage.base_image.clone())
        .collect::<BTreeSet<_>>();
    for copy in recipe.stages.iter().flat_map(|stage| {
        stage
            .instructions
            .iter()
            .filter_map(|instruction| match instruction {
                DockerfileInstruction::Copy(copy) => Some(copy),
                DockerfileInstruction::Run(_)
                | DockerfileInstruction::Workdir(_)
                | DockerfileInstruction::Env { .. }
                | DockerfileInstruction::Arg { .. }
                | DockerfileInstruction::Cmd(_) => None,
            })
    }) {
        if let Some(from) = copy.from.as_deref()
            && !stage_names.contains(from)
            && from.parse::<usize>().is_err()
        {
            references.insert(from.to_owned());
        }
    }
    let mut images = BTreeMap::new();
    for reference in references {
        let image = resolve_image(&reference, cache, policy).await?;
        images.insert(reference, image);
    }
    Ok(images)
}

async fn prepare_flattened_disk(
    cache: &Path,
    image: &PulledImage,
    disk_bytes: u64,
) -> Result<(PathBuf, DiskStatus), ImageError> {
    let key = disk_cache_key(&image.manifest_digest, disk_bytes);
    let path = cache.join("images").join(format!("{key}.ext4"));
    if path.is_file() {
        return Ok((path, DiskStatus::Hit));
    }
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| io::Error::other("prepared root disk cache path has no parent"))?,
    )?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let temporary_for_task = temporary.clone();
    let layers = image.layers.clone();
    tokio::task::spawn_blocking(move || format_root_disk(&temporary_for_task, disk_bytes, &layers))
        .await??;
    fs::rename(&temporary, &path)?;
    validate_ext4_disk(&path)?;
    Ok((path, DiskStatus::Created))
}

async fn prepare_built_disk(
    context_directory: &Path,
    dockerfile: &str,
    recipe: &DockerfileRecipe,
    images: &BTreeMap<String, PulledImage>,
    cache: &Path,
    disk_bytes: u64,
    builder: &VmImageBuilder,
) -> Result<(PathBuf, DiskStatus, BTreeMap<String, String>), ImageError> {
    let context_directory = context_directory.to_path_buf();
    let context_cache = cache.to_path_buf();
    let (context_image, context_digest) = tokio::task::spawn_blocking(move || {
        prepare_context_disk(&context_directory, &context_cache, disk_bytes)
    })
    .await??;
    let key = build_cache_key(dockerfile, &context_digest, recipe, images, disk_bytes);
    let builds = cache.join("builds");
    fs::create_dir_all(&builds)?;
    let path = builds.join(format!("{key}.ext4"));
    let final_environment = final_recipe_environment(recipe, images)?;
    if path.is_file() {
        return Ok((path, DiskStatus::Hit, final_environment));
    }

    let temporary = tempfile::Builder::new()
        .prefix(&format!("{key}."))
        .tempdir_in(&builds)?;
    let mut stage_disks = Vec::with_capacity(recipe.stages.len());
    for (stage_index, stage) in recipe.stages.iter().enumerate() {
        let image = images
            .get(&stage.base_image)
            .ok_or_else(|| ImageError::UnknownCopySource(stage.base_image.clone()))?;
        let (base, _) = prepare_flattened_disk(cache, image, disk_bytes).await?;
        let stage_root = temporary.path().join(format!("stage-{stage_index}.ext4"));
        reflink_or_sparse_copy(&base, &stage_root)?;
        execute_stage(
            stage_index,
            stage,
            recipe,
            images,
            cache,
            disk_bytes,
            &context_image,
            &stage_disks,
            &stage_root,
            builder,
        )
        .await?;
        stage_disks.push(stage_root);
    }
    let final_stage = stage_disks.last().ok_or(ImageError::InvalidFrom)?;
    let published = path.with_extension(format!("{}.tmp", std::process::id()));
    reflink_or_sparse_copy(final_stage, &published)?;
    fs::rename(published, &path)?;
    validate_root_disk(&path)?;
    Ok((path, DiskStatus::Created, final_environment))
}

fn prepare_context_disk(
    environment: &Path,
    cache: &Path,
    task_disk_bytes: u64,
) -> Result<(PathBuf, String), ImageError> {
    let contexts = cache.join("contexts");
    fs::create_dir_all(&contexts)?;
    let mut archive_file = tempfile::NamedTempFile::new_in(&contexts)?;
    {
        let mut archive = tar::Builder::new(archive_file.as_file_mut());
        let mut walker = WalkBuilder::new(environment);
        walker
            .hidden(false)
            .parents(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .follow_links(false)
            .sort_by_file_path(std::cmp::Ord::cmp);
        for entry in walker.build() {
            let entry = entry.map_err(io::Error::other)?;
            let relative = entry
                .path()
                .strip_prefix(environment)
                .map_err(io::Error::other)?;
            if relative.as_os_str().is_empty() {
                continue;
            }
            archive.append_path_with_name(entry.path(), Path::new("context").join(relative))?;
        }
        archive.finish()?;
    }
    archive_file.as_file_mut().seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = archive_file.as_file_mut().read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = format!("{:x}", hasher.finalize());
    let disk_bytes = task_disk_bytes.max(CONTEXT_DISK_BYTES);
    let mut identity = Sha256::new();
    identity.update(b"nanoeval-context-ext4-v1\0");
    identity.update(digest.as_bytes());
    identity.update(disk_bytes.to_le_bytes());
    let key = format!("{:x}", identity.finalize());
    let path = contexts.join(format!("{key}.ext4"));
    if !path.is_file() {
        archive_file.as_file_mut().seek(SeekFrom::Start(0))?;
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        let mut formatter = Formatter::new(&temporary, BLOCK_SIZE, disk_bytes)?;
        formatter.unpack_tar(BufReader::new(archive_file.as_file_mut()))?;
        formatter.close()?;
        fs::rename(temporary, &path)?;
    }
    validate_ext4_disk(&path)?;
    Ok((path, digest))
}

struct BuildMount {
    key: String,
    disk: PathBuf,
    device: String,
    mount: String,
}

#[allow(clippy::too_many_arguments)]
async fn execute_stage(
    stage_index: usize,
    stage: &DockerfileStage,
    recipe: &DockerfileRecipe,
    images: &BTreeMap<String, PulledImage>,
    cache: &Path,
    disk_bytes: u64,
    context_image: &Path,
    stage_disks: &[PathBuf],
    stage_root: &Path,
    builder: &VmImageBuilder,
) -> Result<(), ImageError> {
    let mut mounts = Vec::<BuildMount>::new();
    let mut source_mounts = BTreeMap::<String, String>::new();
    for copy in stage
        .instructions
        .iter()
        .filter_map(|instruction| match instruction {
            DockerfileInstruction::Copy(copy) => Some(copy),
            DockerfileInstruction::Run(_)
            | DockerfileInstruction::Workdir(_)
            | DockerfileInstruction::Env { .. }
            | DockerfileInstruction::Arg { .. }
            | DockerfileInstruction::Cmd(_) => None,
        })
    {
        let Some(from) = copy.from.as_deref() else {
            continue;
        };
        if source_mounts.contains_key(from) {
            continue;
        }
        let disk = if let Some(source_stage) = resolve_stage_index(recipe, from) {
            if source_stage >= stage_index {
                return Err(ImageError::UnknownCopySource(from.to_owned()));
            }
            stage_disks
                .get(source_stage)
                .cloned()
                .ok_or_else(|| ImageError::UnknownCopySource(from.to_owned()))?
        } else {
            let image = images
                .get(from)
                .ok_or_else(|| ImageError::UnknownCopySource(from.to_owned()))?;
            prepare_flattened_disk(cache, image, disk_bytes).await?.0
        };
        let source_number = mounts.len();
        let mount = format!("/mnt/nanoeval-source-{source_number}");
        source_mounts.insert(from.to_owned(), mount.clone());
        mounts.push(BuildMount {
            key: format!("source-{source_number}"),
            disk,
            device: guest_block_device(source_number + 3)?,
            mount,
        });
    }

    let mut command = build_vmm_command(builder, stage_root, context_image, &mounts)?;
    let session = VmToolSession::spawn(&mut command)?;
    let execution = execute_stage_inner(
        &session,
        stage_index,
        stage,
        images,
        &source_mounts,
        &mounts,
    )
    .await;
    let shutdown = session.shutdown().await;
    execution?;
    shutdown?;
    Ok(())
}

fn build_vmm_command(
    builder: &VmImageBuilder,
    stage_root: &Path,
    context_image: &Path,
    mounts: &[BuildMount],
) -> Result<Command, ImageError> {
    let mut command = Command::new(&builder.vmm);
    if builder
        .firmware_directory
        .join("libkrunfw.5.dylib")
        .is_file()
    {
        command.env(
            "DYLD_LIBRARY_PATH",
            builder.firmware_directory.canonicalize()?,
        );
    }
    command
        .arg("vm")
        .arg("run")
        .arg("--root")
        .arg(stage_root)
        .arg("--ext4")
        .arg("--read-only-disk")
        .arg(format!(
            "{BUILD_RUNTIME_ID}={}",
            builder.runtime_image.display()
        ))
        .arg("--read-only-disk")
        .arg(format!("{BUILD_CONTEXT_ID}={}", context_image.display()));
    for mount in mounts {
        command
            .arg("--read-only-disk")
            .arg(format!("{}={}", mount.key, mount.disk.display()));
    }
    let resolver = host_resolver_configuration()?;
    command
        .arg("--cpus")
        .arg("2")
        .arg("--memory-mib")
        .arg(BUILD_VM_MEMORY_MIB.to_string())
        .arg("/bin/sh")
        .arg("-c")
        .arg(format!(
            "printf '{resolver}' > /etc/resolv.conf && mkdir -p {BUILD_RUNTIME_MOUNT} && mount -t ext4 -o ro {BUILD_RUNTIME_DEVICE} {BUILD_RUNTIME_MOUNT} && exec {BUILD_RUNTIME_MOUNT}/nanocodex-vm-guest /"
        ))
        .arg("nanoeval-image-build");
    Ok(command)
}

async fn execute_stage_inner(
    session: &VmToolSession,
    stage_index: usize,
    stage: &DockerfileStage,
    images: &BTreeMap<String, PulledImage>,
    source_mounts: &BTreeMap<String, String>,
    mounts: &[BuildMount],
) -> Result<(), ImageError> {
    mount_build_disk(
        session,
        BUILD_CONTEXT_DEVICE,
        BUILD_CONTEXT_MOUNT,
        stage_index,
        0,
    )
    .await?;
    for (index, mount) in mounts.iter().enumerate() {
        mount_build_disk(session, &mount.device, &mount.mount, stage_index, index + 1).await?;
    }

    let image = images
        .get(&stage.base_image)
        .ok_or_else(|| ImageError::UnknownCopySource(stage.base_image.clone()))?;
    let mut environment = docker_process_environment(&image.config.environment);
    let mut arguments = BTreeMap::<String, String>::new();
    let mut workdir = image.config.working_directory.clone();
    if !valid_guest_workdir(&workdir) {
        "/".clone_into(&mut workdir);
    }

    for (instruction_index, instruction) in stage.instructions.iter().enumerate() {
        match instruction {
            DockerfileInstruction::Workdir(directory) => {
                workdir.clone_from(directory);
                run_build_command(
                    session,
                    stage_index,
                    instruction_index,
                    VmCommand::new("/bin/mkdir")
                        .arg("-p")
                        .arg(directory)
                        .timeout(BUILD_COPY_TIMEOUT),
                )
                .await?;
            }
            DockerfileInstruction::Env { name, value } => {
                let value = expand_variables(value, &environment, &arguments);
                environment.insert(name.clone(), value);
            }
            DockerfileInstruction::Arg { name, default } => {
                let value = default
                    .as_deref()
                    .map(|value| expand_variables(value, &environment, &arguments))
                    .unwrap_or_default();
                arguments.insert(name.clone(), value);
            }
            DockerfileInstruction::Run(script) => {
                let mut command = VmCommand::new("/bin/sh")
                    .arg("-c")
                    .arg(script)
                    .current_directory(&workdir)
                    .timeout(BUILD_STEP_TIMEOUT);
                command = command.environment(build_environment(&environment, &arguments));
                run_build_command(session, stage_index, instruction_index, command).await?;
            }
            DockerfileInstruction::Copy(copy) => {
                execute_copy(CopyExecution {
                    session,
                    stage_index,
                    instruction_index,
                    copy,
                    workdir: &workdir,
                    source_mounts,
                    environment: &environment,
                    arguments: &arguments,
                })
                .await?;
            }
            DockerfileInstruction::Cmd(_) => {}
        }
    }
    Ok(())
}

async fn mount_build_disk(
    session: &VmToolSession,
    device: &str,
    mount: &str,
    stage: usize,
    instruction: usize,
) -> Result<(), ImageError> {
    run_build_command(
        session,
        stage,
        instruction,
        VmCommand::new("/bin/sh")
            .arg("-c")
            .arg("mkdir -p \"$1\" && mount -t ext4 -o ro \"$2\" \"$1\"")
            .arg("nanoeval-mount")
            .arg(mount)
            .arg(device)
            .timeout(BUILD_COPY_TIMEOUT),
    )
    .await
}

struct CopyExecution<'a> {
    session: &'a VmToolSession,
    stage_index: usize,
    instruction_index: usize,
    copy: &'a DockerfileCopy,
    workdir: &'a str,
    source_mounts: &'a BTreeMap<String, String>,
    environment: &'a BTreeMap<String, String>,
    arguments: &'a BTreeMap<String, String>,
}

async fn execute_copy(input: CopyExecution<'_>) -> Result<(), ImageError> {
    let source_root = match input.copy.from.as_deref() {
        None => format!("{BUILD_CONTEXT_MOUNT}/context"),
        Some(from) => input
            .source_mounts
            .get(from)
            .cloned()
            .ok_or_else(|| ImageError::UnknownCopySource(from.to_owned()))?,
    };
    let mut sources = Vec::with_capacity(input.copy.sources.len());
    for source in &input.copy.sources {
        let expanded = expand_variables(source, input.environment, input.arguments);
        let source_path = if input.copy.from.is_none() {
            let relative = Path::new(expanded.trim_start_matches("./"));
            if relative.is_absolute()
                || relative.components().any(|component| {
                    matches!(component, Component::ParentDir | Component::Prefix(_))
                })
            {
                return Err(ImageError::MissingCopySource(expanded));
            }
            Path::new(&source_root).join(relative)
        } else {
            Path::new(&source_root).join(expanded.trim_start_matches('/'))
        };
        sources.push(source_path.to_string_lossy().into_owned());
    }
    let destination = expand_variables(&input.copy.destination, input.environment, input.arguments);
    let destination = if Path::new(&destination).is_absolute() {
        destination
    } else {
        Path::new(input.workdir)
            .join(destination)
            .to_string_lossy()
            .into_owned()
    };
    let mut command = VmCommand::new("/bin/sh")
        .arg("-c")
        .arg(COPY_SCRIPT)
        .arg("nanoeval-copy")
        .arg(destination)
        .timeout(BUILD_COPY_TIMEOUT);
    for source in sources {
        command = command.arg(source);
    }
    run_build_command(
        input.session,
        input.stage_index,
        input.instruction_index,
        command,
    )
    .await
}

async fn run_build_command(
    session: &VmToolSession,
    stage: usize,
    instruction: usize,
    command: VmCommand,
) -> Result<(), ImageError> {
    let output = session.command(command).await?;
    info!(
        target: "nanoeval",
        build_stage = stage,
        build_instruction = instruction,
        process.exit_code = output.exit_code,
        process.stdout.bytes = output.stdout.len(),
        process.stderr.bytes = output.stderr.len(),
        "VM image build instruction completed"
    );
    if output.exit_code == 0 {
        return Ok(());
    }
    let stdout = output_tail(&output.stdout);
    let stderr = output_tail(&output.stderr);
    Err(ImageError::BuildStep {
        stage,
        instruction,
        exit_code: output.exit_code,
        stdout,
        stderr,
    })
}

fn output_tail(output: &[u8]) -> String {
    const MAXIMUM_CHARS: usize = 8_192;

    let output = String::from_utf8_lossy(output);
    let skip = output.chars().count().saturating_sub(MAXIMUM_CHARS);
    output.chars().skip(skip).collect()
}

fn build_environment(
    environment: &BTreeMap<String, String>,
    arguments: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut result = environment.clone();
    result.extend(arguments.clone());
    result.into_iter().collect()
}

fn docker_process_environment(
    image_environment: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut environment = image_environment.clone();
    environment
        .entry("HOME".to_owned())
        .or_insert_with(|| "/root".to_owned());
    environment
        .entry("PATH".to_owned())
        .or_insert_with(|| DEFAULT_GUEST_PATH.to_owned());
    environment
}

fn resolve_stage_index(recipe: &DockerfileRecipe, reference: &str) -> Option<usize> {
    reference.parse::<usize>().ok().or_else(|| {
        recipe
            .stages
            .iter()
            .position(|stage| stage.name.as_deref() == Some(reference))
    })
}

fn guest_block_device(index: usize) -> Result<String, ImageError> {
    let suffix = u8::try_from(index)
        .ok()
        .and_then(|index| b'a'.checked_add(index))
        .filter(u8::is_ascii_lowercase)
        .ok_or_else(|| {
            ImageError::UnsupportedDockerfile("too many build source disks".to_owned())
        })?;
    Ok(format!("/dev/vd{}", char::from(suffix)))
}

fn final_recipe_environment(
    recipe: &DockerfileRecipe,
    images: &BTreeMap<String, PulledImage>,
) -> Result<BTreeMap<String, String>, ImageError> {
    let stage = recipe.final_stage().ok_or(ImageError::InvalidFrom)?;
    let image = images
        .get(&stage.base_image)
        .ok_or_else(|| ImageError::UnknownCopySource(stage.base_image.clone()))?;
    let mut environment = docker_process_environment(&image.config.environment);
    let mut arguments = BTreeMap::new();
    for instruction in &stage.instructions {
        match instruction {
            DockerfileInstruction::Env { name, value } => {
                environment.insert(
                    name.clone(),
                    expand_variables(value, &environment, &arguments),
                );
            }
            DockerfileInstruction::Arg { name, default } => {
                arguments.insert(
                    name.clone(),
                    default
                        .as_deref()
                        .map(|value| expand_variables(value, &environment, &arguments))
                        .unwrap_or_default(),
                );
            }
            DockerfileInstruction::Run(_)
            | DockerfileInstruction::Copy(_)
            | DockerfileInstruction::Workdir(_)
            | DockerfileInstruction::Cmd(_) => {}
        }
    }
    Ok(environment)
}

fn expand_variables(
    input: &str,
    environment: &BTreeMap<String, String>,
    arguments: &BTreeMap<String, String>,
) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            output.push(char::from(bytes[index]));
            index += 1;
            continue;
        }
        if bytes.get(index + 1) == Some(&b'{') {
            let Some(end) = bytes[index + 2..].iter().position(|byte| *byte == b'}') else {
                output.push('$');
                index += 1;
                continue;
            };
            let end = index + 2 + end;
            let name = &input[index + 2..end];
            output.push_str(
                arguments
                    .get(name)
                    .or_else(|| environment.get(name))
                    .map_or("", String::as_str),
            );
            index = end + 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && (bytes[end] == b'_' || bytes[end].is_ascii_alphanumeric()) {
            end += 1;
        }
        if end == start {
            output.push('$');
            index += 1;
            continue;
        }
        let name = &input[start..end];
        output.push_str(
            arguments
                .get(name)
                .or_else(|| environment.get(name))
                .map_or("", String::as_str),
        );
        index = end;
    }
    output
}

fn build_cache_key(
    dockerfile: &str,
    context_digest: &str,
    recipe: &DockerfileRecipe,
    images: &BTreeMap<String, PulledImage>,
    disk_bytes: u64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"nanoeval-task-build\0");
    hasher.update(TASK_BUILD_CACHE_VERSION.to_le_bytes());
    hasher.update(b"linux\0arm64\0");
    hasher.update(disk_bytes.to_le_bytes());
    hasher.update(context_digest.as_bytes());
    hasher.update([0]);
    hasher.update(dockerfile.as_bytes());
    for stage in &recipe.stages {
        hasher.update([0]);
        hasher.update(stage.base_image.as_bytes());
        if let Some(image) = images.get(&stage.base_image) {
            hasher.update([0]);
            hasher.update(image.manifest_digest.as_bytes());
        }
    }
    for (reference, image) in images {
        hasher.update([0]);
        hasher.update(reference.as_bytes());
        hasher.update([0]);
        hasher.update(image.manifest_digest.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn host_resolver_configuration() -> io::Result<String> {
    for path in ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"] {
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        let configuration = resolver_configuration(&contents);
        if !configuration.is_empty() {
            return Ok(configuration);
        }
    }
    Err(io::Error::other("host resolver has no usable nameserver"))
}

fn resolver_configuration(contents: &str) -> String {
    let mut configuration = String::new();
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        let Some(address) = fields.next() else {
            continue;
        };
        let Ok(address) = address.parse::<std::net::IpAddr>() else {
            continue;
        };
        if fields.next().is_some() || address.is_loopback() || address.is_unspecified() {
            continue;
        }
        configuration.push_str("nameserver ");
        configuration.push_str(&address.to_string());
        configuration.push_str("\\n");
    }
    configuration
}

#[derive(Clone)]
struct PulledLayer {
    digest: String,
    path: PathBuf,
    media_type: String,
}

#[derive(Clone)]
struct PulledImage {
    manifest_digest: String,
    layers: Vec<PulledLayer>,
    source: ManifestSource,
    config: ImageRuntimeConfig,
}

async fn resolve_image(
    image_reference: &str,
    cache: &Path,
    policy: CachePolicy,
) -> Result<PulledImage, ImageError> {
    let reference_path = cache
        .join("references")
        .join(format!("{}.json", reference_cache_key(image_reference)));
    if policy == CachePolicy::Reuse
        && let Some(record) = read_cache_record::<ReferenceRecord>(&reference_path)?
        && let Some(image) = local_image(image_reference, cache, record)
    {
        return Ok(image);
    }

    let image = pull_layers(image_reference, &cache.join("blobs")).await?;
    let record = ReferenceRecord {
        version: CACHE_RECORD_VERSION,
        image_reference: image_reference.to_owned(),
        manifest_digest: image.manifest_digest.clone(),
        layers: image
            .layers
            .iter()
            .map(|layer| LayerRecord {
                digest: layer.digest.clone(),
                media_type: layer.media_type.clone(),
            })
            .collect(),
        config: image.config.clone(),
    };
    write_cache_record(&reference_path, &record)?;
    Ok(image)
}

fn local_image(image: &str, cache: &Path, record: ReferenceRecord) -> Option<PulledImage> {
    if record.version != CACHE_RECORD_VERSION
        || record.image_reference != image
        || !valid_digest(&record.manifest_digest)
        || record.layers.is_empty()
        || record
            .layers
            .iter()
            .any(|layer| !valid_digest(&layer.digest) || layer.media_type.is_empty())
    {
        return None;
    }
    let layers = record
        .layers
        .into_iter()
        .map(|layer| PulledLayer {
            path: blob_path(&cache.join("blobs"), &layer.digest),
            digest: layer.digest,
            media_type: layer.media_type,
        })
        .collect::<Vec<_>>();
    if layers.iter().any(|layer| !layer.path.is_file()) {
        return None;
    }
    Some(PulledImage {
        manifest_digest: record.manifest_digest,
        layers,
        source: ManifestSource::Local,
        config: record.config,
    })
}

async fn pull_layers(image: &str, blobs: &Path) -> Result<PulledImage, ImageError> {
    fs::create_dir_all(blobs)?;
    let reference = Reference::try_from(image).map_err(|source| ImageError::Reference {
        image: image.to_owned(),
        source,
    })?;
    let config = ClientConfig {
        platform_resolver: Some(Box::new(linux_guest_manifest)),
        ..ClientConfig::default()
    };
    let client = Client::new(config);
    let (manifest, manifest_digest, config_json) = client
        .pull_manifest_and_config(&reference, &RegistryAuth::Anonymous)
        .await?;
    let config = parse_image_config(&config_json)?;
    let mut layers = Vec::with_capacity(manifest.layers.len());
    for descriptor in manifest.layers {
        let path = blob_path(blobs, &descriptor.digest);
        if !path.is_file() {
            let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
            let mut output = tokio::fs::File::create(&temporary).await?;
            client
                .pull_blob(&reference, &descriptor, &mut output)
                .await?;
            drop(output);
            fs::rename(temporary, &path)?;
        }
        layers.push(PulledLayer {
            digest: descriptor.digest,
            path,
            media_type: descriptor.media_type,
        });
    }
    Ok(PulledImage {
        manifest_digest,
        layers,
        source: ManifestSource::Registry,
        config,
    })
}

fn parse_image_config(config: &str) -> Result<ImageRuntimeConfig, ImageError> {
    let config = serde_json::from_str::<ConfigFile>(config)?
        .config
        .unwrap_or_default();
    let mut environment = BTreeMap::new();
    for entry in config.env.unwrap_or_default() {
        let Some((name, value)) = entry.split_once('=') else {
            continue;
        };
        if valid_environment_name(name) {
            environment.insert(name.to_owned(), value.to_owned());
        }
    }
    let working_directory = config
        .working_dir
        .filter(|directory| valid_guest_workdir(directory))
        .unwrap_or_else(|| "/".to_owned());
    Ok(ImageRuntimeConfig {
        environment,
        working_directory,
    })
}

fn blob_path(cache: &Path, digest: &str) -> PathBuf {
    cache.join(digest.replace(':', "-"))
}

fn valid_digest(digest: &str) -> bool {
    let Some(hash) = digest.strip_prefix("sha256:") else {
        return false;
    };
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn linux_guest_manifest(manifests: &[ImageIndexEntry]) -> Option<String> {
    #[cfg(target_arch = "aarch64")]
    const GUEST_ARCHITECTURE: &str = "arm64";
    #[cfg(target_arch = "x86_64")]
    const GUEST_ARCHITECTURE: &str = "amd64";

    manifests
        .iter()
        .find(|entry| {
            entry.platform.as_ref().is_some_and(|platform| {
                platform.os.to_string() == "linux"
                    && platform.architecture.to_string() == GUEST_ARCHITECTURE
            })
        })
        .map(|entry| entry.digest.clone())
}

fn format_root_disk(path: &Path, size: u64, layers: &[PulledLayer]) -> Result<(), ImageError> {
    let mut formatter = Formatter::new(path, BLOCK_SIZE, size)?;
    for layer in layers {
        let file = File::open(&layer.path)?;
        match layer.media_type.as_str() {
            "application/vnd.docker.image.rootfs.diff.tar.gzip"
            | "application/vnd.oci.image.layer.v1.tar+gzip"
            | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip" => {
                formatter.unpack_tar(GzDecoder::new(BufReader::new(file)))?;
            }
            "application/vnd.oci.image.layer.v1.tar+zstd" => {
                formatter.unpack_tar(zstd::stream::read::Decoder::new(BufReader::new(file))?)?;
            }
            "application/vnd.docker.image.rootfs.diff.tar"
            | "application/vnd.oci.image.layer.v1.tar"
            | "application/vnd.oci.image.layer.nondistributable.v1.tar" => {
                formatter.unpack_tar(BufReader::new(file))?;
            }
            media_type => return Err(ImageError::UnsupportedLayer(media_type.to_owned())),
        }
    }
    formatter.close()?;
    Ok(())
}

fn validate_root_disk(path: &Path) -> Result<(), ImageError> {
    validate_ext4_disk(path)?;
    let mut reader = Reader::new(path)?;
    for required in ["/bin/sh"] {
        if !reader.exists(required) {
            return Err(ImageError::MissingPreparedPath(required));
        }
    }
    Ok(())
}

fn cached_prepared_shell(path: &Path) -> Result<String, ImageError> {
    let metadata = fs::metadata(path)?;
    let modified_nanos = modified_nanos(&metadata)?;
    let record_path = path.with_extension("prepared.json");
    if let Some(record) = read_cache_record::<PreparedDiskRecord>(&record_path)?
        && record.version == PREPARED_DISK_RECORD_VERSION
        && record.file_bytes == metadata.len()
        && record.modified_nanos == modified_nanos
        && matches!(record.shell.as_str(), "bash" | "sh")
    {
        return Ok(record.shell);
    }

    let shell = prepared_shell(path)?.to_owned();
    write_cache_record(
        &record_path,
        &PreparedDiskRecord {
            version: PREPARED_DISK_RECORD_VERSION,
            file_bytes: metadata.len(),
            modified_nanos,
            shell: shell.clone(),
        },
    )?;
    Ok(shell)
}

fn modified_nanos(metadata: &fs::Metadata) -> io::Result<u128> {
    metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(io::Error::other)
}

fn prepared_shell(path: &Path) -> Result<&'static str, ImageError> {
    let mut reader = Reader::new(path)?;
    if reader.exists("/bin/bash") {
        Ok("bash")
    } else if reader.exists("/bin/sh") {
        Ok("sh")
    } else {
        Err(ImageError::MissingPreparedPath("/bin/sh"))
    }
}

fn validate_ext4_disk(path: &Path) -> Result<(), ImageError> {
    let mut reader = Reader::new(path)?;
    if !reader.exists("/") {
        return Err(ImageError::MissingPreparedPath("/"));
    }
    Ok(())
}

fn disk_cache_key(manifest_digest: &str, disk_bytes: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"nanoeval-ext4-v3\0linux\0arm64\0");
    hasher.update(manifest_digest.as_bytes());
    hasher.update([0]);
    hasher.update(disk_bytes.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

fn reference_cache_key(image: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"nanoeval-reference-v1\0linux\0arm64\0");
    hasher.update(image.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, process::Command};

    use super::{
        COPY_SCRIPT, DockerfileRecipe, disk_cache_key, docker_process_environment, output_tail,
        reference_cache_key, resolver_configuration,
    };

    #[test]
    fn accepts_the_first_proof_dockerfile() {
        let recipe =
            DockerfileRecipe::parse("FROM python:3.13-slim-bookworm\nWORKDIR /app\n").unwrap();

        assert_eq!(
            recipe.final_stage().unwrap().base_image,
            "python:3.13-slim-bookworm"
        );
        assert_eq!(recipe.final_workdir(), "/app");
        assert!(!recipe.requires_build());
    }

    #[test]
    fn supplies_docker_root_process_defaults() {
        let environment = docker_process_environment(&BTreeMap::new());

        assert_eq!(environment.get("HOME").map(String::as_str), Some("/root"));
        assert_eq!(
            environment.get("PATH").map(String::as_str),
            Some(super::DEFAULT_GUEST_PATH)
        );
    }

    #[test]
    fn resolver_configuration_rejects_host_local_stubs() {
        assert_eq!(
            resolver_configuration(
                "nameserver 127.0.0.53\nnameserver ::1\nnameserver 213.186.33.99\n"
            ),
            "nameserver 213.186.33.99\\n"
        );
    }

    #[test]
    fn copy_creates_a_missing_directory_for_a_trailing_slash_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("client.conf");
        let destination = format!("{}/", temporary.path().join("etc/pipewire").display());
        fs::write(&source, "configured").unwrap();

        let status = Command::new("/bin/sh")
            .args([
                "-c",
                COPY_SCRIPT,
                "nanoeval-copy",
                &destination,
                source.to_str().unwrap(),
            ])
            .status()
            .unwrap();

        assert!(status.success());
        assert_eq!(
            fs::read_to_string(temporary.path().join("etc/pipewire/client.conf")).unwrap(),
            "configured"
        );
    }

    #[test]
    fn parses_the_complete_terminal_bench_instruction_shape() {
        let recipe = DockerfileRecipe::parse(
            r#"FROM ubuntu:24.04 AS build
ARG SOURCE=https://example.com/input
ENV MODE=test
RUN apt-get update && \
    apt-get install -y curl
COPY input.txt /root/
FROM ubuntu:24.04 AS target
COPY --from=build /root/input.txt /app/input.txt
WORKDIR /app
CMD ["/bin/sh"]
"#,
        )
        .unwrap();

        assert_eq!(recipe.stages.len(), 2);
        assert_eq!(recipe.stages[0].name.as_deref(), Some("build"));
        assert_eq!(recipe.stages[1].name.as_deref(), Some("target"));
        assert_eq!(recipe.final_workdir(), "/app");
        assert!(recipe.requires_build());
        let super::DockerfileInstruction::Copy(copy) = &recipe.stages[1].instructions[0] else {
            panic!("expected the final-stage COPY instruction");
        };
        assert_eq!(copy.from.as_deref(), Some("build"));
        assert_eq!(copy.sources, ["/root/input.txt"]);
        assert_eq!(copy.destination, "/app/input.txt");
    }

    #[test]
    fn rejects_workdir_parent_traversal() {
        let error =
            DockerfileRecipe::parse("FROM python:3.13-slim-bookworm\nWORKDIR /app/../root\n")
                .err()
                .unwrap();

        assert!(error.to_string().contains("WORKDIR /app/../root"));
    }

    #[test]
    fn flattened_disk_identity_is_task_recipe_independent() {
        let digest = "sha256:56249d7a2f93306106f6d8bcdf6423afb73c1b747d874febcc778beee25cb8bb";
        let first =
            DockerfileRecipe::parse("FROM python:3.13-slim-bookworm\nWORKDIR /app\n").unwrap();
        let second =
            DockerfileRecipe::parse("FROM python:3.13-slim-bookworm\nWORKDIR /workspace\n")
                .unwrap();

        assert_ne!(first.final_workdir(), second.final_workdir());
        assert_eq!(
            first.final_stage().unwrap().base_image,
            second.final_stage().unwrap().base_image
        );
        let first_key = disk_cache_key(digest, 1024);
        let second_key = disk_cache_key(digest, 1024);
        assert_eq!(first_key, second_key);
        assert_ne!(disk_cache_key(digest, 1024), disk_cache_key(digest, 2048));
        assert_ne!(
            reference_cache_key("python:3.13-slim-bookworm"),
            reference_cache_key("python:3.12-slim-bookworm")
        );
    }

    #[test]
    fn build_failure_diagnostics_keep_the_output_tail() {
        let mut output = vec![b'a'; 8_192];
        output.extend_from_slice(b"final compiler error");

        let retained = output_tail(&output);

        assert_eq!(retained.chars().count(), 8_192);
        assert!(retained.ends_with("final compiler error"));
    }
}
