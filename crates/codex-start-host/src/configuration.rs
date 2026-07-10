//! Host-side configuration discovery, resolution, and mutation.

use std::{
    env,
    ffi::OsString,
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use codex_start_core::{
    ConfigDocument, ConfigLayer, ConfigLayerKind, ConfigPatch, ConfigResolver, HomeConfig,
    MergePatch, NetworkMode, ResolvedConfig, RuntimeKind as CoreRuntimeKind, TtyMode,
    WorktreeMode as CoreWorktreeMode,
};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::{
    cli::{MergeRunOptions, NetworkModeArg, PortProtocol, PortSpec, RunOptions},
    environments::EnvironmentCatalog,
    error::{HostError, Result},
    git::GitRepo,
    home::discover_home_configs,
    paths::{AppPaths, atomic_write, ensure_regular_file_or_missing},
    runtime::{RuntimeKind, format_publish_address},
};

/// Paths and identity relevant to one configuration resolution.
#[derive(Clone, Debug)]
pub struct ConfigContext {
    /// Application roots.
    pub paths: AppPaths,
    /// Canonical invocation directory.
    pub cwd: PathBuf,
    /// Optional repository metadata.
    pub repo: Option<GitRepo>,
    /// Global file selected for this invocation.
    pub global_file: PathBuf,
    /// Private project settings file.
    pub project_file: PathBuf,
}

/// Persistent configuration layer edited by the interactive configuration command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigTarget {
    /// Settings private to the current project.
    Project,
    /// User-wide settings and definitions.
    Global,
}

impl ConfigTarget {
    /// Human-readable target name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }

    const fn is_global(self) -> bool {
        matches!(self, Self::Global)
    }
}

/// Common launcher settings supported by the interactive editor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommonSettings {
    pub environment: Option<String>,
    pub runtime: Option<CoreRuntimeKind>,
    pub network: Option<NetworkMode>,
    pub worktree: Option<CoreWorktreeMode>,
    pub home: Option<String>,
    pub tty: Option<TtyMode>,
    pub rebuild: Option<bool>,
}

impl From<&ConfigPatch> for CommonSettings {
    fn from(patch: &ConfigPatch) -> Self {
        Self {
            environment: patch.environment.clone(),
            runtime: patch.runtime,
            network: patch.network,
            worktree: patch.worktree,
            home: patch.home.clone(),
            tty: patch.tty,
            rebuild: patch.rebuild,
        }
    }
}

/// In-memory edit session for one global or project document.
#[derive(Clone, Debug)]
pub struct ConfigDraft {
    target: ConfigTarget,
    path: PathBuf,
    original: Option<String>,
    document: DocumentMut,
    initial: CommonSettings,
    settings: CommonSettings,
}

impl ConfigDraft {
    #[must_use]
    pub const fn target(&self) -> ConfigTarget {
        self.target
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn settings(&self) -> &CommonSettings {
        &self.settings
    }

    pub const fn settings_mut(&mut self) -> &mut CommonSettings {
        &mut self.settings
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.settings != self.initial
    }
}

impl ConfigContext {
    /// Discover configuration and project locations without creating project settings.
    pub fn discover(global_override: Option<&Path>) -> Result<Self> {
        let paths = AppPaths::discover()?;
        paths.ensure()?;
        let cwd =
            env::current_dir().map_err(|source| HostError::io("current directory", source))?;
        let cwd = fs::canonicalize(&cwd).map_err(|source| HostError::io(&cwd, source))?;
        let repo = GitRepo::discover(&cwd)?;
        let global_file = global_override.map_or_else(|| paths.config_file(), Path::to_path_buf);
        let project_file = repo.as_ref().map_or_else(
            || paths.non_git_project_file(&cwd),
            GitRepo::project_config_path,
        );
        Ok(Self {
            paths,
            cwd,
            repo,
            global_file,
            project_file,
        })
    }

    /// Project root mounted for direct runs.
    pub fn project_root(&self) -> &Path {
        self.repo
            .as_ref()
            .map_or(self.cwd.as_path(), |repo| repo.root.as_path())
    }

    /// Path associated with one editable persistent layer.
    #[must_use]
    pub fn config_path(&self, target: ConfigTarget) -> &Path {
        match target {
            ConfigTarget::Project => &self.project_file,
            ConfigTarget::Global => &self.global_file,
        }
    }

