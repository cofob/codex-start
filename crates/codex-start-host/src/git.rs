//! Git repository and worktree lifecycle management.

use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use codex_start_core::ProjectIdentity;
use tempfile::NamedTempFile;
use uuid::Uuid;

use crate::{
    command::{CommandSpec, IoMode, run_capture, run_checked, run_interactive},
    error::{HostError, Result},
};

/// Whether a run should use a linked Git worktree.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorktreeMode {
    /// Use worktrees when the current directory is in a repository with a HEAD.
    #[default]
    Auto,
    /// Require a linked worktree.
    Always,
    /// Mount the current repository directly.
    Never,
}

/// Git metadata for the invocation directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRepo {
    /// Canonical invocation directory.
    pub invocation_dir: PathBuf,
    /// Root of the current worktree.
    pub root: PathBuf,
    /// Per-worktree Git directory.
    pub git_dir: PathBuf,
    /// Common Git directory shared between worktrees.
    pub common_dir: PathBuf,
    /// Stable display name.
    pub project_name: String,
    /// Stable canonical-path digest.
    pub project_id: String,
    /// Invocation directory relative to this worktree root.
    pub relative_cwd: PathBuf,
}

/// Workspace selected or created for a run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    /// Host directory mounted into the container.
    pub host_root: PathBuf,
    /// Relative working directory inside the workspace.
    pub relative_cwd: PathBuf,
    /// Worktree display identifier or `direct`.
    pub name: String,
    /// Branch checked out in a linked worktree, when applicable.
    pub branch: Option<String>,
    /// Base commit captured before a new worktree was created.
    pub base_commit: Option<String>,
    /// Whether this invocation created the worktree directory.
    pub created: bool,
    /// Whether this invocation created the branch.
    pub branch_created: bool,
}

impl Workspace {
    /// Create direct-mount workspace metadata.
    pub fn direct(path: PathBuf, relative_cwd: PathBuf) -> Self {
        Self {
            host_root: path,
            relative_cwd,
            name: "direct".to_owned(),
            branch: None,
            base_commit: None,
            created: false,
            branch_created: false,
        }
    }
}

impl GitRepo {
    /// Discover a repository at or above `cwd`.
    pub fn discover(cwd: &Path) -> Result<Option<Self>> {
        let invocation_dir = fs::canonicalize(cwd).map_err(|source| HostError::io(cwd, source))?;
        let probe = run_capture(&CommandSpec::new("git").args([
            "-C",
            invocation_dir.to_string_lossy().as_ref(),
            "rev-parse",
            "--is-inside-work-tree",
        ]))?;
        if !probe.status.success() || probe.stdout_text() != "true" {
            return Ok(None);
        }

        let root = git_absolute(&invocation_dir, "--show-toplevel")?;
        let git_dir = git_absolute(&invocation_dir, "--git-dir")?;
        let common_dir = git_absolute(&invocation_dir, "--git-common-dir")?;
        let identity = ProjectIdentity::git(&root, &common_dir, &invocation_dir)
            .map_err(|error| HostError::Git(error.to_string()))?;

        Ok(Some(Self {
            invocation_dir,
            root,
            git_dir,
            common_dir,
            project_name: identity.display_name,
            project_id: identity.id,
            relative_cwd: identity.relative_workdir,
        }))
    }

    /// Require a repository and produce a helpful error otherwise.
    pub fn require(cwd: &Path) -> Result<Self> {
        Self::discover(cwd)?.ok_or_else(|| {
            HostError::Git(format!("{} is not inside a Git worktree", cwd.display()))
        })
    }

    /// Whether this worktree has an initial commit.
    pub fn has_head(&self) -> Result<bool> {
        Ok(
            run_capture(&self.git_spec(["rev-parse", "--verify", "HEAD"]))?
                .status
                .success(),
        )
    }

    /// Whether the invocation is already in a linked worktree.
    pub fn is_linked(&self) -> bool {
        canonical_or_original(&self.git_dir) != canonical_or_original(&self.common_dir)
    }

    /// Read the current branch, returning `None` for detached HEAD.
    pub fn current_branch(&self) -> Result<Option<String>> {
        let output = run_capture(&self.git_spec(["symbolic-ref", "--quiet", "--short", "HEAD"]))?;
        if output.status.success() {
            Ok(Some(output.stdout_text()))
        } else {
            Ok(None)
        }
    }

