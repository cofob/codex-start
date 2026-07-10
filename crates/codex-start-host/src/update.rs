//! Signed GitHub Release discovery and host-binary updates.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{IsTerminal, Read},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(not(windows))]
use flate2::read::GzDecoder;
use fs2::FileExt as _;
use reqwest::{
    Client, StatusCode,
    header::{ACCEPT, ETAG, HeaderValue, IF_NONE_MATCH, USER_AGENT},
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
#[cfg(not(windows))]
use uuid::Uuid;

#[cfg(not(windows))]
use std::path::Component;

use crate::{
    cli::{Cli, Command as CliCommand, OutputFormat, SessionCommand, UpdateApplyArgs, UpdateArgs},
    command::{CommandSpec, run_capture},
    configuration::ConfigContext,
    error::{HostError, Result},
    paths::{atomic_write, set_private_file},
};

const REPOSITORY: &str = "cofob/codex-start";
const API_ROOT: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const USER_AGENT_VALUE: &str = concat!("codex-start/", env!("CARGO_PKG_VERSION"));
const STATE_SCHEMA_VERSION: u32 = 1;
const MANIFEST_SCHEMA_VERSION: u32 = 1;
const RECEIPT_SCHEMA_VERSION: u32 = 1;
const METADATA_LIMIT: usize = 4 * 1024 * 1024;
const ARTIFACT_LIMIT: usize = 512 * 1024 * 1024;
const EXECUTABLE_LIMIT: u64 = 256 * 1024 * 1024;
const FAILED_RETRY_SECONDS: u64 = 60 * 60;
const METADATA_TIMEOUT: Duration = Duration::from_secs(8);
const ARTIFACT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_ASSET_PAGES: u32 = 4;
pub(crate) const UPDATE_REEXEC_ENV: &str = "CODEX_START_UPDATE_REEXEC";

#[derive(Clone, Debug, Deserialize)]
struct GitHubRelease {
    id: u64,
    tag_name: String,
    html_url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubAsset {
    id: u64,
    name: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct AvailableRelease {
    pub tag: String,
    pub version: Version,
    pub url: String,
    assets: BTreeMap<String, GitHubAsset>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseManifest {
    schema_version: u32,
    version: String,
    tag: String,
    artifacts: Vec<ReleaseArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseArtifact {
    kind: String,
    os: String,
    arch: String,
    libc: Option<String>,
    filename: String,
    size: u64,
    sha256: String,
    bundle: String,
    sbom: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct UpdateState {
    schema_version: u32,
    last_attempt_unix_seconds: Option<u64>,
    last_success_unix_seconds: Option<u64>,
    etag: Option<String>,
    latest_tag: Option<String>,
    latest_url: Option<String>,
    last_prompt_unix_seconds: Option<u64>,
    skipped_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallReceipt {
    schema_version: u32,
    method: String,
    target: String,
    executable: PathBuf,
    require_signature: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UpdateCheck {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub release_url: String,
}

enum LatestResponse {
    NotModified,
    Release(AvailableRelease, Option<String>),
}

struct UpdateLock(fs::File);

impl UpdateLock {
    fn acquire(context: &ConfigContext, wait: bool) -> Result<Option<Self>> {
        let path = context.paths.runtime_dir().join("update.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| HostError::io(&path, source))?;
        if wait {
            file.lock_exclusive()
                .map_err(|source| HostError::io(&path, source))?;
            Ok(Some(Self(file)))
        } else {
            match file.try_lock_exclusive() {
                Ok(()) => Ok(Some(Self(file))),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(source) => Err(HostError::io(&path, source)),
            }
        }
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

struct ReleaseClient {
    client: Client,
    api_root: String,
}

impl ReleaseClient {
    fn github() -> Result<Self> {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            drop(rustls::crypto::aws_lc_rs::default_provider().install_default());
        }
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            return Err(HostError::Runtime(
                "install the TLS cryptography provider".to_owned(),
            ));
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 8 {
                    return attempt.error("too many release-asset redirects");
                }
                let url = attempt.url();
                let trusted = url.scheme() == "https"
                    && url.host_str().is_some_and(|host| {
                        host == "github.com"
                            || host == "api.github.com"
                            || host.ends_with(".githubusercontent.com")
                    });
                if trusted {
                    attempt.follow()
                } else {
                    attempt.error("release asset redirected outside GitHub")
                }
            }))
            .https_only(true)
            .build()
            .map_err(|error| HostError::Runtime(format!("build update HTTP client: {error}")))?;
        Ok(Self {
            client,
            api_root: API_ROOT.to_owned(),
        })
    }

    async fn latest(&self, etag: Option<&str>) -> Result<LatestResponse> {
        let endpoint = format!("{}/repos/{REPOSITORY}/releases/latest", self.api_root);
        let mut request = self
            .client
            .get(endpoint)
            .timeout(METADATA_TIMEOUT)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION);
        if let Some(etag) = etag {
            let value = HeaderValue::from_str(etag)
                .map_err(|_| HostError::Config("cached update ETag is invalid".to_owned()))?;
            request = request.header(IF_NONE_MATCH, value);
        }
        let response = request
            .send()
            .await
            .map_err(|error| HostError::Runtime(format!("request latest release: {error}")))?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(LatestResponse::NotModified);
        }
        let response = response.error_for_status().map_err(|error| {
            HostError::Runtime(format!("GitHub latest-release request failed: {error}"))
        })?;
        ensure_content_length(&response, METADATA_LIMIT)?;
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = read_limited(response, METADATA_LIMIT, "latest release").await?;
        let release: GitHubRelease = serde_json::from_slice(&bytes)
            .map_err(|error| HostError::Serialization(format!("latest release: {error}")))?;
        let assets = self.assets(release.id).await?;
        Ok(LatestResponse::Release(
            parse_release(release, assets)?,
            etag,
        ))
    }

    async fn assets(&self, release_id: u64) -> Result<Vec<GitHubAsset>> {
        let mut assets = Vec::new();
        for page in 1..=MAX_ASSET_PAGES {
            let endpoint = format!(
                "{}/repos/{REPOSITORY}/releases/{release_id}/assets?per_page=100&page={page}",
                self.api_root
            );
            let response = self
                .client
                .get(endpoint)
                .timeout(METADATA_TIMEOUT)
                .header(USER_AGENT, USER_AGENT_VALUE)
                .header(ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", API_VERSION)
                .send()
                .await
                .map_err(|error| HostError::Runtime(format!("list release assets: {error}")))?
                .error_for_status()
                .map_err(|error| {
                    HostError::Runtime(format!("GitHub release-assets request failed: {error}"))
                })?;
            ensure_content_length(&response, METADATA_LIMIT)?;
            let bytes = read_limited(response, METADATA_LIMIT, "release assets").await?;
            let mut page_assets: Vec<GitHubAsset> = serde_json::from_slice(&bytes)
                .map_err(|error| HostError::Serialization(format!("release assets: {error}")))?;
            let complete = page_assets.len() < 100;
            assets.append(&mut page_assets);
            if complete {
                return Ok(assets);
            }
        }
        Err(HostError::Runtime(format!(
            "release has more than {} assets",
            MAX_ASSET_PAGES * 100
        )))
    }

    async fn download(
        &self,
        release: &AvailableRelease,
        name: &str,
        limit: usize,
    ) -> Result<Vec<u8>> {
        let asset = release
            .assets
            .get(name)
            .ok_or_else(|| HostError::NotFound(format!("release asset {name:?}")))?;
        if asset.size > limit as u64 {
            return Err(HostError::Runtime(format!(
                "release asset {name:?} is larger than the {limit}-byte limit"
            )));
        }
        let endpoint = format!(
            "{}/repos/{REPOSITORY}/releases/assets/{}",
            self.api_root, asset.id
        );
        let response = self
            .client
            .get(endpoint)
            .timeout(ARTIFACT_TIMEOUT)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, "application/octet-stream")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .map_err(|error| HostError::Runtime(format!("download {name}: {error}")))?
            .error_for_status()
            .map_err(|error| HostError::Runtime(format!("download {name}: {error}")))?;
        ensure_content_length(&response, limit)?;
        let bytes = read_limited(response, limit, name).await?;
        if bytes.len() as u64 != asset.size {
            return Err(HostError::Runtime(format!(
                "release asset {name:?} size differs from GitHub metadata"
            )));
        }
        Ok(bytes)
    }
}

async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
    label: &str,
) -> Result<Vec<u8>> {
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(16 * 1024)
        .min(limit);
    let mut bytes = Vec::with_capacity(capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| HostError::Runtime(format!("read {label}: {error}")))?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(HostError::Runtime(format!(
                "update response {label:?} exceeded its {limit}-byte limit"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn ensure_content_length(response: &reqwest::Response, limit: usize) -> Result<()> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(HostError::Runtime(format!(
            "update response exceeds the {limit}-byte limit"
        )));
    }
    Ok(())
}

fn parse_release(
    release: GitHubRelease,
    release_assets: Vec<GitHubAsset>,
) -> Result<AvailableRelease> {
    let raw = release.tag_name.strip_prefix('v').ok_or_else(|| {
        HostError::Runtime(format!(
            "latest release tag is not v-prefixed: {}",
            release.tag_name
        ))
    })?;
    let version = Version::parse(raw)
        .map_err(|error| HostError::Runtime(format!("invalid latest release version: {error}")))?;
    if !version.pre.is_empty() {
        return Err(HostError::Runtime(
            "GitHub latest release unexpectedly refers to a prerelease".to_owned(),
        ));
    }
    let mut assets = BTreeMap::new();
    for asset in release_assets {
        let name = asset.name.clone();
        if assets.insert(name.clone(), asset).is_some() {
            return Err(HostError::Runtime(format!(
                "latest release contains duplicate asset {name:?}"
            )));
        }
    }
    Ok(AvailableRelease {
        tag: release.tag_name,
        version,
        url: release.html_url,
        assets,
    })
}

pub(crate) async fn execute(
    context: &ConfigContext,
    args: UpdateArgs,
    output: OutputFormat,
) -> Result<u8> {
    let _lock = UpdateLock::acquire(context, true)?.expect("blocking update lock");
    let policy = context.resolve_update_policy()?;
    let client = ReleaseClient::github()?;
    let release = match client.latest(None).await? {
        LatestResponse::Release(release, _) => release,
        LatestResponse::NotModified => {
            return Err(HostError::Runtime(
                "GitHub returned not-modified without a cached release".to_owned(),
            ));
        }
    };
    let check = update_check(&release)?;
    if args.check || !check.available {
        emit_check(output, &check)?;
        return Ok(0);
    }
    if !args.yes {
        if output != OutputFormat::Human
            || !std::io::IsTerminal::is_terminal(&std::io::stdin())
            || !std::io::IsTerminal::is_terminal(&std::io::stderr())
        {
            return Err(HostError::Usage(
                "a newer release is available; rerun `codex-start update --yes` to install it"
                    .to_owned(),
            ));
        }
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!(
                "Update codex-start from {} to {}?",
                check.current, check.latest
            ))
            .default(true)
            .interact()
            .map_err(|error| HostError::Runtime(format!("update confirmation: {error}")))?;
        if !confirmed {
            println!("update cancelled");
            return Ok(0);
        }
    }
    let require_signature = args.require_signature || policy.require_signature;
    let result = install_release(context, &client, &release, require_signature, false).await?;
    emit_updated(output, &check.current, &check.latest, &result)?;
    Ok(0)
}

/// Best-effort cleanup for deferred Windows updater directories.
pub(crate) fn cleanup_stale(context: &ConfigContext) {
    let root = context.paths.runtime_dir();
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(24 * 60 * 60))
        .unwrap_or(UNIX_EPOCH);
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("update-") {
            continue;
        }
        let path = entry.path();
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified < cutoff);
        if old && let Err(error) = fs::remove_dir_all(&path) {
            tracing::debug!(path = %path.display(), %error, "could not remove stale update directory");
        }
    }
}

/// Check for a newer release before an eligible interactive command.
///
/// Returns `true` only when the original command has been replaced by a newly
/// installed executable and the caller must not dispatch the old command.
#[allow(clippy::too_many_lines)]
pub(crate) async fn maybe_prompt(context: &ConfigContext, cli: &Cli) -> Result<bool> {
    if !automatic_check_eligible(cli) {
        return Ok(false);
    }
    let resolved = context.resolve(None)?;
    if !resolved.config.updates.enabled
        || resolved.config.network == codex_start_core::NetworkMode::Offline
    {
        return Ok(false);
    }
    let Some(_lock) = (match UpdateLock::acquire(context, false) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::debug!(%error, "automatic update lock is unavailable");
            return Ok(false);
        }
    }) else {
        return Ok(false);
    };
    let mut state = match load_state(context) {
        Ok(state) => state,
        Err(error) => {
            tracing::debug!(%error, "discarding unreadable automatic update state");
            UpdateState {
                schema_version: STATE_SCHEMA_VERSION,
                ..UpdateState::default()
            }
        }
    };
    if !automatic_check_due(&state, resolved.config.updates.check_interval_hours) {
        return Ok(false);
    }
    let now = now_seconds();
    state.last_attempt_unix_seconds = Some(now);
    save_automatic_state(context, &state);
    let client = match ReleaseClient::github() {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!(%error, "automatic update client is unavailable");
            return Ok(false);
        }
    };
    let response = match client.latest(state.etag.as_deref()).await {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(%error, "automatic update check failed");
            return Ok(false);
        }
    };
    let release = match response {
        LatestResponse::Release(release, etag) => {
            state.etag = etag;
            state.latest_tag = Some(release.tag.clone());
            state.latest_url = Some(release.url.clone());
            Some(release)
        }
        LatestResponse::NotModified => match cached_release(&state) {
            Ok(release) => release,
            Err(error) => {
                tracing::debug!(%error, "cached automatic update metadata is unreadable");
                return Ok(false);
            }
        },
    };
    state.last_success_unix_seconds = Some(now);
    save_automatic_state(context, &state);
    let Some(mut release) = release else {
        return Ok(false);
    };
    let check = update_check(&release)?;
    if !check.available
        || state
            .skipped_version
            .as_deref()
            .is_some_and(|version| version == check.latest)
    {
        return Ok(false);
    }
    let choices = [
        format!("Update now to {}", check.latest),
        "Later".to_owned(),
        format!("Skip version {}", check.latest),
        "Disable automatic update checks".to_owned(),
    ];
    let choice = dialoguer::Select::new()
        .with_prompt(format!(
            "A new codex-start release is available ({} → {})",
            check.current, check.latest
        ))
        .items(&choices)
        .default(0)
        .interact_opt()
        .map_err(|error| HostError::Runtime(format!("update prompt: {error}")))?;
    match choice {
        Some(0) => {
            if release.assets.is_empty() {
                let refreshed = match client.latest(None).await? {
                    LatestResponse::Release(release, _) => release,
                    LatestResponse::NotModified => {
                        return Err(HostError::Runtime(
                            "GitHub omitted release assets during update".to_owned(),
                        ));
                    }
                };
                if refreshed.tag != release.tag || refreshed.version != release.version {
                    return Err(HostError::Runtime(
                        "the latest release changed after the update prompt; rerun the command to review the new version"
                            .to_owned(),
                    ));
                }
                release = refreshed;
            }
            let outcome = install_release(
                context,
                &client,
                &release,
                resolved.config.updates.require_signature,
                true,
            )
            .await?;
            if outcome.pending {
                println!(
                    "verified codex-start {} and staged the update using {}; applying it and restarting the command",
                    check.latest, outcome.method
                );
            } else {
                println!(
                    "updated codex-start from {} to {} using {}; restarting command",
                    check.current, check.latest, outcome.method
                );
            }
            #[cfg(windows)]
            {
                Ok(true)
            }
            #[cfg(not(windows))]
            {
                reexecute_original()?;
                Ok(true)
            }
        }
        Some(1) | None => {
            state.last_prompt_unix_seconds = Some(now);
            save_automatic_state(context, &state);
            Ok(false)
        }
        Some(2) => {
            state.skipped_version = Some(check.latest);
            state.last_prompt_unix_seconds = Some(now);
            save_automatic_state(context, &state);
            Ok(false)
        }
        Some(3) => {
            context.set(true, "updates.enabled", "false")?;
            println!("automatic update checks disabled");
            Ok(false)
        }
        Some(_) => Err(HostError::Runtime(
            "update prompt returned an invalid choice".to_owned(),
        )),
    }
}