    /// Load one persistent layer without creating or rewriting it.
    pub fn load_common_settings(&self, target: ConfigTarget) -> Result<ConfigDraft> {
        let path = self.config_path(target);
        let original = read_optional_text(path)?;
        let contents = original.as_deref().unwrap_or("schema_version = 1\n");
        let document = contents
            .parse::<DocumentMut>()
            .map_err(|error| HostError::Config(format!("{}: {error}", path.display())))?;
        let parsed = ConfigDocument::parse_file(path, contents).map_err(config_error)?;
        if !target.is_global() {
            parsed.validate_as_project().map_err(config_error)?;
        }
        let settings = CommonSettings::from(&parsed.settings);
        Ok(ConfigDraft {
            target,
            path: path.to_path_buf(),
            original,
            document,
            initial: settings.clone(),
            settings,
        })
    }

    /// Validate and atomically persist the changed common settings in one write.
    pub fn save_common_settings(&self, mut draft: ConfigDraft) -> Result<Option<PathBuf>> {
        if draft.path != self.config_path(draft.target) {
            return Err(HostError::Config(
                "configuration draft does not belong to this invocation".to_owned(),
            ));
        }
        if !draft.is_dirty() {
            return Ok(None);
        }
        if read_optional_text(&draft.path)? != draft.original {
            return Err(HostError::Config(format!(
                "{} changed while the interactive editor was open; reload it and try again",
                draft.path.display()
            )));
        }

        apply_common_settings(&mut draft)?;
        let rendered = draft.document.to_string();
        let parsed = ConfigDocument::parse_file(&draft.path, &rendered).map_err(config_error)?;
        if !draft.target.is_global() {
            parsed.validate_as_project().map_err(config_error)?;
        }
        atomic_write(&draft.path, &rendered)?;
        Ok(Some(draft.path))
    }

    /// Resolve every persistent layer, environment override, and CLI patch.
    pub fn resolve(&self, cli_patch: Option<ConfigPatch>) -> Result<ResolvedConfig> {
        let preliminary = self.resolve_layers(None, cli_patch.clone())?;
        let catalog = EnvironmentCatalog::load(&self.paths)?;
        let selected = if preliminary
            .provenance
            .source_for("environment")
            .is_some_and(|source| source.kind == ConfigLayerKind::BuiltIn)
        {
            catalog
                .detect(self.project_root())?
                .unwrap_or_else(|| preliminary.config.environment.clone())
        } else {
            preliminary.config.environment.clone()
        };
        let environment = catalog.resolve(&selected)?;
        let mut defaults = environment.settings;
        defaults.environment = Some(selected.clone());
        self.resolve_layers(
            Some((format!("environment:{selected}"), defaults)),
            cli_patch,
        )
    }

    fn resolve_layers(
        &self,
        environment: Option<(String, ConfigPatch)>,
        cli_patch: Option<ConfigPatch>,
    ) -> Result<ResolvedConfig> {
        let mut resolver = ConfigResolver::new();
        let discovered_homes = discover_home_configs(&self.paths)?;
        if !discovered_homes.is_empty() {
            resolver
                .add_document(
                    ConfigLayerKind::BuiltIn,
                    "discovered homes",
                    ConfigDocument {
                        homes: discovered_homes,
                        ..ConfigDocument::default()
                    },
                )
                .map_err(config_error)?;
        }
        if let Some((label, patch)) = environment {
            resolver
                .add_layer(ConfigLayer::new(ConfigLayerKind::Environment, label, patch))
                .map_err(config_error)?;
        }
        if ensure_regular_file_or_missing(&self.global_file)? {
            let document = read_document(&self.global_file, false)?;
            resolver
                .add_document(
                    ConfigLayerKind::Global,
                    self.global_file.display().to_string(),
                    document,
                )
                .map_err(config_error)?;
        }
        if ensure_regular_file_or_missing(&self.project_file)? {
            let document = read_document(&self.project_file, true)?;
            resolver
                .add_document(
                    ConfigLayerKind::Project,
                    self.project_file.display().to_string(),
                    document,
                )
                .map_err(config_error)?;
        }
        resolver
            .add_environment_overrides(environment_overrides(env::vars_os())?)
            .map_err(config_error)?;
        if let Some(patch) = cli_patch {
            resolver
                .add_layer(ConfigLayer::new(
                    ConfigLayerKind::CommandLine,
                    "command line",
                    patch,
                ))
                .map_err(config_error)?;
        }
        resolver.resolve().map_err(config_error)
    }