    /// Resolve project-private settings inside the shared Git directory.
    pub fn project_config_path(&self) -> PathBuf {
        self.common_dir.join("codex-start.toml")
    }

    /// Create or reuse a run workspace.
    pub fn prepare_workspace(
        &self,
        mode: WorktreeMode,
        requested_name: Option<&str>,
        base_dir: &Path,
        branch_prefix: &str,
    ) -> Result<Workspace> {
        if mode == WorktreeMode::Never {
            return Ok(Workspace::direct(
                self.root.clone(),
                self.relative_cwd.clone(),
            ));
        }
        if !self.has_head()? {
            if mode == WorktreeMode::Always {
                return Err(HostError::Git(
                    "worktree mode was required, but this repository has no HEAD commit".to_owned(),
                ));
            }
            return Ok(Workspace::direct(
                self.root.clone(),
                self.relative_cwd.clone(),
            ));
        }
        if requested_name.is_none() && self.is_linked() {
            return Ok(Workspace {
                host_root: self.root.clone(),
                relative_cwd: self.relative_cwd.clone(),
                name: sanitize_name(
                    self.root
                        .file_name()
                        .unwrap_or_else(|| OsStr::new("linked"))
                        .to_string_lossy()
                        .as_ref(),
                ),
                branch: self.current_branch()?,
                base_commit: None,
                created: false,
                branch_created: false,
            });
        }

        let name = requested_name.map_or_else(generated_name, sanitize_name);
        let branch_component = sanitize_branch_component(requested_name.unwrap_or(&name));
        let prefix = normalize_branch_prefix(branch_prefix)?;
        let branch = format!("{prefix}{branch_component}");
        let project_dir = base_dir.join(format!("{}-{}", self.project_name, self.project_id));
        let worktree_path = project_dir.join(&name);
        fs::create_dir_all(&project_dir).map_err(|source| HostError::io(&project_dir, source))?;

        if worktree_path.exists() {
            let reused =
                self.verify_owned_worktree(&worktree_path, &prefix, Some(&branch), &project_dir)?;
            return Ok(Workspace {
                host_root: worktree_path,
                relative_cwd: self.relative_cwd.clone(),
                name,
                branch: reused.current_branch()?,
                base_commit: None,
                created: false,
                branch_created: false,
            });
        }

        let base_commit = self.rev_parse("HEAD")?;
        let branch_exists = run_capture(&self.git_spec([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ]))?
        .status
        .success();

        if branch_exists {
            run_checked(&self.git_spec([
                "worktree",
                "add",
                worktree_path.to_string_lossy().as_ref(),
                &branch,
            ]))?;
        } else {
            run_checked(&self.git_spec([
                "worktree",
                "add",
                "-b",
                &branch,
                worktree_path.to_string_lossy().as_ref(),
                "HEAD",
            ]))?;
        }

        Ok(Workspace {
            host_root: worktree_path,
            relative_cwd: self.relative_cwd.clone(),
            name,
            branch: Some(branch),
            base_commit: Some(base_commit),
            created: true,
            branch_created: !branch_exists,
        })
    }

    /// Remove a newly created, untouched worktree and its owned branch.
    pub fn cleanup_untouched_workspace(
        &self,
        workspace: &Workspace,
        base_dir: &Path,
        branch_prefix: &str,
    ) -> Result<bool> {
        if !(workspace.created && workspace.branch_created) {
            return Ok(false);
        }
        let Some(branch) = &workspace.branch else {
            return Ok(false);
        };
        let prefix = normalize_branch_prefix(branch_prefix)?;
        if !branch.starts_with(&prefix) || !path_is_under(&workspace.host_root, base_dir)? {
            return Err(HostError::UnsafePath {
                path: workspace.host_root.clone(),
                reason: "worktree ownership could not be proven".to_owned(),
            });
        }
        if !is_clean(&workspace.host_root)? {
            return Ok(false);
        }
        let current = git_text(&workspace.host_root, ["rev-parse", "HEAD"])?;
        if workspace.base_commit.as_deref() != Some(current.as_str()) {
            return Ok(false);
        }
        run_checked(&self.git_spec([
            "worktree",
            "remove",
            "--force",
            workspace.host_root.to_string_lossy().as_ref(),
        ]))?;
        run_checked(&self.git_spec(["branch", "-D", branch]))?;
        Ok(true)
    }