fn automatic_check_eligible(cli: &Cli) -> bool {
    if cli.quiet
        || cli.output != OutputFormat::Human
        || !std::io::stdin().is_terminal()
        || !std::io::stderr().is_terminal()
        || std::env::var_os("CI").is_some()
        || std::env::var_os(UPDATE_REEXEC_ENV).is_some()
        || std::env::var_os("CODEX_START_SESSION_WORKER").is_some()
    {
        return false;
    }
    match &cli.command {
        Some(CliCommand::Update(_) | CliCommand::UpdateApply(_) | CliCommand::Config(_)) => false,
        Some(CliCommand::Run(args)) => !args.options.dry_run && !args.options.offline,
        Some(CliCommand::Shell(args)) => !args.options.dry_run && !args.options.offline,
        Some(CliCommand::Merge(args)) => !args.options.dry_run && !args.options.offline,
        Some(CliCommand::Session(args)) => !matches!(
            &args.command,
            Some(SessionCommand::Start(run)) if run.options.dry_run || run.options.offline
        ),
        _ => true,
    }
}

fn cached_release(state: &UpdateState) -> Result<Option<AvailableRelease>> {
    let (Some(tag), Some(url)) = (&state.latest_tag, &state.latest_url) else {
        return Ok(None);
    };
    let version = Version::parse(tag.strip_prefix('v').ok_or_else(|| {
        HostError::Serialization("cached release tag is not v-prefixed".to_owned())
    })?)
    .map_err(|error| HostError::Serialization(format!("cached release version: {error}")))?;
    Ok(Some(AvailableRelease {
        tag: tag.clone(),
        version,
        url: url.clone(),
        assets: BTreeMap::new(),
    }))
}