    /// Initialize an explicit global or private project document.
    pub fn initialize(
        &self,
        global: bool,
        environment: Option<&str>,
        force: bool,
    ) -> Result<PathBuf> {
        let path = if global {
            &self.global_file
        } else {
            &self.project_file
        };
        let exists = ensure_regular_file_or_missing(path)?;
        if exists && !force {
            return Err(HostError::Config(format!(
                "{} already exists; use --force to replace it",
                path.display()
            )));
        }
        if exists {
            let backup = path.with_extension("toml.bak");
            let original =
                fs::read_to_string(path).map_err(|source| HostError::io(path, source))?;
            atomic_write(&backup, &original)?;
        }
        let catalog = EnvironmentCatalog::load(&self.paths)?;
        let environment = if let Some(environment) = environment {
            catalog.resolve(environment)?;
            environment.to_owned()
        } else {
            catalog
                .detect(self.project_root())?
                .unwrap_or_else(|| "generic".to_owned())
        };
        let contents = if global {
            format!(
                "schema_version = 1\n\n[settings]\nenvironment = {environment:?}\nruntime = \"auto\"\nnetwork = \"allowlist\"\nworktree = \"auto\"\nhome = \"default\"\n\n[homes.default]\nkind = \"managed\"\nname = \"default\"\n"
            )
        } else {
            format!("schema_version = 1\n\n[settings]\nenvironment = {environment:?}\n")
        };
        atomic_write(path, &contents)?;
        Ok(path.clone())
    }

    /// Set a dotted TOML key atomically, creating an otherwise minimal document.
    pub fn set(&self, global: bool, key: &str, value: &str) -> Result<PathBuf> {
        let path = if global {
            &self.global_file
        } else {
            &self.project_file
        };
        let contents = if ensure_regular_file_or_missing(path)? {
            fs::read_to_string(path).map_err(|source| HostError::io(path, source))?
        } else {
            "schema_version = 1\n".to_owned()
        };
        let mut document = contents
            .parse::<DocumentMut>()
            .map_err(|error| HostError::Config(format!("{}: {error}", path.display())))?;
        let key = if key.starts_with("settings.")
            || key.starts_with("homes.")
            || key.starts_with("secrets.")
            || key.starts_with("profiles.")
            || key == "schema_version"
        {
            key.to_owned()
        } else {
            format!("settings.{key}")
        };
        let segments = key.split('.').collect::<Vec<_>>();
        if segments.iter().any(|segment| segment.is_empty()) {
            return Err(HostError::Usage(
                "configuration key contains an empty segment".to_owned(),
            ));
        }
        let mut wrapper = format!("value = {value}")
            .parse::<DocumentMut>()
            .map_err(|error| HostError::Usage(format!("value is not valid TOML: {error}")))?;
        let item = wrapper
            .remove("value")
            .ok_or_else(|| HostError::Usage("value is not valid TOML".to_owned()))?;
        insert_item(document.as_table_mut(), &segments, item)?;
        // Strict validation before replacing a user's file.
        let rendered = document.to_string();
        let parsed = ConfigDocument::parse_file(path, &rendered).map_err(config_error)?;
        if !global {
            parsed.validate_as_project().map_err(config_error)?;
        }
        atomic_write(path, &rendered)?;
        Ok(path.clone())
    }
}

fn read_optional_text(path: &Path) -> Result<Option<String>> {
    if ensure_regular_file_or_missing(path)? {
        fs::read_to_string(path)
            .map(Some)
            .map_err(|source| HostError::io(path, source))
    } else {
        Ok(None)
    }
}

fn apply_common_settings(draft: &mut ConfigDraft) -> Result<()> {
    if draft.settings.environment != draft.initial.environment {
        update_setting(
            &mut draft.document,
            "environment",
            draft.settings.environment.as_deref().map(value),
        )?;
    }
    if draft.settings.runtime != draft.initial.runtime {
        update_setting(
            &mut draft.document,
            "runtime",
            draft
                .settings
                .runtime
                .map(|setting| value(runtime_name(setting))),
        )?;
    }
    if draft.settings.network != draft.initial.network {
        update_setting(
            &mut draft.document,
            "network",
            draft
                .settings
                .network
                .map(|setting| value(network_name(setting))),
        )?;
    }
    if draft.settings.worktree != draft.initial.worktree {
        update_setting(
            &mut draft.document,
            "worktree",
            draft
                .settings
                .worktree
                .map(|setting| value(worktree_name(setting))),
        )?;
    }
    if draft.settings.home != draft.initial.home {
        update_setting(
            &mut draft.document,
            "home",
            draft.settings.home.as_deref().map(value),
        )?;
    }
    if draft.settings.tty != draft.initial.tty {
        update_setting(
            &mut draft.document,
            "tty",
            draft.settings.tty.map(|setting| value(tty_name(setting))),
        )?;
    }
    if draft.settings.rebuild != draft.initial.rebuild {
        update_setting(
            &mut draft.document,
            "rebuild",
            draft.settings.rebuild.map(value),
        )?;
    }
    Ok(())
}

fn update_setting(document: &mut DocumentMut, key: &str, item: Option<Item>) -> Result<()> {
    if item.is_none() && document.get("settings").is_none() {
        return Ok(());
    }
    if document.get("settings").is_none() {
        document.insert("settings", Item::Table(Table::new()));
    }
    let settings = document
        .get_mut("settings")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| HostError::Config("`settings` is not a TOML table".to_owned()))?;
    if let Some(mut item) = item {
        if let Some(existing) = settings.get_mut(key) {
            if let (Some(existing), Some(replacement)) = (existing.as_value(), item.as_value_mut())
            {
                replacement.decor_mut().clone_from(existing.decor());
            }
            *existing = item;
        } else {
            settings.insert(key, item);
        }
    } else {
        settings.remove(key);
    }
    Ok(())
}