    /// Select a named worktree, or the most recently modified owned worktree.
    pub fn select_workspace(
        &self,
        base_dir: &Path,
        name: Option<&str>,
        branch_prefix: &str,
    ) -> Result<PathBuf> {
        let project_dir = base_dir.join(format!("{}-{}", self.project_name, self.project_id));
        let selected = if let Some(name) = name {
            project_dir.join(sanitize_name(name))
        } else {
            latest_directory(&project_dir)?
        };
        if !selected.is_dir() {
            return Err(HostError::NotFound(format!(
                "worktree {}",
                selected.display()
            )));
        }
        let prefix = normalize_branch_prefix(branch_prefix)?;
        self.verify_owned_worktree(&selected, &prefix, None, &project_dir)?;
        Ok(selected)
    }

    /// Run interactive `git commit` in a selected worktree.
    pub fn commit(worktree: &Path) -> Result<u8> {
        run_interactive(
            &CommandSpec::new("git")
                .args(["-C", worktree.to_string_lossy().as_ref(), "commit"])
                .io(IoMode::Inherit),
        )
    }

    /// Autosave the source and merge it squash-style into this worktree.
    pub fn squash(&self, source: &Path) -> Result<u8> {
        ensure_distinct(source, &self.root)?;
        if !is_clean(&self.root)? {
            return Err(HostError::Git(
                "target worktree must be clean before squash".to_owned(),
            ));
        }
        let branch = git_text(source, ["rev-parse", "--abbrev-ref", "HEAD"])?;
        autosave(source, &format!("codex-start: autosave {branch}"))?;
        run_checked(&self.git_spec(["merge", "--squash", &branch]))?;
        let staged = run_capture(&self.git_spec(["diff", "--cached", "--quiet"]))?;
        if staged.status.success() {
            return Ok(0);
        }
        Self::commit(&self.root)
    }

    /// Apply all source changes to this worktree without committing.
    pub fn move_changes(&self, source: &Path) -> Result<()> {
        ensure_distinct(source, &self.root)?;
        if !is_clean(&self.root)? {
            return Err(HostError::Git(
                "target worktree must be clean before move".to_owned(),
            ));
        }
        let branch = git_text(source, ["rev-parse", "--abbrev-ref", "HEAD"])?;
        let base = merge_base(&self.root, &branch)?;
        let patch_output = run_checked(&CommandSpec::new("git").args([
            "-C",
            source.to_string_lossy().as_ref(),
            "diff",
            "--binary",
            &base,
        ]))?;
        if !patch_output.stdout.is_empty() {
            let patch =
                NamedTempFile::new().map_err(|source| HostError::io("temporary patch", source))?;
            fs::write(patch.path(), &patch_output.stdout)
                .map_err(|source| HostError::io(patch.path(), source))?;
            run_checked(&self.git_spec([
                "apply",
                "--3way",
                "--whitespace=nowarn",
                patch.path().to_string_lossy().as_ref(),
            ]))?;
        }
        copy_untracked(source, &self.root)
    }