#[cfg(not(windows))]
fn reexecute_original() -> Result<()> {
    let executable =
        std::env::current_exe().map_err(|source| HostError::io("current executable", source))?;
    let mut command = Command::new(&executable);
    command
        .args(std::env::args_os().skip(1))
        .env(UPDATE_REEXEC_ENV, "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let error = command.exec();
        Err(HostError::CommandIo {
            program: executable.into_os_string(),
            source: error,
        })
    }
    #[cfg(not(unix))]
    {
        command.spawn().map_err(|source| HostError::CommandIo {
            program: executable.into_os_string(),
            source,
        })?;
        Ok(())
    }
}

fn update_check(release: &AvailableRelease) -> Result<UpdateCheck> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|error| HostError::Serialization(format!("current version: {error}")))?;
    Ok(UpdateCheck {
        current: current.to_string(),
        latest: release.version.to_string(),
        available: release.version > current,
        release_url: release.url.clone(),
    })
}

fn emit_check(output: OutputFormat, check: &UpdateCheck) -> Result<()> {
    match output {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string(check)
                .map_err(|error| HostError::Serialization(error.to_string()))?
        ),
        OutputFormat::Human if check.available => println!(
            "codex-start {} is available (current {}): {}",
            check.latest, check.current, check.release_url
        ),
        OutputFormat::Human => println!("codex-start {} is current", check.current),
    }
    Ok(())
}

