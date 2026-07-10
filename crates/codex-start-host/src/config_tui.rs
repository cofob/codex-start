//! Interactive editor for common codex-start configuration settings.

use std::{
    io::{self, IsTerminal},
    path::PathBuf,
};

use codex_start_core::{
    ConfigLayerKind, NetworkMode, ResolvedConfig, RuntimeKind, TtyMode, WorktreeMode,
};
use dialoguer::{Confirm, Select, theme::ColorfulTheme};

use crate::{
    cli::OutputFormat,
    configuration::{ConfigContext, ConfigDraft, ConfigTarget},
    environments::EnvironmentCatalog,
    error::{HostError, Result},
};

const SAVE_INDEX: usize = 7;
const CANCEL_INDEX: usize = 8;

/// Run the interactive editor used by bare `codex-start config`.
pub fn run(context: &ConfigContext, output: OutputFormat) -> Result<u8> {
    validate_terminal(
        output,
        io::stdin().is_terminal(),
        io::stderr().is_terminal(),
    )?;
    let mut backend = DialoguerBackend::default();
    match run_with_backend(context, &mut backend)? {
        EditorOutcome::Saved(path) => println!("updated {}", path.display()),
        EditorOutcome::Unchanged => println!("no configuration changes"),
        EditorOutcome::Cancelled => println!("configuration unchanged"),
    }
    Ok(0)
}