    /// Remove owned worktrees and branches after validating scope.
    ///
    /// Without `force`, dirty worktrees are rejected and Git itself preserves
    /// branches that have not been merged into the current branch.
    pub fn cleanup_owned(
        &self,
        base_dir: &Path,
        branch_prefix: &str,
        force: bool,
    ) -> Result<(usize, usize)> {
        let prefix = normalize_branch_prefix(branch_prefix)?;
        if self
            .current_branch()?
            .is_some_and(|branch| branch.starts_with(&prefix))
        {
            return Err(HostError::Git(
                "cannot clean owned branches while the current worktree uses one".to_owned(),
            ));
        }
        let project_dir = base_dir.join(format!("{}-{}", self.project_name, self.project_id));
        if path_is_under(&self.root, &project_dir).unwrap_or(false) {
            return Err(HostError::Git(
                "run cleanup from the main project worktree".to_owned(),
            ));
        }
        run_checked(&self.git_spec(["worktree", "prune"]))?;
        let mut worktrees = 0;
        if project_dir.is_dir() {
            for entry in
                fs::read_dir(&project_dir).map_err(|source| HostError::io(&project_dir, source))?
            {
                let path = entry
                    .map_err(|source| HostError::io(&project_dir, source))?
                    .path();
                if !path.is_dir() {
                    continue;
                }
                self.verify_owned_worktree(&path, &prefix, None, &project_dir)?;
                if !force && has_changes(&path)? {
                    return Err(HostError::Git(format!(
                        "refusing to remove dirty managed worktree {}; commit, move, or rerun cleanup with --force",
                        path.display()
                    )));
                }
                if force {
                    run_checked(&self.git_spec([
                        "worktree",
                        "remove",
                        "--force",
                        path.to_string_lossy().as_ref(),
                    ]))?;
                } else {
                    run_checked(&self.git_spec([
                        "worktree",
                        "remove",
                        path.to_string_lossy().as_ref(),
                    ]))?;
                }
                worktrees += 1;
            }
            let _ = fs::remove_dir(&project_dir);
        }
        let refs = run_checked(&self.git_spec([
            "for-each-ref",
            "--format=%(refname:short)",
            &format!("refs/heads/{prefix}"),
        ]))?
        .stdout_text();
        let mut branches = 0;
        let delete_flag = if force { "-D" } else { "-d" };
        for branch in refs.lines().filter(|line| line.starts_with(&prefix)) {
            if run_capture(&self.git_spec(["branch", delete_flag, branch]))?
                .status
                .success()
            {
                branches += 1;
            }
        }
        run_checked(&self.git_spec(["worktree", "prune"]))?;
        Ok((worktrees, branches))
    }

    fn rev_parse(&self, revision: &str) -> Result<String> {
        git_text(&self.root, ["rev-parse", revision])
    }

    fn git_spec<I, S>(&self, args: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        CommandSpec::new("git")
            .arg("-C")
            .arg(self.root.as_os_str())
            .args(args)
    }

    fn verify_owned_worktree(
        &self,
        path: &Path,
        branch_prefix: &str,
        expected_branch: Option<&str>,
        project_dir: &Path,
    ) -> Result<Self> {
        if !path_is_under(path, project_dir)? {
            return Err(HostError::UnsafePath {
                path: path.to_path_buf(),
                reason: "worktree is outside this project's managed directory".to_owned(),
            });
        }
        let candidate = Self::require(path)?;
        if canonical_or_original(&candidate.common_dir) != canonical_or_original(&self.common_dir) {
            return Err(HostError::Git(format!(
                "refusing foreign repository at managed worktree path {}",
                path.display()
            )));
        }
        let canonical = canonical_or_original(path);
        if !self
            .registered_worktrees()?
            .iter()
            .any(|registered| canonical_or_original(registered) == canonical)
        {
            return Err(HostError::Git(format!(
                "{} is not registered as a worktree of this repository",
                path.display()
            )));
        }
        let branch = candidate.current_branch()?.ok_or_else(|| {
            HostError::Git(format!(
                "managed worktree {} has detached HEAD",
                path.display()
            ))
        })?;
        if expected_branch.is_some_and(|expected| branch != expected)
            || !branch.starts_with(branch_prefix)
        {
            return Err(HostError::Git(format!(
                "managed worktree {} uses unexpected branch {branch:?}",
                path.display()
            )));
        }
        Ok(candidate)
    }

    fn registered_worktrees(&self) -> Result<Vec<PathBuf>> {
        let output = run_checked(&self.git_spec(["worktree", "list", "--porcelain", "-z"]))?;
        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter_map(|field| field.strip_prefix(b"worktree "))
            .map(bytes_to_path)
            .collect())
    }
}

fn git_absolute(cwd: &Path, argument: &str) -> Result<PathBuf> {
    let value = git_text(cwd, ["rev-parse", "--path-format=absolute", argument])?;
    Ok(PathBuf::from(value))
}

fn git_text<I, S>(cwd: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let spec = CommandSpec::new("git")
        .arg("-C")
        .arg(cwd.as_os_str())
        .args(args);
    Ok(run_checked(&spec)?.stdout_text())
}