#[derive(Serialize)]
struct UpdateResult<'a> {
    from: &'a str,
    to: &'a str,
    method: &'a str,
    updated: bool,
    pending: bool,
}

struct InstallOutcome {
    method: &'static str,
    pending: bool,
}

fn emit_updated(
    output: OutputFormat,
    from: &str,
    to: &str,
    outcome: &InstallOutcome,
) -> Result<()> {
    match output {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string(&UpdateResult {
                from,
                to,
                method: outcome.method,
                updated: !outcome.pending,
                pending: outcome.pending,
            })
            .map_err(|error| HostError::Serialization(error.to_string()))?
        ),
        OutputFormat::Human if outcome.pending => println!(
            "verified codex-start {to} and staged the update using {}; it will be applied after this process exits",
            outcome.method
        ),
        OutputFormat::Human => println!(
            "updated codex-start from {from} to {to} using {}",
            outcome.method
        ),
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn install_release(
    context: &ConfigContext,
    client: &ReleaseClient,
    release: &AvailableRelease,
    require_signature: bool,
    restart: bool,
) -> Result<InstallOutcome> {
    let executable =
        std::env::current_exe().map_err(|source| HostError::io("current executable", source))?;
    let receipt = load_receipt(context, &executable)?;
    let require_signature = require_signature
        || receipt
            .as_ref()
            .is_some_and(|receipt| receipt.require_signature);
    let method = receipt.as_ref().map_or_else(
        || detect_install_method(&executable),
        |receipt| receipt.method.clone(),
    );
    preflight_install(&method, &executable)?;
    let temporary = tempfile::Builder::new()
        .prefix("update-")
        .tempdir_in(context.paths.runtime_dir())
        .map_err(|source| HostError::io(context.paths.runtime_dir(), source))?;
    let checksums = download_to(
        client,
        release,
        "SHA256SUMS",
        METADATA_LIMIT,
        temporary.path(),
    )
    .await?;
    let cosign = cosign_available();
    if require_signature || cosign {
        let bundle = download_to(
            client,
            release,
            "SHA256SUMS.bundle",
            METADATA_LIMIT,
            temporary.path(),
        )
        .await?;
        verify_sigstore(&checksums, &bundle, &release.tag, require_signature)?;
    }
    let sums = parse_checksums(
        &fs::read_to_string(&checksums).map_err(|source| HostError::io(&checksums, source))?,
    )?;
    let manifest_path = download_to(
        client,
        release,
        "release-manifest.json",
        METADATA_LIMIT,
        temporary.path(),
    )
    .await?;
    verify_file_checksum(
        &manifest_path,
        checksum_for(&sums, "release-manifest.json")?,
    )?;
    let manifest: ReleaseManifest = serde_json::from_slice(
        &fs::read(&manifest_path).map_err(|source| HostError::io(&manifest_path, source))?,
    )
    .map_err(|error| HostError::Serialization(format!("release manifest: {error}")))?;
    validate_manifest(&manifest, release)?;
    let artifact = select_artifact(&manifest, &method)?;
    let artifact_path = download_to(
        client,
        release,
        &artifact.filename,
        ARTIFACT_LIMIT,
        temporary.path(),
    )
    .await?;
    let expected = checksum_for(&sums, &artifact.filename)?;
    if expected != artifact.sha256 {
        return Err(HostError::Runtime(format!(
            "release manifest checksum for {} differs from SHA256SUMS",
            artifact.filename
        )));
    }
    verify_file_checksum(&artifact_path, expected)?;
    if let Some(asset) = release.assets.get(&artifact.filename)
        && let Some(digest) = &asset.digest
        && digest
            .strip_prefix("sha256:")
            .is_some_and(|value| value != expected)
    {
        return Err(HostError::Runtime(format!(
            "GitHub digest for {} differs from the signed release metadata",
            artifact.filename
        )));
    }
    if require_signature || cosign {
        let bundle_path = download_to(
            client,
            release,
            &artifact.bundle,
            METADATA_LIMIT,
            temporary.path(),
        )
        .await?;
        verify_sigstore(
            &artifact_path,
            &bundle_path,
            &release.tag,
            require_signature,
        )?;
    }
    match method.as_str() {
        "deb" | "rpm" | "apk" => install_package(&method, &artifact_path)?,
        _ => install_portable(&artifact_path, &executable, temporary, restart)?,
    }
    Ok(InstallOutcome {
        method: match method.as_str() {
            "deb" => "apt",
            "rpm" => "dnf/rpm",
            "apk" => "apk",
            _ => "portable archive",
        },
        pending: cfg!(windows) && method == "portable",
    })
}

#[allow(clippy::unnecessary_wraps)]
fn preflight_install(method: &str, executable: &Path) -> Result<()> {
    #[cfg(windows)]
    if method == "portable" {
        let parent = executable.parent().ok_or_else(|| HostError::UnsafePath {
            path: executable.to_path_buf(),
            reason: "executable has no parent directory".to_owned(),
        })?;
        tempfile::NamedTempFile::new_in(parent).map_err(|source| {
            HostError::Usage(format!(
                "cannot update {} with the current Windows permissions ({source}); rerun codex-start from an elevated terminal",
                executable.display()
            ))
        })?;
    }
    #[cfg(not(windows))]
    let _ = (method, executable);
    Ok(())
}

async fn download_to(
    client: &ReleaseClient,
    release: &AvailableRelease,
    name: &str,
    limit: usize,
    directory: &Path,
) -> Result<PathBuf> {
    if !safe_asset_name(name) {
        return Err(HostError::Runtime(format!(
            "unsafe release asset name {name:?}"
        )));
    }
    let bytes = client.download(release, name, limit).await?;
    let path = directory.join(name);
    fs::write(&path, bytes).map_err(|source| HostError::io(&path, source))?;
    set_private_file(&path)?;
    Ok(path)
}

fn safe_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
        && name != "."
        && name != ".."
}