fn validate_terminal(
    output: OutputFormat,
    input_terminal: bool,
    prompt_terminal: bool,
) -> Result<()> {
    if output != OutputFormat::Human {
        return Err(HostError::Usage(
            "interactive configuration requires human output; use `config show`, `config set`, or `config edit` with --output json"
                .to_owned(),
        ));
    }
    if !input_terminal || !prompt_terminal {
        return Err(HostError::Usage(
            "interactive configuration requires a terminal; use `config set` or `config edit` in non-interactive environments"
                .to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum PromptEvent<T> {
    Selected(T),
    Escaped,
    Interrupted,
}

trait PromptBackend {
    fn select(
        &mut self,
        prompt: &str,
        items: &[String],
        default: usize,
    ) -> Result<PromptEvent<usize>>;

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<PromptEvent<bool>>;
}

#[derive(Default)]
struct DialoguerBackend {
    theme: ColorfulTheme,
}

impl PromptBackend for DialoguerBackend {
    fn select(
        &mut self,
        prompt: &str,
        items: &[String],
        default: usize,
    ) -> Result<PromptEvent<usize>> {
        let result = Select::with_theme(&self.theme)
            .with_prompt(prompt)
            .items(items)
            .default(default.min(items.len().saturating_sub(1)))
            .interact_opt();
        prompt_event(result)
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<PromptEvent<bool>> {
        let result = Confirm::with_theme(&self.theme)
            .with_prompt(prompt)
            .default(default)
            .interact_opt();
        prompt_event(result)
    }
}

fn prompt_event<T>(result: dialoguer::Result<Option<T>>) -> Result<PromptEvent<T>> {
    match result {
        Ok(Some(value)) => Ok(PromptEvent::Selected(value)),
        Ok(None) => Ok(PromptEvent::Escaped),
        Err(dialoguer::Error::IO(error)) if error.kind() == io::ErrorKind::Interrupted => {
            Ok(PromptEvent::Interrupted)
        }
        Err(dialoguer::Error::IO(error)) => Err(HostError::io("interactive terminal", error)),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum EditorOutcome {
    Saved(PathBuf),
    Unchanged,
    Cancelled,
}

fn run_with_backend(
    context: &ConfigContext,
    backend: &mut impl PromptBackend,
) -> Result<EditorOutcome> {
    let targets = [
        format!(
            "Project configuration ({})",
            context.config_path(ConfigTarget::Project).display()
        ),
        format!(
            "Global configuration ({})",
            context.config_path(ConfigTarget::Global).display()
        ),
    ];
    let target = match backend.select("Configuration target", &targets, 0)? {
        PromptEvent::Selected(0) => ConfigTarget::Project,
        PromptEvent::Selected(1) => ConfigTarget::Global,
        PromptEvent::Selected(_) => {
            return Err(HostError::Config(
                "interactive target selection returned an invalid item".to_owned(),
            ));
        }
        PromptEvent::Escaped | PromptEvent::Interrupted => return Ok(EditorOutcome::Cancelled),
    };

    let resolved = context.resolve(None)?;
    let catalog = EnvironmentCatalog::load(&context.paths)?;
    let environments = catalog.names().map(str::to_owned).collect::<Vec<_>>();
    let homes = resolved.config.homes.keys().cloned().collect::<Vec<_>>();
    let effective = EffectiveSettings::new(&resolved);
    let draft = context.load_common_settings(target)?;
    edit_draft(context, backend, draft, &environments, &homes, &effective)
}

fn edit_draft(
    context: &ConfigContext,
    backend: &mut impl PromptBackend,
    mut draft: ConfigDraft,
    environments: &[String],
    homes: &[String],
    effective: &EffectiveSettings,
) -> Result<EditorOutcome> {
    let prompt = format!(
        "Configure {} settings ({})",
        draft.target().label(),
        draft.path().display()
    );
    let mut cursor = 0;
    loop {
        let items = menu_items(&draft, effective);
        let selection = match backend.select(&prompt, &items, cursor)? {
            PromptEvent::Selected(selection) => selection,
            PromptEvent::Escaped => {
                if confirm_cancel(backend, draft.is_dirty())? {
                    return Ok(EditorOutcome::Cancelled);
                }
                continue;
            }
            PromptEvent::Interrupted => return Ok(EditorOutcome::Cancelled),
        };
        cursor = selection.min(CANCEL_INDEX);
        match selection {
            0 if edit_environment(backend, &mut draft, environments, effective)? => {
                return Ok(EditorOutcome::Cancelled);
            }
            1 if edit_runtime(backend, &mut draft, effective)? => {
                return Ok(EditorOutcome::Cancelled);
            }
            2 if edit_network(backend, &mut draft, effective)? => {
                return Ok(EditorOutcome::Cancelled);
            }
            3 if edit_worktree(backend, &mut draft, effective)? => {
                return Ok(EditorOutcome::Cancelled);
            }
            4 if edit_home(backend, &mut draft, homes, effective)? => {
                return Ok(EditorOutcome::Cancelled);
            }
            5 if edit_tty(backend, &mut draft, effective)? => {
                return Ok(EditorOutcome::Cancelled);
            }
            6 if edit_rebuild(backend, &mut draft, effective)? => {
                return Ok(EditorOutcome::Cancelled);
            }
            0..=6 => {}
            SAVE_INDEX => {
                let question = format!("Save changes to {}?", draft.path().display());
                match backend.confirm(&question, true)? {
                    PromptEvent::Selected(true) => {
                        return Ok(match context.save_common_settings(draft)? {
                            Some(path) => EditorOutcome::Saved(path),
                            None => EditorOutcome::Unchanged,
                        });
                    }
                    PromptEvent::Selected(false) | PromptEvent::Escaped => {}
                    PromptEvent::Interrupted => return Ok(EditorOutcome::Cancelled),
                }
            }
            CANCEL_INDEX => {
                if confirm_cancel(backend, draft.is_dirty())? {
                    return Ok(EditorOutcome::Cancelled);
                }
            }
            _ => {
                return Err(HostError::Config(
                    "interactive settings menu returned an invalid item".to_owned(),
                ));
            }
        }
    }
}

fn edit_environment(
    backend: &mut impl PromptBackend,
    draft: &mut ConfigDraft,
    environments: &[String],
    effective: &EffectiveSettings,
) -> Result<bool> {
    let choices = environments
        .iter()
        .map(|name| (name.clone(), name.clone()))
        .collect::<Vec<_>>();
    let event = prompt_optional(
        backend,
        "Environment",
        draft.settings().environment.as_ref(),
        &choices,
        &effective.environment,
    )?;
    Ok(apply_edit(event, &mut draft.settings_mut().environment))
}

fn edit_runtime(
    backend: &mut impl PromptBackend,
    draft: &mut ConfigDraft,
    effective: &EffectiveSettings,
) -> Result<bool> {
    let choices = [
        ("auto".to_owned(), RuntimeKind::Auto),
        ("docker".to_owned(), RuntimeKind::Docker),
        ("podman".to_owned(), RuntimeKind::Podman),
    ];
    let event = prompt_optional(
        backend,
        "Runtime",
        draft.settings().runtime.as_ref(),
        &choices,
        &effective.runtime,
    )?;
    Ok(apply_edit(event, &mut draft.settings_mut().runtime))
}

fn edit_network(
    backend: &mut impl PromptBackend,
    draft: &mut ConfigDraft,
    effective: &EffectiveSettings,
) -> Result<bool> {
    let choices = [
        ("offline".to_owned(), NetworkMode::Offline),
        ("allowlist".to_owned(), NetworkMode::Allowlist),
        ("bridge".to_owned(), NetworkMode::Bridge),
        ("host".to_owned(), NetworkMode::Host),
    ];
    let event = prompt_optional(
        backend,
        "Network",
        draft.settings().network.as_ref(),
        &choices,
        &effective.network,
    )?;
    Ok(apply_edit(event, &mut draft.settings_mut().network))
}

fn edit_worktree(
    backend: &mut impl PromptBackend,
    draft: &mut ConfigDraft,
    effective: &EffectiveSettings,
) -> Result<bool> {
    let choices = [
        ("auto".to_owned(), WorktreeMode::Auto),
        ("always".to_owned(), WorktreeMode::Always),
        ("never".to_owned(), WorktreeMode::Never),
    ];
    let event = prompt_optional(
        backend,
        "Worktree",
        draft.settings().worktree.as_ref(),
        &choices,
        &effective.worktree,
    )?;
    Ok(apply_edit(event, &mut draft.settings_mut().worktree))
}

fn edit_home(
    backend: &mut impl PromptBackend,
    draft: &mut ConfigDraft,
    homes: &[String],
    effective: &EffectiveSettings,
) -> Result<bool> {
    let choices = homes
        .iter()
        .map(|name| (name.clone(), name.clone()))
        .collect::<Vec<_>>();
    let event = prompt_optional(
        backend,
        "Codex home",
        draft.settings().home.as_ref(),
        &choices,
        &effective.home,
    )?;
    Ok(apply_edit(event, &mut draft.settings_mut().home))
}

fn edit_tty(
    backend: &mut impl PromptBackend,
    draft: &mut ConfigDraft,
    effective: &EffectiveSettings,
) -> Result<bool> {
    let choices = [
        ("auto".to_owned(), TtyMode::Auto),
        ("always".to_owned(), TtyMode::Always),
        ("never".to_owned(), TtyMode::Never),
    ];
    let event = prompt_optional(
        backend,
        "TTY",
        draft.settings().tty.as_ref(),
        &choices,
        &effective.tty,
    )?;
    Ok(apply_edit(event, &mut draft.settings_mut().tty))
}

fn edit_rebuild(
    backend: &mut impl PromptBackend,
    draft: &mut ConfigDraft,
    effective: &EffectiveSettings,
) -> Result<bool> {
    let choices = [("true".to_owned(), true), ("false".to_owned(), false)];
    let event = prompt_optional(
        backend,
        "Rebuild images",
        draft.settings().rebuild.as_ref(),
        &choices,
        &effective.rebuild,
    )?;
    Ok(apply_edit(event, &mut draft.settings_mut().rebuild))
}

fn apply_edit<T>(event: PromptEvent<Option<T>>, target: &mut Option<T>) -> bool {
    match event {
        PromptEvent::Selected(value) => {
            *target = value;
            false
        }
        PromptEvent::Escaped => false,
        PromptEvent::Interrupted => true,
    }
}

fn confirm_cancel(backend: &mut impl PromptBackend, dirty: bool) -> Result<bool> {
    if !dirty {
        return Ok(true);
    }
    match backend.confirm("Discard unsaved changes?", false)? {
        PromptEvent::Selected(value) => Ok(value),
        PromptEvent::Escaped => Ok(false),
        PromptEvent::Interrupted => Ok(true),
    }
}

fn prompt_optional<T: Clone + PartialEq>(
    backend: &mut impl PromptBackend,
    prompt: &str,
    current: Option<&T>,
    choices: &[(String, T)],
    effective: &EffectiveValue,
) -> Result<PromptEvent<Option<T>>> {
    let mut items = Vec::with_capacity(choices.len() + 1);
    items.push(format!(
        "Inherit / remove target override (current effective: {})",
        effective.summary()
    ));
    items.extend(choices.iter().map(|(label, _)| label.clone()));
    let default = current
        .and_then(|current| choices.iter().position(|(_, value)| value == current))
        .map_or(0, |index| index + 1);
    match backend.select(prompt, &items, default)? {
        PromptEvent::Selected(0) => Ok(PromptEvent::Selected(None)),
        PromptEvent::Selected(index) => choices
            .get(index - 1)
            .map(|(_, value)| PromptEvent::Selected(Some(value.clone())))
            .ok_or_else(|| {
                HostError::Config("interactive value selection returned an invalid item".to_owned())
            }),
        PromptEvent::Escaped => Ok(PromptEvent::Escaped),
        PromptEvent::Interrupted => Ok(PromptEvent::Interrupted),
    }
}

fn menu_items(draft: &ConfigDraft, effective: &EffectiveSettings) -> Vec<String> {
    let settings = draft.settings();
    vec![
        setting_row(
            "Environment",
            settings.environment.clone(),
            &effective.environment,
        ),
        setting_row(
            "Runtime",
            settings.runtime.map(runtime_name).map(str::to_owned),
            &effective.runtime,
        ),
        setting_row(
            "Network",
            settings.network.map(network_name).map(str::to_owned),
            &effective.network,
        ),
        setting_row(
            "Worktree",
            settings.worktree.map(worktree_name).map(str::to_owned),
            &effective.worktree,
        ),
        setting_row("Codex home", settings.home.clone(), &effective.home),
        setting_row(
            "TTY",
            settings.tty.map(tty_name).map(str::to_owned),
            &effective.tty,
        ),
        setting_row(
            "Rebuild images",
            settings.rebuild.map(|value| value.to_string()),
            &effective.rebuild,
        ),
        if draft.is_dirty() {
            "Save changes".to_owned()
        } else {
            "Save (no changes)".to_owned()
        },
        "Cancel".to_owned(),
    ]
}

fn setting_row(name: &str, explicit: Option<String>, effective: &EffectiveValue) -> String {
    explicit.map_or_else(
        || {
            format!(
                "{name}: inherit; current effective: {}",
                effective.summary()
            )
        },
        |value| {
            format!(
                "{name}: {value} [target]; current effective: {}",
                effective.summary()
            )
        },
    )
}

struct EffectiveSettings {
    environment: EffectiveValue,
    runtime: EffectiveValue,
    network: EffectiveValue,
    worktree: EffectiveValue,
    home: EffectiveValue,
    tty: EffectiveValue,
    rebuild: EffectiveValue,
}

impl EffectiveSettings {
    fn new(resolved: &ResolvedConfig) -> Self {
        Self {
            environment: EffectiveValue::new(
                resolved,
                "environment",
                resolved.config.environment.clone(),
            ),
            runtime: EffectiveValue::new(
                resolved,
                "runtime",
                runtime_name(resolved.config.runtime).to_owned(),
            ),
            network: EffectiveValue::new(
                resolved,
                "network",
                network_name(resolved.config.network).to_owned(),
            ),
            worktree: EffectiveValue::new(
                resolved,
                "worktree",
                worktree_name(resolved.config.worktree).to_owned(),
            ),
            home: EffectiveValue::new(resolved, "home", resolved.config.home_name.clone()),
            tty: EffectiveValue::new(resolved, "tty", tty_name(resolved.config.tty).to_owned()),
            rebuild: EffectiveValue::new(resolved, "rebuild", resolved.config.rebuild.to_string()),
        }
    }
}

struct EffectiveValue {
    value: String,
    source: String,
}

impl EffectiveValue {
    fn new(resolved: &ResolvedConfig, path: &str, value: String) -> Self {
        let source = resolved.provenance.source_for(path).map_or_else(
            || "unknown source".to_owned(),
            |source| format!("{}: {}", layer_name(source.kind), source.label),
        );
        Self { value, source }
    }

    fn summary(&self) -> String {
        format!("{} ({})", self.value, self.source)
    }
}

const fn layer_name(kind: ConfigLayerKind) -> &'static str {
    match kind {
        ConfigLayerKind::BuiltIn => "built-in",
        ConfigLayerKind::Environment => "environment",
        ConfigLayerKind::Global => "global",
        ConfigLayerKind::Profile => "profile",
        ConfigLayerKind::Project => "project",
        ConfigLayerKind::EnvironmentVariables => "environment variable",
        ConfigLayerKind::CommandLine => "command line",
    }
}

const fn runtime_name(value: RuntimeKind) -> &'static str {
    match value {
        RuntimeKind::Auto => "auto",
        RuntimeKind::Docker => "docker",
        RuntimeKind::Podman => "podman",
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

const fn worktree_name(value: WorktreeMode) -> &'static str {
    match value {
        WorktreeMode::Auto => "auto",
        WorktreeMode::Always => "always",
        WorktreeMode::Never => "never",
    }
}

const fn tty_name(value: TtyMode) -> &'static str {
    match value {
        TtyMode::Auto => "auto",
        TtyMode::Always => "always",
        TtyMode::Never => "never",
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, fs};

    use super::{EditorOutcome, PromptBackend, PromptEvent, run_with_backend, validate_terminal};
    use crate::{
        cli::OutputFormat,
        configuration::ConfigContext,
        error::{HostError, Result},
        paths::AppPaths,
    };

    #[derive(Default)]
    struct ScriptedBackend {
        selections: VecDeque<PromptEvent<usize>>,
        confirmations: VecDeque<PromptEvent<bool>>,
    }

    impl PromptBackend for ScriptedBackend {
        fn select(
            &mut self,
            _prompt: &str,
            _items: &[String],
            _default: usize,
        ) -> Result<PromptEvent<usize>> {
            self.selections
                .pop_front()
                .ok_or_else(|| HostError::Config("scripted selection queue is empty".to_owned()))
        }

        fn confirm(&mut self, _prompt: &str, _default: bool) -> Result<PromptEvent<bool>> {
            self.confirmations
                .pop_front()
                .ok_or_else(|| HostError::Config("scripted confirmation queue is empty".to_owned()))
        }
    }

    #[test]
    fn scripted_editor_stages_and_saves_one_project_update() {
        let (root, context) = test_context();
        let mut backend = ScriptedBackend {
            selections: VecDeque::from([
                PromptEvent::Selected(0), // project target
                PromptEvent::Selected(2), // network row
                PromptEvent::Selected(1), // offline
                PromptEvent::Selected(7), // save
            ]),
            confirmations: VecDeque::from([PromptEvent::Selected(true)]),
        };

        let outcome = run_with_backend(&context, &mut backend).expect("interactive save");
        assert_eq!(outcome, EditorOutcome::Saved(context.project_file.clone()));
        let contents = fs::read_to_string(&context.project_file).expect("project config");
        assert!(contents.contains("network = \"offline\""));
        drop(root);
    }

    #[test]
    fn scripted_editor_discards_dirty_draft_without_creating_file() {
        let (_root, context) = test_context();
        let mut backend = ScriptedBackend {
            selections: VecDeque::from([
                PromptEvent::Selected(0), // project target
                PromptEvent::Selected(6), // rebuild row
                PromptEvent::Selected(1), // true
                PromptEvent::Selected(8), // cancel
            ]),
            confirmations: VecDeque::from([PromptEvent::Selected(true)]),
        };

        let outcome = run_with_backend(&context, &mut backend).expect("interactive cancel");
        assert_eq!(outcome, EditorOutcome::Cancelled);
        assert!(!context.project_file.exists());
    }

    #[test]
    fn scripted_editor_can_remove_an_explicit_override() {
        let (_root, context) = test_context();
        fs::write(
            &context.project_file,
            "schema_version = 1\n\n[settings]\nnetwork = \"offline\"\n",
        )
        .expect("project config");
        let mut backend = ScriptedBackend {
            selections: VecDeque::from([
                PromptEvent::Selected(0), // project target
                PromptEvent::Selected(2), // network row
                PromptEvent::Selected(0), // inherit
                PromptEvent::Selected(7), // save
            ]),
            confirmations: VecDeque::from([PromptEvent::Selected(true)]),
        };

        let outcome = run_with_backend(&context, &mut backend).expect("interactive inherit");
        assert_eq!(outcome, EditorOutcome::Saved(context.project_file.clone()));
        let contents = fs::read_to_string(&context.project_file).expect("project config");
        assert!(!contents.contains("network"));
    }

    #[test]
    fn field_escape_and_interrupt_cancel_without_writing() {
        let (_root, context) = test_context();
        let mut escaped = ScriptedBackend {
            selections: VecDeque::from([
                PromptEvent::Selected(0), // project target
                PromptEvent::Selected(2), // network row
                PromptEvent::Escaped,     // leave field unchanged
                PromptEvent::Selected(8), // clean cancel
            ]),
            confirmations: VecDeque::new(),
        };
        assert_eq!(
            run_with_backend(&context, &mut escaped).expect("escaped field"),
            EditorOutcome::Cancelled
        );

        let mut interrupted = ScriptedBackend {
            selections: VecDeque::from([PromptEvent::Interrupted]),
            confirmations: VecDeque::new(),
        };
        assert_eq!(
            run_with_backend(&context, &mut interrupted).expect("interrupted target"),
            EditorOutcome::Cancelled
        );
        assert!(!context.project_file.exists());
        assert!(!context.global_file.exists());
    }

    #[test]
    fn terminal_guard_rejects_json_and_missing_terminals() {
        assert!(validate_terminal(OutputFormat::Human, true, true).is_ok());
        assert!(validate_terminal(OutputFormat::Json, true, true).is_err());
        assert!(validate_terminal(OutputFormat::Human, false, true).is_err());
        assert!(validate_terminal(OutputFormat::Human, true, false).is_err());
    }

    fn test_context() -> (tempfile::TempDir, ConfigContext) {
        let root = tempfile::tempdir().expect("root");
        let paths = AppPaths {
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
        (root, context)
    }
}