const fn runtime_name(value: CoreRuntimeKind) -> &'static str {
    match value {
        CoreRuntimeKind::Auto => "auto",
        CoreRuntimeKind::Docker => "docker",
        CoreRuntimeKind::Podman => "podman",
    }
}

const fn network_name(value: NetworkMode) -> &'static str {
    match value {
        NetworkMode::Offline => "offline",
        NetworkMode::Allowlist => "allowlist",
        NetworkMode::Bridge => "bridge",
        NetworkMode::Host => "host",
    }
}

const fn worktree_name(value: CoreWorktreeMode) -> &'static str {
    match value {
        CoreWorktreeMode::Auto => "auto",
        CoreWorktreeMode::Always => "always",
        CoreWorktreeMode::Never => "never",
    }
}

const fn tty_name(value: TtyMode) -> &'static str {
    match value {
        TtyMode::Auto => "auto",
        TtyMode::Always => "always",
        TtyMode::Never => "never",
    }
}

fn environment_overrides(
    values: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<Vec<(String, String)>> {
    let mut overrides = Vec::new();
    for (name, value) in values {
        if !name.as_os_str().as_bytes().starts_with(b"CODEX_START__") {
            continue;
        }
        let name = name.into_string().map_err(|name| {
            HostError::Config(format!(
                "codex-start environment override name is not UTF-8: {:?}",
                name.as_os_str().as_bytes()
            ))
        })?;
        let value = value.into_string().map_err(|_| {
            HostError::Config(format!("environment override {name} has a non-UTF-8 value"))
        })?;
        overrides.push((name, value));
    }
    Ok(overrides)
}

/// Convert CLI options into the highest-precedence typed patch.
pub fn patch_from_run_options(environment: Option<&str>, options: &RunOptions) -> ConfigPatch {
    let network = if options.offline {
        Some(NetworkMode::Offline)
    } else if options.no_network {
        Some(NetworkMode::Allowlist)
    } else {
        options.network.map(|mode| match mode {
            NetworkModeArg::Offline => NetworkMode::Offline,
            NetworkModeArg::Allowlist => NetworkMode::Allowlist,
            NetworkModeArg::Bridge => NetworkMode::Bridge,
            NetworkModeArg::Host => NetworkMode::Host,
        })
    };
    let worktree = if options.no_worktree {
        Some(CoreWorktreeMode::Never)
    } else if options.worktree {
        Some(CoreWorktreeMode::Always)
    } else {
        None
    };
    ConfigPatch {
        environment: environment.map(str::to_owned),
        runtime: options.runtime.map(|runtime| match runtime {
            RuntimeKind::Auto => CoreRuntimeKind::Auto,
            RuntimeKind::Docker => CoreRuntimeKind::Docker,
            RuntimeKind::Podman => CoreRuntimeKind::Podman,
        }),
        network,
        worktree,
        home: options.home.clone(),
        name: options.name.clone(),
        publish: (!options.publish.is_empty())
            .then(|| options.publish.iter().map(render_port).collect()),
        rebuild: options.rebuild.then_some(true),
        tty: options.no_tty.then_some(TtyMode::Never),
        allow_hosts: (!options.allow_hosts.is_empty()).then(|| options.allow_hosts.clone()),
        ..ConfigPatch::default()
    }
}

/// Convert fixed-current-worktree merge options into a command-line config layer.
pub fn patch_from_merge_options(
    environment: Option<&str>,
    model: Option<&str>,
    options: &MergeRunOptions,
) -> ConfigPatch {
    let run_options = RunOptions {
        name: None,
        runtime: options.runtime,
        runtime_program: options.runtime_program.clone(),
        home: options.home.clone(),
        network: options.network,
        offline: options.offline,
        no_network: options.no_network,
        no_worktree: true,
        worktree: false,
        publish: options.publish.clone(),
        rebuild: options.rebuild,
        pull: options.pull,
        no_tty: options.no_tty,
        dry_run: options.dry_run,
        allow_hosts: options.allow_hosts.clone(),
        runtime_args: options.runtime_args.clone(),
    };
    let mut patch = patch_from_run_options(environment, &run_options);
    patch.merge = model.map(|model| MergePatch {
        model: Some(model.to_owned()),
    });
    patch
}

/// Convert a resolved core home into the host home module's representation.
pub fn host_home_spec(home: &HomeConfig) -> crate::home::HomeSpec {
    match home {
        HomeConfig::Managed { name } => crate::home::HomeSpec {
            storage_name: name.clone(),
            ..crate::home::HomeSpec::default()
        },
        HomeConfig::Host => crate::home::HomeSpec {
            kind: crate::home::HomeKind::Host,
            storage_name: None,
            path: None,
            agents_path: None,
        },
        HomeConfig::Path { path, agents_path } => crate::home::HomeSpec {
            kind: crate::home::HomeKind::Path,
            storage_name: None,
            path: Some(path.clone()),
            agents_path: agents_path.clone(),
        },
    }
}

fn read_document(path: &Path, project: bool) -> Result<ConfigDocument> {
    ensure_regular_file_or_missing(path)?;
    let text = fs::read_to_string(path).map_err(|source| HostError::io(path, source))?;
    let document = ConfigDocument::parse_file(path, &text).map_err(config_error)?;
    if project {
        document.validate_as_project().map_err(config_error)?;
    }
    Ok(document)
}

fn insert_item(table: &mut Table, segments: &[&str], item: Item) -> Result<()> {
    let Some((head, tail)) = segments.split_first() else {
        return Err(HostError::Usage("configuration key is empty".to_owned()));
    };
    if tail.is_empty() {
        table.insert(head, item);
        return Ok(());
    }
    if !table.contains_key(head) {
        table.insert(head, Item::Table(Table::new()));
    }
    let child = table
        .get_mut(head)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| HostError::Usage(format!("{head:?} already exists and is not a table")))?;
    insert_item(child, tail, item)
}