fn parse_checksums(contents: &str) -> Result<BTreeMap<String, String>> {
    let mut sums = BTreeMap::new();
    for (index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (checksum, name) = line.split_once("  ").ok_or_else(|| {
            HostError::Runtime(format!("SHA256SUMS line {} is invalid", index + 1))
        })?;
        if checksum.len() != 64
            || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !safe_asset_name(name)
        {
            return Err(HostError::Runtime(format!(
                "SHA256SUMS line {} is invalid",
                index + 1
            )));
        }
        if sums
            .insert(name.to_owned(), checksum.to_ascii_lowercase())
            .is_some()
        {
            return Err(HostError::Runtime(format!("SHA256SUMS repeats {name:?}")));
        }
    }
    if sums.is_empty() {
        return Err(HostError::Runtime("SHA256SUMS is empty".to_owned()));
    }
    Ok(sums)
}

fn checksum_for<'a>(sums: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str> {
    sums.get(name)
        .map(String::as_str)
        .ok_or_else(|| HostError::NotFound(format!("checksum for release asset {name:?}")))
}

fn verify_file_checksum(path: &Path, expected: &str) -> Result<()> {
    let mut file = fs::File::open(path).map_err(|source| HostError::io(path, source))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| HostError::io(path, source))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual = hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        });
    if actual != expected {
        return Err(HostError::Runtime(format!(
            "SHA-256 mismatch for {}: expected {expected}, calculated {actual}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_manifest(manifest: &ReleaseManifest, release: &AvailableRelease) -> Result<()> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(HostError::Runtime(format!(
            "unsupported release manifest schema {}",
            manifest.schema_version
        )));
    }
    if manifest.tag != release.tag || manifest.version != release.version.to_string() {
        return Err(HostError::Runtime(
            "release manifest version does not match GitHub release".to_owned(),
        ));
    }
    if manifest.artifacts.is_empty() || manifest.artifacts.len() > 64 {
        return Err(HostError::Runtime(
            "release manifest has an invalid artifact count".to_owned(),
        ));
    }
    let mut names = std::collections::BTreeSet::new();
    for artifact in &manifest.artifacts {
        if !safe_asset_name(&artifact.filename)
            || !safe_asset_name(&artifact.bundle)
            || !safe_asset_name(&artifact.sbom)
            || artifact.bundle != format!("{}.bundle", artifact.filename)
            || artifact.sbom != format!("{}.spdx.json", artifact.filename)
            || artifact.sha256.len() != 64
            || !artifact
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            || artifact.size == 0
            || artifact.size > ARTIFACT_LIMIT as u64
            || !valid_artifact_platform(artifact)
            || !names.insert(&artifact.filename)
        {
            return Err(HostError::Runtime(
                "release manifest contains an invalid artifact".to_owned(),
            ));
        }
        let asset = release.assets.get(&artifact.filename).ok_or_else(|| {
            HostError::NotFound(format!("GitHub release asset {:?}", artifact.filename))
        })?;
        if asset.size != artifact.size {
            return Err(HostError::Runtime(format!(
                "release manifest size for {} differs from GitHub metadata",
                artifact.filename
            )));
        }
        for related in [&artifact.bundle, &artifact.sbom] {
            if !release.assets.contains_key(related) {
                return Err(HostError::NotFound(format!(
                    "GitHub release asset {related:?}"
                )));
            }
        }
    }
    Ok(())
}

fn valid_artifact_platform(artifact: &ReleaseArtifact) -> bool {
    matches!(
        (
            artifact.kind.as_str(),
            artifact.os.as_str(),
            artifact.arch.as_str(),
            artifact.libc.as_deref(),
        ),
        (
            "archive",
            "linux",
            "x86_64" | "aarch64",
            Some("gnu" | "musl")
        ) | ("archive", "macos" | "windows", "x86_64" | "aarch64", None)
            | ("deb" | "rpm", "linux", "x86_64" | "aarch64", Some("gnu"))
            | ("apk", "linux", "x86_64" | "aarch64", Some("musl"))
            | ("installer", "posix" | "windows", "any", None)
    )
}

fn select_artifact<'a>(manifest: &'a ReleaseManifest, method: &str) -> Result<&'a ReleaseArtifact> {
    let os = match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        "linux" => "linux",
        other => {
            return Err(HostError::Usage(format!(
                "updates are not supported on {other}"
            )));
        }
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => {
            return Err(HostError::Usage(format!(
                "updates are not supported on architecture {other}"
            )));
        }
    };
    let (kind, libc) = match method {
        "deb" => ("deb", Some("gnu")),
        "rpm" => ("rpm", Some("gnu")),
        "apk" => ("apk", Some("musl")),
        _ if os == "linux" => (
            "archive",
            Some(if cfg!(target_env = "musl") {
                "musl"
            } else {
                "gnu"
            }),
        ),
        _ => ("archive", None),
    };
    let mut matches = manifest.artifacts.iter().filter(|artifact| {
        artifact.kind == kind
            && artifact.os == os
            && artifact.arch == arch
            && artifact.libc.as_deref() == libc
    });
    let artifact = matches
        .next()
        .ok_or_else(|| HostError::NotFound(format!("release artifact for {os}/{arch}/{kind}")))?;
    if matches.next().is_some() {
        return Err(HostError::Runtime(
            "release manifest contains multiple matching artifacts".to_owned(),
        ));
    }
    Ok(artifact)
}