fn merge_base(target: &Path, branch: &str) -> Result<String> {
    git_text(target, ["merge-base", "HEAD", branch])
}

fn autosave(worktree: &Path, message: &str) -> Result<()> {
    if !has_changes(worktree)? {
        return Ok(());
    }
    run_checked(&CommandSpec::new("git").args([
        "-C",
        worktree.to_string_lossy().as_ref(),
        "add",
        "-A",
    ]))?;
    run_checked(&CommandSpec::new("git").args([
        "-C",
        worktree.to_string_lossy().as_ref(),
        "commit",
        "-m",
        message,
    ]))?;
    Ok(())
}

fn is_clean(worktree: &Path) -> Result<bool> {
    Ok(!has_changes(worktree)?)
}

fn has_changes(worktree: &Path) -> Result<bool> {
    let tracked = run_capture(&CommandSpec::new("git").args([
        "-C",
        worktree.to_string_lossy().as_ref(),
        "status",
        "--porcelain=v1",
        "--untracked-files=normal",
    ]))?;
    tracked
        .require_success(OsStr::new("git"))
        .map(|output| !output.stdout.is_empty())
}

fn copy_untracked(source: &Path, target: &Path) -> Result<()> {
    let output = run_checked(&CommandSpec::new("git").args([
        "-C",
        source.to_string_lossy().as_ref(),
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
    ]))?;
    for bytes in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
    {
        let relative = bytes_to_path(bytes);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(HostError::UnsafePath {
                path: relative,
                reason: "Git returned a non-relative untracked path".to_owned(),
            });
        }
        let from = source.join(&relative);
        let to = target.join(&relative);
        if fs::symlink_metadata(&to).is_ok() {
            return Err(HostError::Git(format!(
                "untracked source would overwrite {}",
                to.display()
            )));
        }
        ensure_safe_parent(target, &relative)?;
        let metadata = fs::symlink_metadata(&from).map_err(|error| HostError::io(&from, error))?;
        if metadata.file_type().is_symlink() {
            let link = fs::read_link(&from).map_err(|error| HostError::io(&from, error))?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(link, &to).map_err(|error| HostError::io(&to, error))?;
        } else if metadata.is_file() {
            fs::copy(&from, &to).map_err(|error| HostError::io(&to, error))?;
            fs::set_permissions(&to, metadata.permissions())
                .map_err(|error| HostError::io(&to, error))?;
        }
    }
    Ok(())
}