fn render_port(port: &PortSpec) -> String {
    let protocol = match port.protocol {
        PortProtocol::Tcp => "tcp",
        PortProtocol::Udp => "udp",
    };
    format_publish_address(port.host_ip, port.host_port, port.container_port, protocol)
}

fn config_error(error: impl std::fmt::Display) -> HostError {
    HostError::Config(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use codex_start_core::{ConfigLayerKind, ConfigPatch, HomeConfig, NetworkMode, RuntimeKind};

    use super::{ConfigContext, ConfigTarget, environment_overrides};

    #[test]
    fn set_validates_and_writes_project_document() {
        let root = tempfile::tempdir().expect("root");
        let paths = crate::paths::AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            cache: root.path().join("cache"),
        };
        paths.ensure().expect("paths");
        let context = ConfigContext {
            global_file: paths.config_file(),
            project_file: root.path().join("project.toml"),
            cwd: root.path().to_path_buf(),
            repo: None,
            paths,
        };
        context.set(false, "network", "\"offline\"").expect("set");
        let value = fs::read_to_string(&context.project_file).expect("read");
        assert!(value.contains("network = \"offline\""));
    }

    #[test]
    fn common_settings_save_preserves_unrelated_toml_and_supports_inherit() {
        let root = tempfile::tempdir().expect("root");
        let paths = crate::paths::AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            cache: root.path().join("cache"),
        };
        paths.ensure().expect("paths");
        let context = ConfigContext {
            global_file: paths.config_file(),
            project_file: root.path().join("project.toml"),
            cwd: root.path().to_path_buf(),
            repo: None,
            paths,
        };
        fs::write(
            &context.global_file,
            r#"schema_version = 1

[settings]
runtime = "auto"
network = "allowlist" # retain this explanation

[settings.codex.config]
future_option = "untouched"

[profiles.review.settings]
network = "bridge"
"#,
        )
        .expect("global config");

        let mut draft = context
            .load_common_settings(ConfigTarget::Global)
            .expect("load draft");
        draft.settings_mut().runtime = None;
        draft.settings_mut().network = Some(NetworkMode::Offline);
        context
            .save_common_settings(draft)
            .expect("save draft")
            .expect("updated path");

        let contents = fs::read_to_string(&context.global_file).expect("saved global config");
        assert!(!contents.contains("runtime ="));
        assert!(contents.contains("network = \"offline\" # retain this explanation"));
        assert!(contents.contains("future_option = \"untouched\""));
        assert!(contents.contains("[profiles.review.settings]"));
    }

    #[test]
    fn common_settings_draft_creates_only_after_a_real_save() {
        let root = tempfile::tempdir().expect("root");
        let paths = crate::paths::AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            cache: root.path().join("cache"),
        };
        paths.ensure().expect("paths");
        let context = ConfigContext {
            global_file: paths.config_file(),
            project_file: root.path().join("project.toml"),
            cwd: root.path().to_path_buf(),
            repo: None,
            paths,
        };

        let unchanged = context
            .load_common_settings(ConfigTarget::Project)
            .expect("unchanged draft");
        assert!(
            context
                .save_common_settings(unchanged)
                .expect("unchanged save")
                .is_none()
        );
        assert!(!context.project_file.exists());

        let mut changed = context
            .load_common_settings(ConfigTarget::Project)
            .expect("changed draft");
        changed.settings_mut().runtime = Some(RuntimeKind::Docker);
        context
            .save_common_settings(changed)
            .expect("changed save")
            .expect("created path");
        let contents = fs::read_to_string(&context.project_file).expect("created project config");
        assert!(contents.starts_with("schema_version = 1"));
        assert!(contents.contains("runtime = \"docker\""));
    }

    #[test]
    fn common_settings_save_rejects_concurrent_file_changes() {
        let root = tempfile::tempdir().expect("root");
        let paths = crate::paths::AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            cache: root.path().join("cache"),
        };
        paths.ensure().expect("paths");
        let context = ConfigContext {
            global_file: paths.config_file(),
            project_file: root.path().join("project.toml"),
            cwd: root.path().to_path_buf(),
            repo: None,
            paths,
        };
        fs::write(&context.project_file, "schema_version = 1\n").expect("initial config");
        let mut draft = context
            .load_common_settings(ConfigTarget::Project)
            .expect("draft");
        draft.settings_mut().network = Some(NetworkMode::Offline);
        fs::write(
            &context.project_file,
            "schema_version = 1\n\n[settings]\nrebuild = true\n",
        )
        .expect("external edit");

        let error = context
            .save_common_settings(draft)
            .expect_err("concurrent change must fail");
        assert!(error.to_string().contains("changed while"));
        let contents = fs::read_to_string(&context.project_file).expect("external edit retained");
        assert!(contents.contains("rebuild = true"));
        assert!(!contents.contains("network"));
    }

    #[test]
    fn initialize_detects_custom_environment_markers() {
        let root = tempfile::tempdir().expect("root");
        let paths = crate::paths::AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            cache: root.path().join("cache"),
        };
        paths.ensure().expect("paths");
        fs::write(root.path().join("custom.marker"), "").expect("marker");
        fs::write(
            paths.environments_dir().join("custom.toml"),
            "schema_version=1\nname='custom'\nextends='generic'\nmarkers=['custom.marker']\n",
        )
        .expect("custom environment");
        let context = ConfigContext {
            global_file: paths.config_file(),
            project_file: root.path().join("project.toml"),
            cwd: root.path().to_path_buf(),
            repo: None,
            paths,
        };

        context.initialize(false, None, false).expect("initialize");
        let value = fs::read_to_string(&context.project_file).expect("read");
        assert!(value.contains("environment = \"custom\""));
        assert!(context.initialize(true, Some("unknown"), false).is_err());
    }

    #[test]
    fn discovered_home_resolves_without_persisting_configuration() {
        let root = tempfile::tempdir().expect("root");
        let paths = crate::paths::AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            cache: root.path().join("cache"),
        };
        paths.ensure().expect("paths");
        fs::create_dir(paths.homes_dir().join("work")).expect("managed home");
        let context = ConfigContext {
            global_file: paths.config_file(),
            project_file: root.path().join("project.toml"),
            cwd: root.path().to_path_buf(),
            repo: None,
            paths,
        };

        let resolved = context
            .resolve(Some(ConfigPatch {
                home: Some("work".to_owned()),
                ..ConfigPatch::default()
            }))
            .expect("resolve discovered home");
        assert_eq!(
            resolved.config.home,
            HomeConfig::Managed {
                name: Some("work".to_owned())
            }
        );
        let source = resolved
            .provenance
            .source_for("homes.work.kind")
            .expect("home provenance");
        assert_eq!(source.kind, ConfigLayerKind::BuiltIn);
        assert_eq!(source.label, "discovered homes");
        assert!(!context.global_file.exists());
        assert!(!context.project_file.exists());
    }

    #[test]
    fn configured_home_overrides_discovered_home_definition() {
        let root = tempfile::tempdir().expect("root");
        let paths = crate::paths::AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            cache: root.path().join("cache"),
        };
        paths.ensure().expect("paths");
        fs::create_dir(paths.homes_dir().join("shared")).expect("managed home");
        let selected = root.path().join("selected");
        fs::write(
            paths.config_file(),
            format!("schema_version = 1\n\n[homes.shared]\nkind = \"path\"\npath = {selected:?}\n"),
        )
        .expect("global config");
        let context = ConfigContext {
            global_file: paths.config_file(),
            project_file: root.path().join("project.toml"),
            cwd: root.path().to_path_buf(),
            repo: None,
            paths,
        };

        let resolved = context
            .resolve(Some(ConfigPatch {
                home: Some("shared".to_owned()),
                ..ConfigPatch::default()
            }))
            .expect("resolve configured home");
        assert_eq!(
            resolved.config.home,
            HomeConfig::Path {
                path: selected,
                agents_path: None
            }
        );
        assert_eq!(
            resolved
                .provenance
                .source_for("homes.shared.kind")
                .expect("home provenance")
                .kind,
            ConfigLayerKind::Global
        );
    }

    #[cfg(unix)]
    #[test]
    fn environment_override_decoding_ignores_unrelated_non_utf8_and_rejects_relevant_values() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let unrelated = (
            OsString::from_vec(vec![0xff]),
            OsString::from_vec(vec![0xfe]),
        );
        assert!(environment_overrides([unrelated]).unwrap().is_empty());
        let relevant = (
            OsString::from("CODEX_START__NETWORK"),
            OsString::from_vec(vec![0xff]),
        );
        assert!(environment_overrides([relevant]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn set_rejects_symbolic_link_configuration_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let paths = crate::paths::AppPaths {
            config: root.path().join("config"),
            data: root.path().join("data"),
            cache: root.path().join("cache"),
        };
        paths.ensure().expect("paths");
        let outside = root.path().join("outside.toml");
        fs::write(&outside, "schema_version = 1\n").expect("outside");
        let project_file = root.path().join("project.toml");
        symlink(&outside, &project_file).expect("symlink");
        let context = ConfigContext {
            global_file: paths.config_file(),
            project_file,
            cwd: root.path().to_path_buf(),
            repo: None,
            paths,
        };

        assert!(context.set(false, "network", "\"offline\"").is_err());
        assert_eq!(
            fs::read_to_string(outside).expect("outside read"),
            "schema_version = 1\n"
        );
    }
}