fn cosign_available() -> bool {
    Command::new("cosign")
        .arg("version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn verify_sigstore(path: &Path, bundle: &Path, tag: &str, required: bool) -> Result<()> {
    if !cosign_available() {
        if required {
            return Err(HostError::ExecutableMissing(
                "cosign is required by the update policy".to_owned(),
            ));
        }
        return Ok(());
    }
    let identity =
        format!("https://github.com/{REPOSITORY}/.github/workflows/release.yml@refs/tags/{tag}");
    run_capture(&CommandSpec::new("cosign").args([
        OsString::from("verify-blob"),
        OsString::from("--bundle"),
        bundle.as_os_str().to_owned(),
        OsString::from("--certificate-identity"),
        OsString::from(identity),
        OsString::from("--certificate-oidc-issuer"),
        OsString::from("https://token.actions.githubusercontent.com"),
        path.as_os_str().to_owned(),
    ]))?
    .require_success(OsStr::new("cosign"))?;
    Ok(())
}

fn load_receipt(context: &ConfigContext, executable: &Path) -> Result<Option<InstallReceipt>> {
    let path = context.paths.data.join("install.json");
    if !path
        .try_exists()
        .map_err(|source| HostError::io(&path, source))?
    {
        return Ok(None);
    }
    let receipt: InstallReceipt =
        serde_json::from_slice(&fs::read(&path).map_err(|source| HostError::io(&path, source))?)
            .map_err(|error| HostError::Serialization(format!("installation receipt: {error}")))?;
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION
        || !receipt.executable.is_absolute()
        || receipt.target != current_target_triple()?
        || !matches!(receipt.method.as_str(), "portable" | "deb" | "rpm" | "apk")
    {
        return Err(HostError::Config(
            "installation receipt has an unsupported schema or path".to_owned(),
        ));
    }
    let receipt_path = fs::canonicalize(&receipt.executable).unwrap_or(receipt.executable.clone());
    let current = fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    Ok((receipt_path == current).then_some(receipt))
}

fn current_target_triple() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => {
            return Err(HostError::Usage(format!(
                "unsupported architecture {other}"
            )));
        }
    };
    let suffix = match std::env::consts::OS {
        "linux" if cfg!(target_env = "musl") => "unknown-linux-musl",
        "linux" => "unknown-linux-gnu",
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        other => {
            return Err(HostError::Usage(format!(
                "unsupported operating system {other}"
            )));
        }
    };
    Ok(format!("{arch}-{suffix}"))
}