fn ensure_safe_parent(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    for component in parent.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(HostError::UnsafePath {
                path: relative.to_path_buf(),
                reason: "destination parent is not a confined relative path".to_owned(),
            });
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(HostError::UnsafePath {
                    path: current,
                    reason: "destination parent is a symlink or non-directory".to_owned(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|source| HostError::io(&current, source))?;
            }
            Err(source) => return Err(HostError::io(&current, source)),
        }
    }
    let canonical_root = fs::canonicalize(root).map_err(|source| HostError::io(root, source))?;
    let canonical_parent =
        fs::canonicalize(&current).map_err(|source| HostError::io(&current, source))?;
    if !canonical_parent.starts_with(canonical_root) {
        return Err(HostError::UnsafePath {
            path: current,
            reason: "destination parent escapes the target worktree".to_owned(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

fn ensure_distinct(left: &Path, right: &Path) -> Result<()> {
    if canonical_or_original(left) == canonical_or_original(right) {
        Err(HostError::Git(
            "source and target worktrees resolve to the same path".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn path_is_under(path: &Path, parent: &Path) -> Result<bool> {
    let parent = fs::canonicalize(parent).map_err(|source| HostError::io(parent, source))?;
    let path = fs::canonicalize(path).map_err(|source| HostError::io(path, source))?;
    Ok(path.starts_with(&parent) && path != parent)
}

fn latest_directory(parent: &Path) -> Result<PathBuf> {
    let entries = fs::read_dir(parent).map_err(|source| HostError::io(parent, source))?;
    entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            path.is_dir().then_some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
        .ok_or_else(|| HostError::NotFound(format!("worktrees under {}", parent.display())))
}

fn normalize_branch_prefix(prefix: &str) -> Result<String> {
    let prefix = prefix.trim().trim_start_matches('/');
    if prefix.is_empty()
        || prefix.contains("..")
        || prefix.chars().any(char::is_whitespace)
        || prefix.starts_with('-')
    {
        return Err(HostError::Config(format!(
            "invalid worktree branch prefix: {prefix:?}"
        )));
    }
    Ok(format!("{}/", prefix.trim_end_matches('/')))
}

fn sanitize_name(value: &str) -> String {
    sanitize(value, true)
}

fn sanitize_branch_component(value: &str) -> String {
    sanitize(value, false)
}

fn sanitize(value: &str, allow_dot: bool) -> String {
    let mut output = String::new();
    let mut previous_dash = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        let allowed = character.is_ascii_alphanumeric()
            || character == '_'
            || (allow_dot && character == '.');
        if allowed {
            output.push(character);
            previous_dash = false;
        } else if !previous_dash && !output.is_empty() {
            output.push('-');
            previous_dash = true;
        }
    }
    while output.ends_with(['-', '.']) {
        output.pop();
    }
    if output.is_empty() {
        "unnamed".to_owned()
    } else {
        output
    }
}

fn generated_name() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    format!("{seconds}-{}", &Uuid::new_v4().simple().to_string()[..8])
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::{
        GitRepo, WorktreeMode, ensure_safe_parent, sanitize_branch_component, sanitize_name,
    };
    use crate::command::{CommandSpec, run_checked};

    fn git(path: &Path, args: &[&str]) {
        run_checked(
            &CommandSpec::new("git")
                .arg("-C")
                .arg(path.as_os_str())
                .args(args.iter().copied()),
        )
        .expect("git command");
    }

    fn repository() -> TempDir {
        let directory = tempfile::tempdir().expect("tempdir");
        git(directory.path(), &["init", "--quiet"]);
        git(
            directory.path(),
            &["config", "user.email", "test@example.com"],
        );
        git(directory.path(), &["config", "user.name", "Test User"]);
        git(directory.path(), &["config", "commit.gpgSign", "false"]);
        fs::write(directory.path().join("README.md"), "base\n").expect("write");
        git(directory.path(), &["add", "README.md"]);
        git(directory.path(), &["commit", "--quiet", "-m", "initial"]);
        directory
    }

    #[test]
    fn names_are_safe_and_stable() {
        assert_eq!(sanitize_name(" Hello / WORLD... "), "hello-world");
        assert_eq!(sanitize_branch_component("Hello.world"), "hello-world");
        assert_eq!(sanitize_name("---"), "unnamed");
    }

    #[test]
    fn creates_and_removes_untouched_worktree() {
        let repo_dir = repository();
        let base = tempfile::tempdir().expect("worktree base");
        let repo = GitRepo::require(repo_dir.path()).expect("discover");
        let workspace = repo
            .prepare_workspace(
                WorktreeMode::Always,
                Some("Feature One"),
                base.path(),
                "codex/",
            )
            .expect("prepare");
        assert!(workspace.host_root.is_dir());
        assert_eq!(workspace.branch.as_deref(), Some("codex/feature-one"));
        assert!(
            repo.cleanup_untouched_workspace(&workspace, base.path(), "codex/")
                .expect("cleanup")
        );
        assert!(!workspace.host_root.exists());
    }

    #[test]
    fn retains_changed_worktree() {
        let repo_dir = repository();
        let base = tempfile::tempdir().expect("worktree base");
        let repo = GitRepo::require(repo_dir.path()).expect("discover");
        let workspace = repo
            .prepare_workspace(WorktreeMode::Always, Some("changed"), base.path(), "codex/")
            .expect("prepare");
        fs::write(workspace.host_root.join("new.txt"), "change").expect("write");
        assert!(
            !repo
                .cleanup_untouched_workspace(&workspace, base.path(), "codex/")
                .expect("cleanup")
        );
        assert!(workspace.host_root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_destination_parent() {
        let target = tempfile::tempdir().expect("target");
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), target.path().join("ignored")).expect("symlink");
        assert!(ensure_safe_parent(target.path(), Path::new("ignored/nested/file.txt")).is_err());
        assert!(!outside.path().join("nested").exists());
    }
}