fn detect_install_method(executable: &Path) -> String {
    #[cfg(target_os = "linux")]
    for (program, args, method) in [
        ("dpkg-query", vec!["-S"], "deb"),
        ("rpm", vec!["-qf"], "rpm"),
        ("apk", vec!["info", "--who-owns"], "apk"),
    ] {
        let mut spec = CommandSpec::new(program).args(args);
        spec.args.push(executable.as_os_str().to_owned());
        if run_capture(&spec).is_ok_and(|output| output.status.success()) {
            return method.to_owned();
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = executable;
    "portable".to_owned()
}

fn install_package(method: &str, package: &Path) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (method, package);
        Err(HostError::Usage(
            "native release packages are supported only on Linux".to_owned(),
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let (program, mut args): (&str, Vec<OsString>) = match method {
            "deb" => (
                "apt-get",
                vec![
                    "install".into(),
                    "--yes".into(),
                    package.as_os_str().to_owned(),
                ],
            ),
            "rpm" if program_available("dnf") => (
                "dnf",
                vec![
                    "install".into(),
                    "--assumeyes".into(),
                    package.as_os_str().to_owned(),
                ],
            ),
            "rpm" => (
                "rpm",
                vec![
                    "--upgrade".into(),
                    "--replacepkgs".into(),
                    package.as_os_str().to_owned(),
                ],
            ),
            "apk" => (
                "apk",
                vec![
                    "add".into(),
                    "--allow-untrusted".into(),
                    package.as_os_str().to_owned(),
                ],
            ),
            _ => {
                return Err(HostError::Usage(format!(
                    "unknown package method {method:?}"
                )));
            }
        };
        let root = run_capture(&CommandSpec::new("id").args(["-u"]))
            .ok()
            .is_some_and(|output| output.status.success() && output.stdout_text() == "0");
        let spec = if root {
            CommandSpec::new(program).args(args)
        } else {
            let mut elevated = vec![OsString::from(program)];
            elevated.append(&mut args);
            CommandSpec::new("sudo").args(elevated)
        };
        let status = crate::command::run_interactive(&spec)?;
        if status != 0 {
            return Err(HostError::Runtime(format!(
                "package manager exited with status {status}"
            )));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn program_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[allow(clippy::needless_pass_by_value)]
fn install_portable(
    archive: &Path,
    executable: &Path,
    temporary: TempDir,
    restart: bool,
) -> Result<()> {
    #[cfg(windows)]
    {
        return install_portable_windows(archive, executable, temporary, restart);
    }
    #[cfg(not(windows))]
    {
        let _ = restart;
        let binary = extract_tar_binary(archive, temporary.path())?;
        let parent = executable.parent().ok_or_else(|| HostError::UnsafePath {
            path: executable.to_path_buf(),
            reason: "executable has no parent directory".to_owned(),
        })?;
        let mut staged = match tempfile::NamedTempFile::new_in(parent) {
            Ok(staged) => staged,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return install_portable_privileged(&binary, executable);
            }
            Err(source) => return Err(HostError::io(parent, source)),
        };
        let mut source =
            fs::File::open(&binary).map_err(|source| HostError::io(&binary, source))?;
        std::io::copy(&mut source, &mut staged)
            .map_err(|source| HostError::io(staged.path(), source))?;
        staged
            .as_file()
            .sync_all()
            .map_err(|source| HostError::io(staged.path(), source))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(staged.path(), fs::Permissions::from_mode(0o755))
                .map_err(|source| HostError::io(staged.path(), source))?;
        }
        match staged.persist(executable) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == std::io::ErrorKind::PermissionDenied => {
                install_portable_privileged(error.file.path(), executable)
            }
            Err(error) => Err(HostError::io(executable, error.error)),
        }
    }
}

#[cfg(not(windows))]
fn install_portable_privileged(source: &Path, executable: &Path) -> Result<()> {
    let parent = executable.parent().ok_or_else(|| HostError::UnsafePath {
        path: executable.to_path_buf(),
        reason: "executable has no parent directory".to_owned(),
    })?;
    let staged = parent.join(format!(".codex-start.update-{}", Uuid::new_v4()));
    if staged
        .try_exists()
        .map_err(|source| HostError::io(&staged, source))?
    {
        return Err(HostError::UnsafePath {
            path: staged,
            reason: "privileged update staging path already exists".to_owned(),
        });
    }
    run_privileged_update_command(
        "install",
        vec![
            OsString::from("-m"),
            OsString::from("0755"),
            source.as_os_str().to_owned(),
            staged.as_os_str().to_owned(),
        ],
    )?;
    let replacement = run_privileged_update_command(
        "mv",
        vec![
            OsString::from("-f"),
            staged.as_os_str().to_owned(),
            executable.as_os_str().to_owned(),
        ],
    );
    if replacement.is_err() {
        let _ = run_privileged_update_command(
            "rm",
            vec![OsString::from("-f"), staged.as_os_str().to_owned()],
        );
    }
    replacement
}

#[cfg(not(windows))]
fn run_privileged_update_command(program: &str, args: Vec<OsString>) -> Result<()> {
    let root = run_capture(&CommandSpec::new("id").args(["-u"]))
        .ok()
        .is_some_and(|output| output.status.success() && output.stdout_text() == "0");
    let spec = if root {
        CommandSpec::new(program).args(args)
    } else {
        let mut elevated = vec![OsString::from(program)];
        elevated.extend(args);
        CommandSpec::new("sudo").args(elevated)
    };
    let status = crate::command::run_interactive(&spec)?;
    if status != 0 {
        return Err(HostError::Runtime(format!(
            "privileged {program} exited with status {status}"
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn extract_tar_binary(archive: &Path, destination: &Path) -> Result<PathBuf> {
    let file = fs::File::open(archive).map_err(|source| HostError::io(archive, source))?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    let output = destination.join("codex-start");
    let mut found = false;
    for entry in tar
        .entries()
        .map_err(|source| HostError::io(archive, source))?
    {
        let mut entry = entry.map_err(|source| HostError::io(archive, source))?;
        let path = entry
            .path()
            .map_err(|source| HostError::io(archive, source))?
            .into_owned();
        if !safe_archive_path(&path) {
            return Err(HostError::UnsafePath {
                path,
                reason: "release archive entry is unsafe".to_owned(),
            });
        }
        if path.file_name() == Some(OsStr::new("codex-start")) {
            if found || !entry.header().entry_type().is_file() || entry.size() > EXECUTABLE_LIMIT {
                return Err(HostError::Runtime(
                    "release archive does not contain one regular codex-start binary".to_owned(),
                ));
            }
            let mut file =
                fs::File::create(&output).map_err(|source| HostError::io(&output, source))?;
            std::io::copy(&mut entry, &mut file)
                .map_err(|source| HostError::io(&output, source))?;
            found = true;
        }
    }
    if !found {
        return Err(HostError::NotFound(
            "codex-start binary in release archive".to_owned(),
        ));
    }
    Ok(output)
}

#[cfg(not(windows))]
fn safe_archive_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(windows)]
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WindowsApplyState {
    restart: bool,
    arguments: Vec<String>,
    cwd: PathBuf,
}

#[cfg(windows)]
fn install_portable_windows(
    archive: &Path,
    executable: &Path,
    temporary: TempDir,
    restart: bool,
) -> Result<()> {
    let binary = extract_zip_binary(archive, temporary.path())?;
    let directory = temporary.keep();
    let source = directory.join("codex-start.new.exe");
    fs::rename(&binary, &source).map_err(|error| HostError::io(&source, error))?;
    let helper = directory.join("codex-start-update-helper.exe");
    fs::copy(executable, &helper).map_err(|error| HostError::io(&helper, error))?;
    let arguments = directory.join("apply.json");
    let state = WindowsApplyState {
        restart,
        arguments: std::env::args_os()
            .skip(1)
            .map(|argument| {
                argument.into_string().map_err(|_| {
                    HostError::Usage(
                        "Windows updater cannot restart a command with non-Unicode arguments"
                            .to_owned(),
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?,
        cwd: std::env::current_dir().map_err(|error| HostError::io("current directory", error))?,
    };
    fs::write(
        &arguments,
        serde_json::to_vec(&state).map_err(|error| HostError::Serialization(error.to_string()))?,
    )
    .map_err(|error| HostError::io(&arguments, error))?;
    set_private_file(&arguments)?;
    let mut command = Command::new(&helper);
    command.args([
        OsString::from("__apply-update"),
        OsString::from("--source"),
        source.as_os_str().to_owned(),
        OsString::from("--destination"),
        executable.as_os_str().to_owned(),
        OsString::from("--arguments"),
        arguments.as_os_str().to_owned(),
    ]);
    command
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    if restart {
        command.stdin(std::process::Stdio::inherit());
    } else {
        command.stdin(std::process::Stdio::null());
    }
    command.spawn().map_err(|source| HostError::CommandIo {
        program: helper.into_os_string(),
        source,
    })?;
    Ok(())
}

#[cfg(windows)]
fn extract_zip_binary(archive: &Path, destination: &Path) -> Result<PathBuf> {
    let file = fs::File::open(archive).map_err(|source| HostError::io(archive, source))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|error| HostError::Runtime(format!("open release ZIP: {error}")))?;
    let output = destination.join("codex-start.exe");
    let mut found = false;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| HostError::Runtime(format!("read release ZIP: {error}")))?;
        let path = entry.enclosed_name().ok_or_else(|| HostError::UnsafePath {
            path: PathBuf::from(entry.name()),
            reason: "release ZIP entry escapes its root".to_owned(),
        })?;
        if path.file_name() == Some(OsStr::new("codex-start.exe")) {
            if found || entry.is_dir() || entry.size() > EXECUTABLE_LIMIT {
                return Err(HostError::Runtime(
                    "release ZIP does not contain one regular codex-start.exe".to_owned(),
                ));
            }
            let mut file =
                fs::File::create(&output).map_err(|source| HostError::io(&output, source))?;
            std::io::copy(&mut entry, &mut file)
                .map_err(|source| HostError::io(&output, source))?;
            found = true;
        }
    }
    if !found {
        return Err(HostError::NotFound(
            "codex-start.exe in release ZIP".to_owned(),
        ));
    }
    Ok(output)
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn apply_staged(args: UpdateApplyArgs) -> Result<u8> {
    #[cfg(not(windows))]
    {
        let _ = args;
        Err(HostError::Usage(
            "the staged-update helper is available only on Windows".to_owned(),
        ))
    }
    #[cfg(windows)]
    {
        let state: WindowsApplyState = serde_json::from_slice(
            &fs::read(&args.arguments).map_err(|source| HostError::io(&args.arguments, source))?,
        )
        .map_err(|error| HostError::Serialization(format!("update apply state: {error}")))?;
        if !args.source.is_absolute()
            || !args.destination.is_absolute()
            || args.source.parent() != args.arguments.parent()
        {
            return Err(HostError::UnsafePath {
                path: args.source,
                reason: "staged Windows update paths are inconsistent".to_owned(),
            });
        }
        let backup = args
            .arguments
            .parent()
            .ok_or_else(|| HostError::UnsafePath {
                path: args.arguments.clone(),
                reason: "apply state has no parent".to_owned(),
            })?
            .join("codex-start.previous.exe");
        let mut replaced = false;
        for _ in 0..300 {
            match fs::rename(&args.destination, &backup) {
                Ok(()) => {
                    if let Err(source) = fs::copy(&args.source, &args.destination) {
                        let _ = fs::remove_file(&args.destination);
                        if let Err(rollback) = fs::rename(&backup, &args.destination) {
                            return Err(HostError::Runtime(format!(
                                "copy staged Windows update: {source}; restore previous executable: {rollback}"
                            )));
                        }
                        return Err(HostError::io(&args.destination, source));
                    }
                    replaced = true;
                    break;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(source) => return Err(HostError::io(&args.destination, source)),
            }
        }
        if !replaced {
            return Err(HostError::Runtime(
                "timed out waiting to replace the running Windows executable".to_owned(),
            ));
        }
        if state.restart {
            Command::new(&args.destination)
                .args(state.arguments)
                .current_dir(state.cwd)
                .env(UPDATE_REEXEC_ENV, "1")
                .spawn()
                .map_err(|source| HostError::CommandIo {
                    program: args.destination.into_os_string(),
                    source,
                })?;
        }
        Ok(0)
    }
}

fn state_path(context: &ConfigContext) -> PathBuf {
    context.paths.cache.join("update-state.json")
}

fn load_state(context: &ConfigContext) -> Result<UpdateState> {
    let path = state_path(context);
    if !path
        .try_exists()
        .map_err(|source| HostError::io(&path, source))?
    {
        return Ok(UpdateState {
            schema_version: STATE_SCHEMA_VERSION,
            ..UpdateState::default()
        });
    }
    let state: UpdateState =
        serde_json::from_slice(&fs::read(&path).map_err(|source| HostError::io(&path, source))?)
            .map_err(|error| HostError::Serialization(format!("update state: {error}")))?;
    if state.schema_version != STATE_SCHEMA_VERSION {
        return Ok(UpdateState {
            schema_version: STATE_SCHEMA_VERSION,
            ..UpdateState::default()
        });
    }
    Ok(state)
}

fn save_state(context: &ConfigContext, state: &UpdateState) -> Result<()> {
    let serialized = serde_json::to_string_pretty(state)
        .map_err(|error| HostError::Serialization(error.to_string()))?;
    atomic_write(&state_path(context), &format!("{serialized}\n"))
}

fn save_automatic_state(context: &ConfigContext, state: &UpdateState) {
    if let Err(error) = save_state(context, state) {
        tracing::debug!(%error, "could not persist automatic update state");
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[allow(dead_code)]
fn automatic_check_due(state: &UpdateState, interval_hours: u64) -> bool {
    let now = now_seconds();
    if state
        .last_success_unix_seconds
        .is_some_and(|last| now.saturating_sub(last) < interval_hours.saturating_mul(3600))
    {
        return false;
    }
    state
        .last_attempt_unix_seconds
        .is_none_or(|last| now.saturating_sub(last) >= FAILED_RETRY_SECONDS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_parser_rejects_duplicates_and_unsafe_names() {
        let digest = "a".repeat(64);
        assert!(parse_checksums(&format!("{digest}  archive.tar.gz\n")).is_ok());
        assert!(parse_checksums(&format!("{digest}  ../archive\n")).is_err());
        assert!(
            parse_checksums(&format!(
                "{digest}  archive.tar.gz\n{digest}  archive.tar.gz\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn stable_semver_comparison_never_downgrades() {
        let release = AvailableRelease {
            tag: "v99.0.0".to_owned(),
            version: Version::new(99, 0, 0),
            url: "https://example.invalid".to_owned(),
            assets: BTreeMap::new(),
        };
        let check = update_check(&release).unwrap();
        assert!(check.available);
        assert_eq!(check.latest, "99.0.0");
    }

    #[test]
    fn automatic_check_uses_success_and_failure_windows() {
        let now = now_seconds();
        let state = UpdateState {
            schema_version: 1,
            last_success_unix_seconds: Some(now),
            ..UpdateState::default()
        };
        assert!(!automatic_check_due(&state, 24));
        let failed = UpdateState {
            schema_version: 1,
            last_attempt_unix_seconds: Some(now),
            ..UpdateState::default()
        };
        assert!(!automatic_check_due(&failed, 24));
    }

    #[test]
    #[cfg(not(windows))]
    fn archive_paths_must_be_normal_relative_components() {
        assert!(safe_archive_path(Path::new("release/bin/codex-start")));
        assert!(!safe_archive_path(Path::new("../codex-start")));
        assert!(!safe_archive_path(Path::new("/codex-start")));
    }

    #[test]
    fn release_manifest_must_match_github_asset_sizes_and_related_files() {
        let filename = "codex-start-99.0.0-x86_64-unknown-linux-gnu.tar.gz";
        let bundle = format!("{filename}.bundle");
        let sbom = format!("{filename}.spdx.json");
        let mut assets = BTreeMap::new();
        for (id, name, size) in [
            (1, filename, 123),
            (2, bundle.as_str(), 10),
            (3, sbom.as_str(), 20),
        ] {
            assets.insert(
                name.to_owned(),
                GitHubAsset {
                    id,
                    name: name.to_owned(),
                    size,
                    digest: None,
                },
            );
        }
        let release = AvailableRelease {
            tag: "v99.0.0".to_owned(),
            version: Version::new(99, 0, 0),
            url: "https://example.invalid".to_owned(),
            assets,
        };
        let mut manifest = ReleaseManifest {
            schema_version: 1,
            version: "99.0.0".to_owned(),
            tag: "v99.0.0".to_owned(),
            artifacts: vec![ReleaseArtifact {
                kind: "archive".to_owned(),
                os: "linux".to_owned(),
                arch: "x86_64".to_owned(),
                libc: Some("gnu".to_owned()),
                filename: filename.to_owned(),
                size: 123,
                sha256: "a".repeat(64),
                bundle,
                sbom,
            }],
        };

        validate_manifest(&manifest, &release).expect("matching manifest");
        manifest.artifacts[0].size += 1;
        assert!(validate_manifest(&manifest, &release).is_err());
    }
}
