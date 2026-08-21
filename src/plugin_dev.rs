//! Third-party BPP plugin project scaffolding and build pipeline.

use std::{
    fs::{self, OpenOptions},
    io::{IsTerminal as _, Read as _},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use dialoguer::{Confirm, Input, Select};
use plugin_api::PluginManifest;
use plugin_host::{MAX_PLUGIN_PACKAGE_BYTES, ValidatedPluginPackage, WasmPlugin};
use semver::Version;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::{
    process::Command as TokioCommand,
    time::{Duration, sleep, timeout},
};
use url::Url;
use wit_component::ComponentEncoder;
use zip::{ZipWriter, write::SimpleFileOptions};

const WIT_SOURCE: &str = include_str!("../crates/plugin-api/wit/bkm-plugin.wit");
const MARKETPLACE_REPOSITORY: &str = "bkmqb-plugins/marketplace";
const READ_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const MUTATION_COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const RECONCILIATION_TIMEOUT: Duration = Duration::from_secs(45);
const ACTIONS_RELEASE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const LOCAL_RELEASE_TAG_MARKER: &str = "[bkmqb-local-release]";

#[derive(Debug)]
struct NewOptions {
    path: Option<PathBuf>,
    plugin_id: Option<String>,
    display_name: Option<String>,
    class_name: Option<String>,
    language: Option<String>,
    region: Option<String>,
    locale: Option<String>,
    description: Option<String>,
    command: Option<String>,
    interactive: Option<bool>,
    initialize_git: bool,
}

impl Default for NewOptions {
    fn default() -> Self {
        Self {
            path: None,
            plugin_id: None,
            display_name: None,
            class_name: None,
            language: None,
            region: None,
            locale: None,
            description: None,
            command: None,
            interactive: None,
            initialize_git: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishMethod {
    Actions,
    Local,
    Manual,
}

#[derive(Debug, Default)]
struct PublishOptions {
    path: Option<PathBuf>,
    tag: Option<String>,
    method: Option<PublishMethod>,
    remote: Option<String>,
    interactive: Option<bool>,
    yes: bool,
    dry_run: bool,
}

pub(crate) async fn run(
    command: &str,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        "new" => create_project(NewOptions::parse(arguments)?)?,
        "check" => {
            check_project(optional_path(arguments)?)?;
        }
        "build" => {
            build_project(optional_path(arguments)?).await?;
        }
        "package" => {
            package_project(optional_path(arguments)?).await?;
        }
        "publish" => publish_project(PublishOptions::parse(arguments)?).await?,
        _ => return Err(format!("unknown plugin development command `{command}`").into()),
    }
    Ok(())
}

impl NewOptions {
    fn parse(arguments: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut options = Self::default();
        let mut index = 0;
        while index < arguments.len() {
            let argument = &arguments[index];
            if !argument.starts_with('-') && options.path.is_none() {
                options.path = Some(PathBuf::from(argument));
                index += 1;
                continue;
            }
            let target = match argument.as_str() {
                "--id" => &mut options.plugin_id,
                "--name" => &mut options.display_name,
                "--class" => &mut options.class_name,
                "--language" => &mut options.language,
                "--region" => &mut options.region,
                "--locale" => &mut options.locale,
                "--description" => &mut options.description,
                "--command" => &mut options.command,
                "--interactive" => {
                    options.interactive = Some(true);
                    index += 1;
                    continue;
                }
                "--no-interactive" => {
                    options.interactive = Some(false);
                    index += 1;
                    continue;
                }
                "--no-git" => {
                    options.initialize_git = false;
                    index += 1;
                    continue;
                }
                unknown => return Err(format!("unknown plugin new option `{unknown}`").into()),
            };
            index += 1;
            *target = Some(option_value(arguments, index, argument)?.to_owned());
            index += 1;
        }
        Ok(options)
    }
}

impl PublishOptions {
    fn parse(arguments: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut options = Self::default();
        let mut index = 0;
        while index < arguments.len() {
            let argument = &arguments[index];
            if !argument.starts_with('-') && options.path.is_none() {
                options.path = Some(PathBuf::from(argument));
                index += 1;
                continue;
            }
            match argument.as_str() {
                "--tag" => {
                    index += 1;
                    options.tag = Some(option_value(arguments, index, "--tag")?.to_owned());
                }
                "--method" => {
                    index += 1;
                    options.method = Some(parse_publish_method(option_value(
                        arguments, index, "--method",
                    )?)?);
                }
                "--remote" => {
                    index += 1;
                    let remote = option_value(arguments, index, "--remote")?;
                    validate_remote_name(remote)?;
                    options.remote = Some(remote.to_owned());
                }
                "--interactive" => options.interactive = Some(true),
                "--no-interactive" => options.interactive = Some(false),
                "-y" | "--yes" => options.yes = true,
                "--dry-run" => options.dry_run = true,
                unknown => return Err(format!("unknown plugin publish option `{unknown}`").into()),
            }
            index += 1;
        }
        Ok(options)
    }
}

fn option_value<'a>(
    arguments: &'a [String],
    index: usize,
    option: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    let value = arguments
        .get(index)
        .ok_or_else(|| format!("{option} requires a value"))?;
    Ok(value)
}

fn validate_remote_name(remote: &str) -> Result<(), Box<dyn std::error::Error>> {
    if remote.is_empty()
        || remote.starts_with('-')
        || remote == "."
        || remote.ends_with('.')
        || remote.contains("..")
        || !remote
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("Git remote name must contain only ASCII letters, digits, `.`, `_`, or `-`, and must not start with `-` or contain `..`".into());
    }
    let probe = format!("refs/remotes/{remote}/probe");
    if !Command::new("git")
        .args(["check-ref-format", &probe])
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return Err(format!("Git remote name `{remote}` cannot form a valid Git ref").into());
    }
    Ok(())
}

fn create_project(options: NewOptions) -> Result<(), Box<dyn std::error::Error>> {
    create_project_with_revision(
        options,
        option_env!("BKMQB_GIT_REV"),
        option_env!("BKMQB_GIT_REPOSITORY"),
    )
}

fn create_project_with_revision(
    mut options: NewOptions,
    revision: Option<&str>,
    source_repository: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let interactive = options
        .interactive
        .unwrap_or_else(|| std::io::stdin().is_terminal());
    let default_path = PathBuf::from("my-plugin");
    let path = if interactive && options.path.is_none() {
        PathBuf::from(prompt("Project directory", "my-plugin")?)
    } else {
        options.path.take().unwrap_or(default_path)
    };
    let slug = project_slug(&path)?;
    let default_name = title_case(&slug);
    let display_name = value_or_prompt(
        options.display_name,
        interactive,
        "Plugin display name",
        &default_name,
    )?;
    let plugin_id = value_or_prompt(
        options.plugin_id,
        interactive,
        "Plugin ID",
        &format!("dev.bkm.{slug}"),
    )?;
    let rust_name = rust_type_name(&slug);
    let default_class_name = if rust_name.starts_with(|character: char| character.is_ascii_digit())
    {
        format!("Plugin{rust_name}")
    } else {
        format!("{rust_name}Plugin")
    };
    let class_name = value_or_prompt(
        options.class_name,
        interactive,
        "Rust plugin type name",
        &default_class_name,
    )?;
    let locale = resolve_locale(
        options.locale,
        options.language,
        options.region,
        interactive,
    )?;
    let description = value_or_prompt(
        options.description,
        interactive,
        "Description",
        "A BroKnowMyQQBot plugin",
    )?;
    let command = value_or_prompt(options.command, interactive, "Command", "hello")?
        .trim_start_matches('/')
        .to_owned();

    validate_generated_values(&plugin_id, &class_name, &locale, &command)?;
    if path.exists() {
        return Err(format!(
            "target project directory `{}` already exists",
            path.display()
        )
        .into());
    }

    let crate_name = slug.replace('_', "-");
    let normalized_library_name = slug.replace('-', "_");
    let library_name = if normalized_library_name
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_digit)
    {
        format!("plugin_{normalized_library_name}")
    } else {
        normalized_library_name
    };
    let manifest = manifest_template(&plugin_id, &locale, &display_name, &description, &command);
    PluginManifest::from_toml(&manifest)?;
    let source = source_template(&class_name, &command);
    let cargo = cargo_template(&crate_name, &library_name);
    let readme = readme_template(&display_name, &command);
    let release_workflow = release_workflow_template(revision, source_repository)?;

    write_project_files(
        &path,
        &cargo,
        &manifest,
        &source,
        &readme,
        &release_workflow,
    )?;

    finish_created_project(&path, options.initialize_git)
}

fn finish_created_project(
    path: &Path,
    initialize_git: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let absolute_path = absolute_path(path)?;
    println!("Plugin created successfully.\n");
    println!("Directory:\n  {}", absolute_path.display());
    if initialize_git {
        match initialize_git_repository(path) {
            Ok(()) => println!("\nGit repository initialized:\n  branch: main"),
            Err(error) => {
                eprintln!("\nGit initialization failed: {error}");
                eprintln!(
                    "Initialize it later with:\n  git -C {} init -b main",
                    shell_display_path(&absolute_path)
                );
                return Err(format!(
                    "plugin files were created at `{}`, but Git repository initialization failed",
                    absolute_path.display()
                )
                .into());
            }
        }
    } else {
        println!("\nGit repository initialization skipped.");
    }
    println!("\nNext steps:");
    println!("  cd {}", shell_display_path(&absolute_path));
    println!("  bkmqb plugin check");
    println!("  bkmqb plugin build");
    println!("  cargo generate-lockfile");
    println!(
        "  git add Cargo.toml Cargo.lock plugin.toml src/lib.rs wit/bkm-plugin.wit README.md .gitignore .github/workflows/release.yml"
    );
    println!("  git commit -m 'Initial plugin'");
    println!("  git remote add origin <repository-url>");
    println!("  bkmqb plugin publish");
    Ok(())
}

fn resolve_locale(
    locale: Option<String>,
    language: Option<String>,
    region: Option<String>,
    interactive: bool,
) -> Result<String, dialoguer::Error> {
    if let Some(locale) = locale {
        return Ok(locale);
    }
    let language = value_or_prompt(language, interactive, "Language", "en")?.to_ascii_lowercase();
    let region =
        value_or_prompt(region, interactive, "Region (optional)", "")?.to_ascii_uppercase();
    Ok(if region.is_empty() {
        language
    } else {
        format!("{language}-{region}")
    })
}

fn write_project_files(
    path: &Path,
    cargo: &str,
    manifest: &str,
    source: &str,
    readme: &str,
    release_workflow: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(path.join("src"))?;
    fs::create_dir_all(path.join("wit"))?;
    fs::create_dir_all(path.join("assets"))?;
    fs::create_dir_all(path.join(".github/workflows"))?;
    fs::write(path.join("Cargo.toml"), cargo)?;
    fs::write(path.join("plugin.toml"), manifest)?;
    fs::write(path.join("src/lib.rs"), source)?;
    fs::write(path.join("wit/bkm-plugin.wit"), WIT_SOURCE)?;
    fs::write(path.join("README.md"), readme)?;
    fs::write(path.join(".gitignore"), "/target\n")?;
    fs::write(path.join(".github/workflows/release.yml"), release_workflow)?;
    Ok(())
}

fn initialize_git_repository(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(path)
        .output()
        .map_err(|error| format!("could not run `git init`: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure("git init -b main", &output).into())
    }
}

fn optional_path(arguments: &[String]) -> Result<&Path, Box<dyn std::error::Error>> {
    if arguments.len() > 1 {
        return Err("expected at most one project path".into());
    }
    Ok(arguments.first().map_or_else(|| Path::new("."), Path::new))
}

async fn publish_project(mut options: PublishOptions) -> Result<(), Box<dyn std::error::Error>> {
    let interactive = options
        .interactive
        .unwrap_or_else(|| std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
    let path = options.path.take().unwrap_or_else(|| PathBuf::from("."));
    let manifest = check_project(&path)?;
    let absolute_project = absolute_path(&path)?;
    let default_tag = format!("v{}", manifest.version);
    let tag = match options.tag.take() {
        Some(tag) => tag,
        None if interactive => prompt("Release tag", &default_tag)?,
        None => default_tag,
    };
    let version = validate_release_tag(&tag)?;
    if version.to_string() != manifest.version {
        return Err(format!(
            "release tag `{tag}` does not match plugin.toml version `{}`; update and commit plugin.toml before publishing",
            manifest.version
        )
        .into());
    }
    let method = match options.method {
        Some(method) => method,
        None if interactive => select_publish_method()?,
        None => PublishMethod::Actions,
    };

    print_publish_plan(&manifest, &absolute_project, &tag, method);
    let remote_name = options.remote.as_deref().unwrap_or("origin");

    if method == PublishMethod::Manual {
        return publish_manual(&path, &manifest, &tag, remote_name, options.dry_run).await;
    }

    let git = prepare_git_publish(&path, remote_name, interactive && !options.dry_run)?;
    let repository = github_repository(&git.remote_url).ok_or_else(|| {
        format!(
            "remote `{}` is not a supported GitHub repository URL; use the manual publishing method for other hosts",
            safe_remote_display(&git.remote_url)
        )
    })?;
    ensure_actions_workflow_committed(&path, method).await?;

    if method == PublishMethod::Local && !options.dry_run {
        ensure_gh_authenticated(&path).await?;
    }

    if options.dry_run {
        ensure_release_tag_available(&path, &git.remote_url, &tag).await?;
    }

    println!("\nRemote changes:");
    println!(
        "  Remote: {remote_name} -> {}",
        safe_remote_display(&git.remote_url)
    );
    println!("  Branch: {}", git.branch);
    println!("  Commit: {}", git.commit);
    println!("  New tag: {tag}");
    if options.dry_run {
        println!(
            "\nDry run complete; no Git tag, push, Release, or marketplace submission was created."
        );
        return Ok(());
    }
    let package = if method == PublishMethod::Local {
        Some(prepare_local_package(&path, &git.commit, &manifest).await?)
    } else {
        None
    };
    if !confirm_remote_publish(interactive, options.yes)? {
        println!("Publishing cancelled; no tag or remote state was changed.");
        return Ok(());
    }

    create_and_push_tag(&path, &git, &tag, method).await?;
    let release_url = format!("https://github.com/{repository}/releases/tag/{tag}");
    match method {
        PublishMethod::Actions => {
            println!("\nTag pushed. GitHub Actions will build and publish the Release:");
            println!("  {release_url}");
            let asset_name = format!("{}-{}.bkm-plugin", manifest.id, manifest.version);
            if wait_for_github_release(&path, &repository, &tag, &asset_name, &manifest).await? {
                submit_marketplace_issue(&path, &manifest, &repository, &tag, &release_url).await?;
            } else {
                println!("\nAfter the GitHub Action creates the Release, submit it with:");
                print_marketplace_submission_command(&manifest, &repository, &tag, &release_url);
            }
        }
        PublishMethod::Local => {
            let package = package.expect("local publish builds a package");
            let package = absolute_path(&package)?;
            create_github_release(&path, &repository, &tag, &package).await?;
            println!("\nGitHub Release created:");
            println!("  {release_url}");
            submit_marketplace_issue(&path, &manifest, &repository, &tag, &release_url).await?;
        }
        PublishMethod::Manual => unreachable!("manual publishing returned above"),
    }
    Ok(())
}

async fn prepare_local_package(
    path: &Path,
    source_commit: &str,
    expected_manifest: &PluginManifest,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    ensure_prepared_commit(path, source_commit)?;
    let package = build_release_package(path, source_commit, expected_manifest).await?;
    ensure_clean_git_tree(path)?;
    ensure_prepared_commit(path, source_commit)?;
    Ok(package)
}

async fn ensure_actions_workflow_committed(
    path: &Path,
    method: PublishMethod,
) -> Result<(), Box<dyn std::error::Error>> {
    if method == PublishMethod::Actions {
        let output = async_command_output(
            "git",
            path,
            &[
                "ls-tree",
                "--name-only",
                "HEAD",
                "--",
                ".github/workflows/release.yml",
            ],
        )
        .await?;
        if !output.status.success() {
            return Err(command_failure("git ls-tree", &output).into());
        }
        if String::from_utf8(output.stdout)?.trim().is_empty() {
            return Err("GitHub Actions publishing requires `.github/workflows/release.yml` to be committed in HEAD; commit the generated workflow or choose another method".into());
        }
    }
    Ok(())
}

async fn publish_manual(
    path: &Path,
    manifest: &PluginManifest,
    tag: &str,
    remote: &str,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if dry_run {
        let source_commit = prepare_manual_source_for_dry_run(path).await?;
        let remote_url = required_push_remote_url_for_dry_run(path, remote).await?;
        ensure_release_tag_available(path, &remote_url, tag).await?;
        println!("\nRemote checks:");
        println!("  Remote: {remote} -> {}", safe_remote_display(&remote_url));
        println!("  Commit: {source_commit}");
        println!("  Available tag: {tag}");
        println!(
            "\nDry run complete; a real manual publish would rebuild and validate the plugin package before printing the guide."
        );
        return Ok(());
    }
    let source_commit = prepare_manual_source(path)?;
    let package = build_release_package(path, &source_commit, manifest).await?;
    ensure_clean_git_tree(path)?;
    ensure_prepared_commit(path, &source_commit)?;
    print_manual_publish_guide(path, manifest, tag, remote, &package, &source_commit)
}

async fn build_release_package(
    repository: &Path,
    source_commit: &str,
    expected_manifest: &PluginManifest,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let output_directory = repository.join("target/bkmqb");
    let package = default_package_path(&output_directory, expected_manifest);
    let package_relative = package.strip_prefix(repository).map_err(|_| {
        format!(
            "release package `{}` is outside the plugin repository",
            package.display()
        )
    })?;
    if !git_command_succeeds(
        repository,
        &[
            "check-ignore",
            "-q",
            "--no-index",
            "--",
            package_relative
                .to_str()
                .ok_or("release package path is not valid UTF-8")?,
        ],
    ) {
        return Err(format!(
            "release output `{}` is not ignored by Git; add `/target` to .gitignore before publishing",
            package.display()
        )
        .into());
    }
    let worktree = ReleaseWorktree::create(repository, source_commit)?;
    let built_package = build_project(worktree.path()).await?;
    let file_name = built_package
        .file_name()
        .ok_or("built plugin package has no file name")?;
    ensure_safe_package_output_directory(repository, &output_directory)?;
    let package = output_directory.join(file_name);
    copy_package_atomically(&built_package, &package)?;
    let validated = ValidatedPluginPackage::from_path(&package)?;
    if validated.manifest().id != expected_manifest.id
        || validated.manifest().version != expected_manifest.version
    {
        return Err(format!(
            "committed plugin manifest is {} {}, but the requested release is {} {}",
            validated.manifest().id,
            validated.manifest().version,
            expected_manifest.id,
            expected_manifest.version
        )
        .into());
    }
    Ok(package)
}

fn ensure_safe_package_output_directory(
    repository: &Path,
    output_directory: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let target_directory = repository.join("target");
    for directory in [&target_directory, output_directory] {
        match fs::symlink_metadata(directory) {
            Ok(metadata)
                if metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && !is_windows_reparse_point(&metadata) => {}
            Ok(_) => {
                return Err(format!(
                    "release output directory `{}` must be a real directory, not a symlink or reparse point",
                    directory.display()
                )
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(directory)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn copy_package_atomically(
    source: &Path,
    destination: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || is_windows_reparse_point(&metadata)
        {
            return Err(format!(
                "release package destination `{}` must be a regular file",
                destination.display()
            )
            .into());
        }
        #[cfg(windows)]
        {
            if sha256_file(source)? == sha256_file(destination)? {
                return Ok(());
            }
            return Err(format!(
                "release package `{}` already exists with different contents; remove it before publishing so Windows does not expose a non-atomic replacement gap",
                destination.display()
            )
            .into());
        }
    }
    let parent = destination
        .parent()
        .ok_or("release package destination has no parent directory")?;
    let mut temporary = None;
    for attempt in 0..100_u32 {
        let path = parent.join(format!(
            ".bkmqb-release-{}-{}-{attempt}.tmp",
            std::process::id(),
            timestamp_nanos()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                temporary = Some((path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let Some((temporary_path, mut output)) = temporary else {
        return Err("could not allocate a temporary release package file".into());
    };
    let mut input = open_regular_file_without_following_symlinks(source)?;
    if let Err(error) = std::io::copy(&mut input, &mut output) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error.into());
    }
    drop(output);
    if let Err(error) = replace_package_file(&temporary_path, destination) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error.into());
    }
    Ok(())
}

fn print_publish_plan(manifest: &PluginManifest, project: &Path, tag: &str, method: PublishMethod) {
    println!("\nPublish plan:");
    println!("  Plugin: {}", manifest.id);
    println!("  Project: {}", project.display());
    println!("  Version: {}", manifest.version);
    println!("  Tag: {tag}");
    println!("  Method: {}", method.label());
}

impl PublishMethod {
    const fn label(self) -> &'static str {
        match self {
            Self::Actions => "GitHub Actions",
            Self::Local => "local build and GitHub upload",
            Self::Manual => "manual guide",
        }
    }
}

fn parse_publish_method(value: &str) -> Result<PublishMethod, Box<dyn std::error::Error>> {
    match value {
        "actions" => Ok(PublishMethod::Actions),
        "local" => Ok(PublishMethod::Local),
        "manual" => Ok(PublishMethod::Manual),
        _ => Err("--method must be one of: actions, local, manual".into()),
    }
}

fn select_publish_method() -> Result<PublishMethod, dialoguer::Error> {
    let choices = [
        "GitHub Actions (recommended)",
        "Build and upload locally",
        "Show manual publishing guide",
    ];
    Ok(
        match Select::new()
            .with_prompt("Publishing method")
            .items(&choices)
            .default(0)
            .interact()?
        {
            0 => PublishMethod::Actions,
            1 => PublishMethod::Local,
            _ => PublishMethod::Manual,
        },
    )
}

fn validate_release_tag(tag: &str) -> Result<Version, Box<dyn std::error::Error>> {
    let Some(version) = tag.strip_prefix('v') else {
        return Err("release tag must use `v<major>.<minor>.<patch>`".into());
    };
    let version =
        Version::parse(version).map_err(|_| "release tag must use `v<major>.<minor>.<patch>`")?;
    if !version.pre.is_empty()
        || !version.build.is_empty()
        || tag != format!("v{}.{}.{}", version.major, version.minor, version.patch)
    {
        return Err("release tag must use `v<major>.<minor>.<patch>`".into());
    }
    Ok(version)
}

#[derive(Debug)]
struct GitPublishContext {
    project: PathBuf,
    remote_url: String,
    branch: String,
    commit: String,
}

fn prepare_git_publish(
    path: &Path,
    remote: &str,
    interactive: bool,
) -> Result<GitPublishContext, Box<dyn std::error::Error>> {
    validate_remote_name(remote)?;
    let repository_root = git_output(path, &["rev-parse", "--show-toplevel"]).map_err(
        |_| "plugin project is not inside a Git repository; run `git init -b main` first",
    )?;
    if absolute_path(path)? != absolute_path(Path::new(&repository_root))? {
        return Err("plugin publishing currently requires the plugin project to be the Git repository root; publish a standalone scaffold repository or use the manual guide".into());
    }
    ensure_clean_git_tree(path)?;
    let branch = git_output(path, &["branch", "--show-current"])?;
    if branch.is_empty() {
        return Err("cannot publish from detached HEAD; check out a branch first".into());
    }
    let commit = git_output(path, &["rev-parse", "HEAD"])?;
    let remote_url = match git_output_lines(path, &["remote", "get-url", "--push", "--all", remote])
    {
        Ok(urls) if urls.len() == 1 => urls.into_iter().next().expect("one push URL"),
        Ok(urls) => {
            return Err(format!(
                "Git remote `{remote}` must have exactly one effective push URL, found {}",
                urls.len()
            )
            .into());
        }
        Err(_) if interactive => {
            let add = Confirm::new()
                .with_prompt(format!("Git remote `{remote}` was not found. Add it now?"))
                .default(true)
                .interact()?;
            if !add {
                return Err(format!(
                    "Git remote `{remote}` is required; add it with `git remote add {remote} <repository-url>`"
                )
                .into());
            }
            let url = prompt("Repository URL", "")?;
            if url.trim().is_empty() {
                return Err("repository URL cannot be empty".into());
            }
            if remote_url_has_credentials(&url) {
                return Err("repository URL must not contain embedded credentials; configure Git or GitHub CLI credentials separately".into());
            }
            if github_repository(&url).is_none() {
                return Err("automatic publishing requires an HTTPS or `git@github.com:`/`ssh://git@github.com/` repository URL without query or fragment; use the manual method for other hosts".into());
            }
            git_status(path, &["remote", "add", remote, &url])?;
            url
        }
        Err(_) => {
            return Err(format!(
                "Git remote `{remote}` is required; add it with `git remote add {remote} <repository-url>`"
            )
            .into());
        }
    };
    Ok(GitPublishContext {
        project: path.to_path_buf(),
        remote_url,
        branch,
        commit,
    })
}

fn ensure_clean_git_tree(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let status = git_output(path, &["status", "--porcelain", "--untracked-files=normal"])?;
    if status.trim().is_empty() {
        return Ok(());
    }
    Err(format!(
        "Git working tree is not clean; commit or discard the following changes before publishing:\n{status}"
    )
    .into())
}

fn ensure_prepared_commit(
    path: &Path,
    expected_commit: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let current_commit = git_output(path, &["rev-parse", "HEAD"])?;
    if current_commit == expected_commit {
        Ok(())
    } else {
        Err(format!(
            "HEAD changed from prepared commit {expected_commit} to {current_commit}; rebuild and retry the release"
        )
        .into())
    }
}

fn prepare_manual_source(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let repository_root = git_output(path, &["rev-parse", "--show-toplevel"]).map_err(|_| {
        "manual publishing requires a Git repository so the package can be tied to committed source"
    })?;
    if absolute_path(path)? != absolute_path(Path::new(&repository_root))? {
        return Err(
            "manual publishing currently requires the plugin project to be the Git repository root"
                .into(),
        );
    }
    ensure_clean_git_tree(path)?;
    git_output(path, &["rev-parse", "HEAD"])
}

async fn prepare_manual_source_for_dry_run(
    path: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let repository_root = async_git_output(path, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|error| {
            contextual_error(
                "manual publishing requires a Git repository so the package can be tied to committed source",
                error,
            )
        })?;
    if absolute_path(path)? != absolute_path(Path::new(&repository_root))? {
        return Err(
            "manual publishing currently requires the plugin project to be the Git repository root"
                .into(),
        );
    }
    let status =
        async_git_output(path, &["status", "--porcelain", "--untracked-files=normal"]).await?;
    if !status.trim().is_empty() {
        return Err(format!(
            "Git working tree is not clean; commit or discard the following changes before publishing:\n{status}"
        )
        .into());
    }
    async_git_output(path, &["rev-parse", "HEAD"]).await
}

async fn required_push_remote_url_for_dry_run(
    path: &Path,
    remote: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    validate_remote_name(remote)?;
    let urls = async_git_output_lines(path, &["remote", "get-url", "--push", "--all", remote])
        .await
        .map_err(|error| {
            contextual_error(
                format!("could not resolve push URL for Git remote `{remote}`"),
                error,
            )
        })?;
    if urls.len() != 1 {
        return Err(format!(
            "Git remote `{remote}` must have exactly one effective push URL, found {}",
            urls.len()
        )
        .into());
    }
    let url = urls.into_iter().next().expect("one push URL");
    if remote_url_has_credentials(&url) {
        return Err("Git remote URL must not contain embedded credentials; configure Git or GitHub CLI credentials separately".into());
    }
    Ok(url)
}

fn confirm_remote_publish(
    interactive: bool,
    yes: bool,
) -> Result<bool, Box<dyn std::error::Error>> {
    if yes {
        return Ok(true);
    }
    if !interactive {
        return Err("remote publishing requires `--yes` in non-interactive mode".into());
    }
    Ok(Confirm::new()
        .with_prompt("Push the current commit and release tag?")
        .default(true)
        .interact()?)
}

async fn create_and_push_tag(
    path: &Path,
    git: &GitPublishContext,
    tag: &str,
    method: PublishMethod,
) -> Result<(), Box<dyn std::error::Error>> {
    let tag_message = if method == PublishMethod::Local {
        format!("Release {tag}\n\n{LOCAL_RELEASE_TAG_MARKER}")
    } else {
        format!("Release {tag}")
    };
    ensure_clean_git_tree(path)?;
    ensure_prepared_commit(path, &git.commit)?;
    let local_tag_was_present = git_command_succeeds(
        path,
        &["rev-parse", "--verify", &format!("refs/tags/{tag}")],
    );
    let remote_tag = remote_tag_details(path, &git.remote_url, tag).await?;
    if remote_tag.is_some() && !local_tag_was_present {
        return Err(format!(
            "remote Git tag `{tag}` already exists, but no local tag is available to verify its publishing mode"
        )
        .into());
    }
    let local_tag_existed = ensure_local_release_tag(path, tag, method, &git.commit, &tag_message)?;
    let local_commit = git.commit.clone();
    let branch_ref = format!("{}:refs/heads/{}", git.commit, git.branch);
    let tag_ref = format!("refs/tags/{tag}");
    if let Some(remote_tag) = remote_tag.as_ref() {
        let local_tag_object = git_output(path, &["rev-parse", &format!("refs/tags/{tag}")])?;
        if remote_tag.object_id != local_tag_object {
            return Err(format!(
                "remote Git tag `{tag}` has a different annotation object from the verified local tag"
            )
            .into());
        }
        if remote_tag.peeled_commit != local_commit {
            return Err(format!(
                "remote Git tag `{tag}` points to {}, not the prepared release commit {local_commit}",
                remote_tag.peeled_commit
            )
            .into());
        }
        if !local_tag_existed {
            return Err(format!(
                "remote Git tag `{tag}` already exists at the prepared commit, but no pre-existing local tag was available to verify its publishing mode"
            )
            .into());
        }
        if remote_branch_commit(path, &git.remote_url, &git.branch)
            .await?
            .as_deref()
            == Some(local_commit.as_str())
        {
            println!(
                "Release tag and branch already exist at the prepared commit; resuming publish."
            );
            return Ok(());
        }
    }
    let push_arguments = if remote_tag.is_some() {
        vec!["push", &git.remote_url, &branch_ref]
    } else {
        vec!["push", "--atomic", &git.remote_url, &branch_ref, &tag_ref]
    };
    if let Err(error) =
        async_command_status_with_timeout("git", path, &push_arguments, MUTATION_COMMAND_TIMEOUT)
            .await
    {
        return reconcile_failed_git_push(
            path,
            git,
            tag,
            &local_commit,
            (&branch_ref, &tag_ref),
            remote_tag.is_some(),
            &error.to_string(),
        )
        .await;
    }
    Ok(())
}

async fn reconcile_failed_git_push(
    path: &Path,
    git: &GitPublishContext,
    tag: &str,
    local_commit: &str,
    refs: (&str, &str),
    remote_tag_existed: bool,
    error: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let (branch_ref, tag_ref) = refs;
    match wait_for_remote_tag_details(path, &git.remote_url, tag).await {
        Ok(Some(remote_tag)) if remote_tag.peeled_commit == local_commit => {
            let local_tag_object = git_output(path, &["rev-parse", &format!("refs/tags/{tag}")])?;
            if remote_tag.object_id != local_tag_object {
                return Err(format!(
                    "{error}\nRemote tag `{tag}` points to the release commit but has a different annotation object; refusing to continue."
                )
                .into());
            }
            match wait_for_remote_branch_commit(path, &git.remote_url, &git.branch, local_commit)
                .await
            {
                Ok(Some(branch_commit)) if branch_commit == local_commit => {
                    println!(
                        "Git push result was interrupted, but the remote branch and release tag are present."
                    );
                    return Ok(());
                }
                Ok(Some(branch_commit)) => {
                    return Err(format!(
                            "{error}\nRemote tag `{tag}` points to {local_commit}, but branch `{}` points to {branch_commit}; refusing to continue.",
                            git.branch
                        )
                        .into());
                }
                Ok(None) => {
                    return Err(format!(
                            "{error}\nRemote tag `{tag}` is present, but branch `{}` was not confirmed at {local_commit}; check the remote before retrying.",
                            git.branch
                        )
                        .into());
                }
                Err(reconcile_error) => {
                    return Err(format!(
                            "{error}\nRemote tag `{tag}` is present, but branch `{}` could not be verified: {reconcile_error}\nCheck the remote before retrying.",
                            git.branch
                        )
                        .into());
                }
            }
        }
        Ok(Some(remote_tag)) => {
            return Err(format!(
                "{error}\nRemote tag `{tag}` points to {}, not the local release commit {local_commit}; refusing to continue.",
                remote_tag.peeled_commit
            )
                .into());
        }
        Ok(None) => {}
        Err(reconcile_error) => {
            return Err(format!(
                    "{error}\nThe local tag was kept and remote state could not be confirmed: {reconcile_error}\nCheck the remote before retrying."
                )
                .into());
        }
    }
    let retry_push = if remote_tag_existed {
        format!(
            "push {} {}",
            shell_quote(&git.remote_url),
            shell_quote(branch_ref)
        )
    } else {
        format!(
            "push --atomic {} {} {}",
            shell_quote(&git.remote_url),
            shell_quote(branch_ref),
            shell_quote(tag_ref)
        )
    };
    Err(format!(
        "{error}\nThe local tag was kept. Retry with:\n  git -C {} {retry_push}",
        shell_display_path(&git.project),
    )
    .into())
}

fn ensure_local_release_tag(
    path: &Path,
    tag: &str,
    method: PublishMethod,
    expected_commit: &str,
    tag_message: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let tag_ref = format!("refs/tags/{tag}");
    if !git_command_succeeds(path, &["rev-parse", "--verify", &tag_ref]) {
        git_status(
            path,
            &["tag", "-a", tag, "-m", tag_message, expected_commit],
        )?;
        return Ok(false);
    }
    let object_type = git_output(path, &["cat-file", "-t", &tag_ref])?;
    if object_type != "tag" {
        return Err(format!("local Git tag `{tag}` must be an annotated tag").into());
    }
    let commit = git_output(path, &["rev-list", "-n", "1", tag])?;
    if commit != expected_commit {
        return Err(format!(
            "local Git tag `{tag}` points to {commit}, not the prepared release commit {expected_commit}"
        )
        .into());
    }
    let contents = git_output(path, &["for-each-ref", "--format=%(contents)", &tag_ref])?;
    let has_local_marker = contents.contains(LOCAL_RELEASE_TAG_MARKER);
    if has_local_marker != (method == PublishMethod::Local) {
        return Err(
            format!("local Git tag `{tag}` was created for a different publishing method").into(),
        );
    }
    Ok(true)
}

async fn wait_for_remote_branch_commit(
    path: &Path,
    remote_url: &str,
    branch: &str,
    expected_commit: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut last_observed = None;
    let mut last_error = None;
    let poll = async {
        let mut delay = Duration::from_secs(2);
        loop {
            match remote_branch_commit(path, remote_url, branch).await {
                Ok(Some(commit)) if commit == expected_commit => return Ok(Some(commit)),
                Ok(commit) => {
                    last_observed = commit;
                    sleep_with_backoff(&mut delay).await;
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                    sleep_with_backoff(&mut delay).await;
                }
            }
        }
    };
    match timeout(RECONCILIATION_TIMEOUT, poll).await {
        Ok(result) => result,
        Err(_) => match (last_observed, last_error) {
            (Some(commit), _) => Ok(Some(commit)),
            (None, Some(error)) => Err(format!(
                "remote branch could not be confirmed before the reconciliation deadline; last query error: {error}"
            )
            .into()),
            (None, None) => Ok(None),
        },
    }
}

async fn remote_branch_commit(
    path: &Path,
    remote_url: &str,
    branch: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let branch_ref = format!("refs/heads/{branch}");
    let output = async_command_output(
        "git",
        path,
        &["ls-remote", "--heads", remote_url, &branch_ref],
    )
    .await?;
    if !output.status.success() {
        return Err(command_failure("git ls-remote", &output).into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout.split_whitespace().next().map(str::to_owned))
}

async fn wait_for_remote_tag_details(
    path: &Path,
    remote_url: &str,
    tag: &str,
) -> Result<Option<RemoteTagDetails>, Box<dyn std::error::Error>> {
    let mut last_error = None;
    let poll = async {
        let mut delay = Duration::from_secs(2);
        loop {
            match remote_tag_details(path, remote_url, tag).await {
                Ok(Some(details)) => return Ok(Some(details)),
                Ok(None) => sleep_with_backoff(&mut delay).await,
                Err(error) => {
                    last_error = Some(error.to_string());
                    sleep_with_backoff(&mut delay).await;
                }
            }
        }
    };
    match timeout(RECONCILIATION_TIMEOUT, poll).await {
        Ok(result) => result,
        Err(_) => match last_error {
            Some(error) => Err(format!(
                "remote tag could not be confirmed before the reconciliation deadline; last query error: {error}"
            )
            .into()),
            None => Ok(None),
        },
    }
}

struct RemoteTagDetails {
    object_id: String,
    peeled_commit: String,
}

async fn ensure_release_tag_available(
    path: &Path,
    remote_url: &str,
    tag: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let tag_ref = format!("refs/tags/{tag}");
    let output = async_command_output("git", path, &["rev-parse", "--verify", "--quiet", &tag_ref])
        .await
        .map_err(|error| {
            contextual_error(format!("could not check local release tag `{tag}`"), error)
        })?;
    if output.status.success() {
        return Err(format!("release Git tag `{tag}` already exists locally").into());
    }
    if output.status.code() != Some(1) {
        return Err(command_failure("git rev-parse --verify", &output).into());
    }
    if remote_tag_details(path, remote_url, tag).await?.is_some() {
        return Err(format!("release Git tag `{tag}` already exists on the publish remote").into());
    }
    Ok(())
}

async fn remote_tag_details(
    path: &Path,
    remote_url: &str,
    tag: &str,
) -> Result<Option<RemoteTagDetails>, Box<dyn std::error::Error>> {
    let direct_ref = format!("refs/tags/{tag}");
    let peeled_ref = format!("{direct_ref}^{{}}");
    let output = async_command_output(
        "git",
        path,
        &["ls-remote", "--tags", remote_url, &direct_ref, &peeled_ref],
    )
    .await?;
    if !output.status.success() {
        return Err(command_failure("git ls-remote", &output).into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    let mut direct = None;
    let mut peeled = None;
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        let Some(commit) = fields.next() else {
            continue;
        };
        match fields.next() {
            Some(reference) if reference == peeled_ref => peeled = Some(commit.to_owned()),
            Some(reference) if reference == direct_ref => direct = Some(commit.to_owned()),
            _ => {}
        }
    }
    Ok(direct.map(|object_id| RemoteTagDetails {
        peeled_commit: peeled.unwrap_or_else(|| object_id.clone()),
        object_id,
    }))
}

async fn ensure_gh_authenticated(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let output = async_command_output(
        "gh",
        path,
        &["auth", "status", "--hostname", "github.com"],
    )
    .await
    .map_err(|error| {
        format!("GitHub CLI is required for local publishing: {error}. Install it and run `gh auth login --hostname github.com`.")
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err("GitHub CLI is not authenticated for github.com; run `gh auth login --hostname github.com` and retry".into())
    }
}

async fn gh_is_authenticated(path: &Path) -> bool {
    async_command_output("gh", path, &["auth", "status", "--hostname", "github.com"])
        .await
        .is_ok_and(|output| output.status.success())
}

async fn wait_for_github_release(
    path: &Path,
    repository: &str,
    tag: &str,
    expected_asset: &str,
    expected_manifest: &PluginManifest,
) -> Result<bool, Box<dyn std::error::Error>> {
    if !gh_is_authenticated(path).await {
        return Ok(false);
    }
    println!("Waiting for the GitHub Action to publish the Release...");
    let repository = format!("github.com/{repository}");
    let mut last_error = None;
    let poll = async {
        let mut delay = Duration::from_secs(2);
        loop {
            match github_release_asset_size(path, &repository, tag, expected_asset).await {
                Ok(Some(0) | None) => sleep_with_backoff(&mut delay).await,
                Ok(Some(size)) => {
                    if size > u64::try_from(MAX_PLUGIN_PACKAGE_BYTES).unwrap_or(u64::MAX) {
                        return Err::<bool, Box<dyn std::error::Error>>(format!(
                            "GitHub Release asset `{expected_asset}` is {size} bytes, exceeding the {MAX_PLUGIN_PACKAGE_BYTES} byte package limit"
                        )
                        .into());
                    }
                    match download_github_release_asset(path, &repository, tag, expected_asset)
                        .await
                    {
                        Ok(Some(downloaded)) => {
                            validate_downloaded_release_asset(&downloaded, expected_manifest)
                                .await?;
                            return Ok(true);
                        }
                        Ok(None) => sleep_with_backoff(&mut delay).await,
                        Err(error) => {
                            last_error = Some(error.to_string());
                            sleep_with_backoff(&mut delay).await;
                        }
                    }
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                    sleep_with_backoff(&mut delay).await;
                }
            }
        }
    };
    match timeout(ACTIONS_RELEASE_TIMEOUT, poll).await {
        Ok(Ok(true)) => {
            println!("GitHub Release and plugin package asset are available.");
            Ok(true)
        }
        Ok(Ok(false)) => unreachable!("release poll only returns true"),
        Ok(Err(error)) => Err(format!(
            "the release tag was pushed, but the published plugin asset failed validation: {error}"
        )
        .into()),
        Err(_) => {
            if let Some(error) = last_error {
                eprintln!(
                    "GitHub Release was not confirmed within {} minutes; the last query error was: {error}",
                    ACTIONS_RELEASE_TIMEOUT.as_secs() / 60
                );
            } else {
                eprintln!(
                    "GitHub Release was not visible within {} minutes; the Action may still be running or may have failed.",
                    ACTIONS_RELEASE_TIMEOUT.as_secs() / 60
                );
            }
            Ok(false)
        }
    }
}

async fn create_github_release(
    path: &Path,
    repository: &str,
    tag: &str,
    package: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let repository = format!("github.com/{repository}");
    let expected_name = package
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("plugin package file name is not valid UTF-8")?;
    let expected_digest = sha256_file(package)?;
    let result = async_command_status_with_timeout(
        "gh",
        path,
        &[
            "release",
            "create",
            tag,
            package
                .to_str()
                .ok_or("plugin package path is not valid UTF-8")?,
            "--repo",
            &repository,
            "--verify-tag",
            "--generate-notes",
            "--title",
            tag,
        ],
        MUTATION_COMMAND_TIMEOUT,
    )
    .await;
    match result {
        Ok(()) => match wait_for_release_asset_digest(path, &repository, tag, expected_name).await {
            Ok(Some(digest)) if digest == expected_digest => Ok(()),
            Ok(Some(digest)) => Err(format!(
                "GitHub Release asset `{expected_name}` has SHA-256 {digest}, expected {expected_digest}; refusing marketplace submission"
            )
            .into()),
            Ok(None) => Err(format!(
                "GitHub Release was created but asset `{expected_name}` is missing; check the Release before retrying"
            )
            .into()),
            Err(error) => Err(format!(
                "GitHub Release was created but its asset could not be verified: {error}"
            )
            .into()),
        },
        Err(error) => match wait_for_release_asset_digest(path, &repository, tag, expected_name).await {
            Ok(Some(digest)) if digest == expected_digest => {
                println!(
                    "Release creation result was interrupted, but the expected package asset exists."
                );
                Ok(())
            }
            Ok(Some(digest)) => Err(format!(
                    "{error}\nGitHub Release asset `{expected_name}` exists with SHA-256 {digest}, expected {expected_digest}; remote state conflicts with the local package."
                )
                .into()),
            Ok(None) => Err(format!(
                    "{error}\nThe pushed tag was kept, but no matching Release asset became visible within {} seconds. Check GitHub before retrying to avoid creating conflicting release state.",
                    RECONCILIATION_TIMEOUT.as_secs()
                )
                .into()),
            Err(reconcile_error) => Err(format!(
                    "{error}\nThe pushed tag was kept and Release state could not be confirmed: {reconcile_error}\nCheck GitHub before retrying."
                )
                .into()),
        },
    }
}

async fn wait_for_release_asset_digest(
    path: &Path,
    repository: &str,
    tag: &str,
    expected_asset: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut last_error = None;
    let poll = async {
        let mut delay = Duration::from_secs(2);
        loop {
            match github_release_asset_digest(path, repository, tag, expected_asset).await {
                Ok(Some(digest)) => return Ok(Some(digest)),
                Ok(None) => sleep_with_backoff(&mut delay).await,
                Err(error) => {
                    last_error = Some(error.to_string());
                    sleep_with_backoff(&mut delay).await;
                }
            }
        }
    };
    match timeout(RECONCILIATION_TIMEOUT, poll).await {
        Ok(result) => result,
        Err(_) => match last_error {
            Some(error) => Err(format!(
                "release asset could not be confirmed before the reconciliation deadline; last query error: {error}"
            )
            .into()),
            None => Ok(None),
        },
    }
}

async fn github_release_asset_digest(
    path: &Path,
    repository: &str,
    tag: &str,
    expected_asset: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let Some(downloaded) =
        download_github_release_asset(path, repository, tag, expected_asset).await?
    else {
        return Ok(None);
    };
    if downloaded.size > u64::try_from(MAX_PLUGIN_PACKAGE_BYTES).unwrap_or(u64::MAX) {
        return Err(format!(
            "downloaded GitHub Release asset is {} bytes, exceeding the {MAX_PLUGIN_PACKAGE_BYTES} byte package limit",
            downloaded.size
        )
        .into());
    }
    Ok(Some(sha256_file(&downloaded.path)?))
}

async fn validate_downloaded_release_asset(
    downloaded: &DownloadedReleaseAsset,
    expected_manifest: &PluginManifest,
) -> Result<(), Box<dyn std::error::Error>> {
    if downloaded.size > u64::try_from(MAX_PLUGIN_PACKAGE_BYTES).unwrap_or(u64::MAX) {
        return Err(format!(
            "downloaded GitHub Release asset is {} bytes, exceeding the {MAX_PLUGIN_PACKAGE_BYTES} byte package limit",
            downloaded.size
        )
        .into());
    }
    let package = ValidatedPluginPackage::from_path(&downloaded.path)?;
    let actual_manifest = package.manifest();
    if actual_manifest.id != expected_manifest.id
        || actual_manifest.version != expected_manifest.version
    {
        return Err(format!(
            "GitHub Release asset manifest is {} {}, expected {} {}",
            actual_manifest.id,
            actual_manifest.version,
            expected_manifest.id,
            expected_manifest.version
        )
        .into());
    }
    WasmPlugin::from_package(package).await?;
    Ok(())
}

struct DownloadedReleaseAsset {
    _directory: TemporaryDirectory,
    path: PathBuf,
    size: u64,
}

async fn download_github_release_asset(
    path: &Path,
    repository: &str,
    tag: &str,
    expected_asset: &str,
) -> Result<Option<DownloadedReleaseAsset>, Box<dyn std::error::Error>> {
    let Some(size) = github_release_asset_size(path, repository, tag, expected_asset).await? else {
        return Ok(None);
    };
    if size > u64::try_from(MAX_PLUGIN_PACKAGE_BYTES).unwrap_or(u64::MAX) {
        return Err(format!(
            "GitHub Release asset `{expected_asset}` is {size} bytes, exceeding the {MAX_PLUGIN_PACKAGE_BYTES} byte package limit"
        )
        .into());
    }
    let download_directory = TemporaryDirectory::create("bkmqb-release-verify")?;
    let result = async_command_status_with_timeout(
        "gh",
        path,
        &[
            "release",
            "download",
            tag,
            "--repo",
            repository,
            "--pattern",
            expected_asset,
            "--dir",
            download_directory
                .path()
                .to_str()
                .ok_or("temporary download path is not valid UTF-8")?,
        ],
        MUTATION_COMMAND_TIMEOUT,
    )
    .await;
    let downloaded = download_directory.path().join(expected_asset);
    match result {
        Ok(()) if downloaded.is_file() => {
            let size = downloaded.metadata()?.len();
            Ok(Some(DownloadedReleaseAsset {
                _directory: download_directory,
                path: downloaded,
                size,
            }))
        }
        Ok(()) => Err(format!(
            "GitHub Release download completed without expected asset `{expected_asset}`"
        )
        .into()),
        Err(error) => Err(error),
    }
}

async fn github_release_asset_size(
    path: &Path,
    repository: &str,
    tag: &str,
    expected_asset: &str,
) -> Result<Option<u64>, Box<dyn std::error::Error>> {
    let output = async_command_output(
        "gh",
        path,
        &[
            "release",
            "view",
            tag,
            "--repo",
            repository,
            "--json",
            "assets",
            "--jq",
            ".assets[] | [.name, .size] | @tsv",
        ],
    )
    .await?;
    if output.status.success() {
        let stdout = String::from_utf8(output.stdout)?;
        for line in stdout.lines() {
            let Some((name, size)) = line.split_once('\t') else {
                continue;
            };
            if name == expected_asset {
                return Ok(Some(size.parse()?));
            }
        }
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("release not found") || stderr.contains("HTTP 404") {
        Ok(None)
    } else {
        Err(command_failure("gh release view", &output).into())
    }
}

async fn submit_marketplace_issue(
    path: &Path,
    manifest: &PluginManifest,
    repository: &str,
    tag: &str,
    release_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let title = format!("Publish {} {tag}", manifest.id);
    let body = format!(
        "Plugin ID: `{}`\nSource repository: https://github.com/{repository}\nRelease: {release_url}\nTag: `{tag}`\n",
        manifest.id
    );
    if gh_is_authenticated(path).await {
        let publisher = github_publisher_identity(path, repository).await?;
        let marketplace_repository = format!("github.com/{MARKETPLACE_REPOSITORY}");
        if marketplace_submission_exists(path, &marketplace_repository, &title, &body, &publisher)
            .await?
        {
            return Ok(());
        }
        let create_result = async_command_output_with_timeout(
            "gh",
            path,
            &[
                "issue",
                "create",
                "--repo",
                &marketplace_repository,
                "--title",
                &title,
                "--body",
                &body,
            ],
            MUTATION_COMMAND_TIMEOUT,
        )
        .await;
        match create_result {
            Ok(output) if output.status.success() => {
                let created_issue_url = String::from_utf8(output.stdout)?
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .ok_or("gh issue create succeeded without returning the created issue URL")?
                    .trim()
                    .to_owned();
                return verify_created_marketplace_issue(
                    path,
                    &marketplace_repository,
                    &title,
                    &body,
                    &created_issue_url,
                    &publisher,
                )
                .await;
            }
            result => {
                let error = match result {
                    Ok(output) => command_failure("gh issue create", &output),
                    Err(error) => error.to_string(),
                };
                match wait_for_marketplace_issue_state(
                    path,
                    &marketplace_repository,
                    &title,
                    &body,
                    &publisher,
                )
                .await
                {
                    Ok(MarketplaceIssueState::Matching) => {
                        println!("Marketplace submission exists despite the interrupted result.");
                        return Ok(());
                    }
                    Ok(MarketplaceIssueState::Missing) => {
                        return Err(format!(
                            "Marketplace submission result is unknown: {error}. No matching issue became visible within {} seconds; check {MARKETPLACE_REPOSITORY} before retrying.",
                            RECONCILIATION_TIMEOUT.as_secs()
                        ).into());
                    }
                    Ok(MarketplaceIssueState::Conflict(details)) => return Err(format!(
                        "marketplace submission result conflicts with an existing issue: {details}"
                    ).into()),
                    Ok(MarketplaceIssueState::Duplicate(details)) => {
                        return Err(format!(
                            "marketplace submission result contains duplicates: {details}"
                        )
                        .into());
                    }
                    Err(reconcile_error) => return Err(format!(
                        "marketplace submission result is unknown: {error}; verification also failed: {reconcile_error}. Check the marketplace before retrying."
                    ).into()),
                }
            }
        }
    }
    print_marketplace_submission_command(manifest, repository, tag, release_url);
    Ok(())
}

async fn marketplace_submission_exists(
    path: &Path,
    repository: &str,
    title: &str,
    body: &str,
    publisher: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    match marketplace_issue_state(path, repository, title, body, publisher).await {
        Ok(MarketplaceIssueState::Matching) => {
            println!("Marketplace submission already exists in {MARKETPLACE_REPOSITORY}.");
            Ok(true)
        }
        Ok(MarketplaceIssueState::Conflict(details)) => Err(format!(
            "marketplace submission conflicts with an existing issue: {details}"
        )
        .into()),
        Ok(MarketplaceIssueState::Duplicate(details)) => {
            Err(format!("duplicate marketplace submissions already exist: {details}").into())
        }
        Ok(MarketplaceIssueState::Missing) => Ok(false),
        Err(error) => Err(format!("marketplace submission preflight failed: {error}").into()),
    }
}

async fn github_publisher_identity(
    path: &Path,
    source_repository: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let login_output = async_command_output(
        "gh",
        path,
        &["api", "--hostname", "github.com", "user", "--jq", ".login"],
    )
    .await?;
    if !login_output.status.success() {
        return Err(command_failure("gh api user", &login_output).into());
    }
    let login = String::from_utf8(login_output.stdout)?.trim().to_owned();
    if login.is_empty() {
        return Err("GitHub API returned an empty authenticated login".into());
    }
    let endpoint = format!("repos/{source_repository}/collaborators/{login}/permission");
    let permission_output = async_command_output(
        "gh",
        path,
        &[
            "api",
            "--hostname",
            "github.com",
            &endpoint,
            "--jq",
            ".permission",
        ],
    )
    .await?;
    if !permission_output.status.success() {
        return Err(command_failure("gh api repository permission", &permission_output).into());
    }
    let permission = String::from_utf8(permission_output.stdout)?
        .trim()
        .to_ascii_lowercase();
    if !matches!(permission.as_str(), "admin" | "maintain" | "write") {
        return Err(format!(
            "GitHub user `{login}` does not have publish permission on `{source_repository}`"
        )
        .into());
    }
    Ok(login)
}

async fn verify_created_marketplace_issue(
    path: &Path,
    repository: &str,
    title: &str,
    body: &str,
    created_issue_url: &str,
    expected_author: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match wait_for_marketplace_issue_state(path, repository, title, body, expected_author).await? {
        MarketplaceIssueState::Matching => {
            println!("Marketplace submission created in {MARKETPLACE_REPOSITORY}.");
            Ok(())
        }
        MarketplaceIssueState::Duplicate(details) => {
            if close_newer_duplicate_issue(path, repository, created_issue_url, &details).await? {
                Err(format!(
                    "concurrent marketplace submissions were detected; this invocation's newer duplicate issue was closed, canonical issue: {}",
                    details.canonical_url().unwrap_or("unknown")
                )
                .into())
            } else {
                Err(format!(
                    "concurrent marketplace submissions were detected; this invocation created the canonical issue, but another duplicate still needs reconciliation: {details}"
                )
                .into())
            }
        }
        MarketplaceIssueState::Conflict(details) => Err(format!(
            "marketplace submission was created, but conflicting issue state was detected: {details}"
        )
        .into()),
        MarketplaceIssueState::Missing => Err(format!(
            "marketplace submission command succeeded, but no matching issue became visible within {} seconds; check {MARKETPLACE_REPOSITORY} before retrying",
            RECONCILIATION_TIMEOUT.as_secs()
        )
        .into()),
    }
}

async fn close_newer_duplicate_issue(
    path: &Path,
    repository: &str,
    created_issue_url: &str,
    duplicates: &MarketplaceDuplicates,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(created_number) = issue_number(created_issue_url) else {
        return Err(
            "could not identify the created marketplace issue number for duplicate rollback".into(),
        );
    };
    let Some(canonical_number) = duplicates.canonical_number() else {
        return Err("could not identify a canonical marketplace issue number".into());
    };
    if created_number == canonical_number {
        return Ok(false);
    }
    let created_number = created_number.to_string();
    async_command_status_with_timeout(
        "gh",
        path,
        &["issue", "close", &created_number, "--repo", repository],
        MUTATION_COMMAND_TIMEOUT,
    )
    .await?;
    Ok(true)
}

fn issue_number(url: &str) -> Option<u64> {
    url.trim_end_matches('/').rsplit('/').next()?.parse().ok()
}

async fn wait_for_marketplace_issue_state(
    path: &Path,
    repository: &str,
    title: &str,
    expected_body: &str,
    expected_author: &str,
) -> Result<MarketplaceIssueState, Box<dyn std::error::Error>> {
    let mut last_error = None;
    let poll = async {
        let mut delay = Duration::from_secs(2);
        loop {
            match marketplace_issue_state(path, repository, title, expected_body, expected_author)
                .await
            {
                Ok(MarketplaceIssueState::Missing) => sleep_with_backoff(&mut delay).await,
                Ok(state) => return Ok(state),
                Err(error) => {
                    last_error = Some(error.to_string());
                    sleep_with_backoff(&mut delay).await;
                }
            }
        }
    };
    match timeout(RECONCILIATION_TIMEOUT, poll).await {
        Ok(result) => result,
        Err(_) => match last_error {
            Some(error) => Err(format!(
                "marketplace issue state could not be confirmed before the reconciliation deadline; last query error: {error}"
            )
            .into()),
            None => Ok(MarketplaceIssueState::Missing),
        },
    }
}

async fn sleep_with_backoff(delay: &mut Duration) {
    sleep(*delay).await;
    *delay = delay
        .checked_mul(2)
        .unwrap_or(Duration::from_secs(30))
        .min(Duration::from_secs(30));
}

enum MarketplaceIssueState {
    Missing,
    Matching,
    Duplicate(MarketplaceDuplicates),
    Conflict(String),
}

struct MarketplaceDuplicates(Vec<String>);

impl MarketplaceDuplicates {
    fn canonical_number(&self) -> Option<u64> {
        self.0.iter().filter_map(|url| issue_number(url)).min()
    }

    fn canonical_url(&self) -> Option<&str> {
        let canonical = self.canonical_number()?;
        self.0
            .iter()
            .find(|url| issue_number(url) == Some(canonical))
            .map(String::as_str)
    }
}

impl std::fmt::Display for MarketplaceDuplicates {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0.join(", "))
    }
}

async fn marketplace_issue_state(
    path: &Path,
    repository: &str,
    title: &str,
    expected_body: &str,
    expected_author: &str,
) -> Result<MarketplaceIssueState, Box<dyn std::error::Error>> {
    let repository_path = repository
        .strip_prefix("github.com/")
        .ok_or("marketplace repository must use the github.com/owner/name form")?;
    let endpoint = format!("repos/{repository_path}/issues?state=all&per_page=100");
    let output = async_command_output(
        "gh",
        path,
        &[
            "api",
            "--hostname",
            "github.com",
            "--paginate",
            &endpoint,
            "--jq",
            ".[] | @json",
        ],
    )
    .await?;
    if !output.status.success() {
        return Err(command_failure("gh api --paginate", &output).into());
    }
    let issues = parse_issue_json_lines(&output.stdout)?;
    classify_marketplace_issues(&issues, title, expected_body, expected_author)
}

fn parse_issue_json_lines(output: &[u8]) -> Result<Value, Box<dyn std::error::Error>> {
    let stdout = std::str::from_utf8(output)?;
    Ok(Value::Array(
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn classify_marketplace_issues(
    issues: &Value,
    title: &str,
    expected_body: &str,
    expected_author: &str,
) -> Result<MarketplaceIssueState, Box<dyn std::error::Error>> {
    let mut matching = Vec::new();
    let mut conflict = None;
    for issue in issues
        .as_array()
        .ok_or("GitHub issue response is not an array")?
    {
        if issue.get("pull_request").is_some() {
            continue;
        }
        if issue
            .get("user")
            .and_then(|user| user.get("login"))
            .and_then(Value::as_str)
            != Some(expected_author)
        {
            continue;
        }
        if issue.get("title").and_then(Value::as_str) != Some(title) {
            continue;
        }
        let body = issue
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let state = issue
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let url = issue
            .get("html_url")
            .or_else(|| issue.get("url"))
            .and_then(Value::as_str)
            .unwrap_or("unknown URL");
        if body == expected_body && state.eq_ignore_ascii_case("open") {
            matching.push(url.to_owned());
            continue;
        }
        conflict = Some(format!(
            "{url} has state `{state}` or different submission metadata"
        ));
    }
    if let Some(details) = conflict {
        return Ok(MarketplaceIssueState::Conflict(details));
    }
    if matching.len() > 1 {
        return Ok(MarketplaceIssueState::Duplicate(MarketplaceDuplicates(
            matching,
        )));
    }
    if matching.len() == 1 {
        return Ok(MarketplaceIssueState::Matching);
    }
    Ok(MarketplaceIssueState::Missing)
}

fn print_marketplace_submission_command(
    manifest: &PluginManifest,
    repository: &str,
    tag: &str,
    release_url: &str,
) {
    let title = format!("Publish {} {tag}", manifest.id);
    let body = format!(
        "Plugin ID: `{}`\nSource repository: https://github.com/{repository}\nRelease: {release_url}\nTag: `{tag}`\n",
        manifest.id
    );
    println!("Submit the Release to the marketplace with:");
    println!(
        "  gh issue create --repo github.com/{MARKETPLACE_REPOSITORY} --title {} --body {}",
        shell_quote(&title),
        shell_quote(&body)
    );
}

fn print_manual_publish_guide(
    path: &Path,
    manifest: &PluginManifest,
    tag: &str,
    remote: &str,
    package: &Path,
    source_commit: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let digest = sha256_file(package)?;
    let absolute_package = absolute_path(package)?;
    println!("\nManual publishing guide:");
    println!("  Package: {}", absolute_package.display());
    println!("  SHA-256: {digest}");
    println!("  Source commit: {source_commit}");
    println!(
        "\n1. Create and push the release tag for the exact commit used to build the package:"
    );
    println!(
        "  git tag -a {tag} -m {} {source_commit}",
        shell_quote(&format!("Release {tag}"))
    );
    println!("  git push {} refs/tags/{tag}", shell_quote(remote));
    println!("2. Create a GitHub Release and upload:");
    let github_repository = git_output(path, &["remote", "get-url", "--push", remote])
        .ok()
        .and_then(|remote_url| github_repository(&remote_url));
    if let Some(repository) = &github_repository {
        println!(
            "  gh release create {tag} {} --repo github.com/{repository} --verify-tag --generate-notes",
            shell_display_path(&absolute_package)
        );
    } else {
        println!(
            "  gh release create {tag} {} --verify-tag --generate-notes",
            shell_display_path(&absolute_package)
        );
    }
    println!(
        "3. Submit `{}` {tag} to {MARKETPLACE_REPOSITORY}.",
        manifest.id
    );
    if let Some(repository) = github_repository {
        println!("  Release URL: https://github.com/{repository}/releases/tag/{tag}");
    }
    Ok(())
}

fn check_project(path: &Path) -> Result<PluginManifest, Box<dyn std::error::Error>> {
    let manifest_path = path.join("plugin.toml");
    if !manifest_path.is_file() {
        return Err(format!(
            "`{}` is not a plugin project: `plugin.toml` was not found. Create one with `bkmqb plugin new <directory>`, enter that directory, or pass it to this command as `bkmqb plugin build <directory>`.",
            path.display()
        )
        .into());
    }
    let manifest_source = fs::read_to_string(&manifest_path).map_err(|error| {
        format!(
            "failed to read plugin manifest `{}`: {error}",
            manifest_path.display()
        )
    })?;
    let manifest = PluginManifest::from_toml(&manifest_source).map_err(|error| {
        format!(
            "plugin manifest `{}` is invalid: {error}",
            manifest_path.display()
        )
    })?;
    for required in ["Cargo.toml", "src/lib.rs", "wit/bkm-plugin.wit"] {
        if !path.join(required).is_file() {
            return Err(format!("plugin project is missing `{required}`").into());
        }
    }
    println!(
        "Plugin project is valid: {} {}",
        manifest.id, manifest.version
    );
    Ok(manifest)
}

async fn build_project(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest = check_project(path)?;
    ensure_wasm_target()?;
    let artifact_name = library_artifact_name(path)?;
    let target_directory = path.join("target");
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .arg("--target-dir")
        .arg(&target_directory)
        .current_dir(path)
        .status()?;
    if !status.success() {
        return Err("Cargo failed to build the plugin for wasm32-unknown-unknown".into());
    }
    let core_wasm = target_directory.join(format!(
        "wasm32-unknown-unknown/release/{}.wasm",
        artifact_name.replace('-', "_")
    ));
    if !core_wasm.is_file() {
        return Err(format!("Cargo output `{}` was not found", core_wasm.display()).into());
    }
    let output_directory = path.join("target/bkmqb");
    fs::create_dir_all(&output_directory)?;
    let component_path = output_directory.join("component.wasm");
    let component = ComponentEncoder::default()
        .module(&fs::read(&core_wasm)?)?
        .validate(true)
        .encode()?;
    fs::write(&component_path, component)?;
    let package_path = default_package_path(&output_directory, &manifest);
    write_package(path, &component_path, &package_path).await?;
    println!("Built plugin package `{}`", package_path.display());
    Ok(package_path)
}

fn ensure_wasm_target() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("rustc")
        .args([
            "--print",
            "target-libdir",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .output();
    let installed = output.is_ok_and(|output| {
        output.status.success()
            && target_libdir_is_populated(String::from_utf8_lossy(&output.stdout).trim())
    });
    if installed {
        return Ok(());
    }
    Err("Rust target `wasm32-unknown-unknown` is not installed. Install it once with `rustup target add wasm32-unknown-unknown`, then rerun `bkmqb plugin build`.".into())
}

fn target_libdir_is_populated(path: &str) -> bool {
    !path.is_empty()
        && fs::read_dir(path)
            .ok()
            .and_then(|mut entries| entries.next())
            .is_some()
}

async fn package_project(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest = check_project(path)?;
    let component_path = path.join("target/bkmqb/component.wasm");
    if !component_path.is_file() {
        return Err("component.wasm is missing; run `bkmqb plugin build` first".into());
    }
    let package_path = default_package_path(&path.join("target/bkmqb"), &manifest);
    write_package(path, &component_path, &package_path).await?;
    println!("Packaged plugin `{}`", package_path.display());
    Ok(package_path)
}

async fn validate_built_package(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let package = ValidatedPluginPackage::from_path(path)?;
    WasmPlugin::from_package(package).await?;
    Ok(())
}

async fn write_package(
    project: &Path,
    component_path: &Path,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(metadata) = fs::symlink_metadata(output) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "plugin package destination `{}` must be a regular file",
                output.display()
            )
            .into());
        }
    }
    let directory = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)?;
    let mut temporary = None;
    for nonce in 0..100_u32 {
        let candidate = directory.join(format!(
            ".bkmqb-package-{}-{}-{nonce}.tmp.bkm-plugin",
            std::process::id(),
            timestamp_nanos()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let Some((temporary_path, file)) = temporary else {
        return Err("could not allocate a temporary plugin package path".into());
    };
    let result = write_package_archive(project, component_path, file);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = validate_built_package(&temporary_path).await {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = replace_package_file(&temporary_path, output) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error.into());
    }
    Ok(())
}

fn write_package_archive(
    project: &Path,
    component_path: &Path,
    file: fs::File,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    add_file(
        &mut archive,
        &project.join("plugin.toml"),
        "plugin.toml",
        options,
    )?;
    add_file(&mut archive, component_path, "component.wasm", options)?;
    for optional in ["config.schema.json", "README.md", "LICENSE"] {
        let source = project.join(optional);
        if source.is_file() {
            add_file(&mut archive, &source, optional, options)?;
        }
    }
    for directory in ["assets", "migrations"] {
        let source = project.join(directory);
        if source.exists() {
            let metadata = fs::symlink_metadata(&source)?;
            if metadata.file_type().is_symlink()
                || is_windows_reparse_point(&metadata)
                || !metadata.is_dir()
            {
                return Err(format!(
                    "package input `{}` must be a regular directory",
                    source.display()
                )
                .into());
            }
            add_directory_files(&mut archive, &source, directory, options)?;
        }
    }
    archive.finish()?.sync_all()?;
    Ok(())
}

fn add_directory_files(
    archive: &mut ZipWriter<fs::File>,
    directory: &Path,
    archive_root: &str,
    options: SimpleFileOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry.file_type()?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if file_type.is_symlink() || is_windows_reparse_point(&metadata) {
            return Err(format!(
                "package input `{}` is a symbolic link",
                entry.path().display()
            )
            .into());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "package paths must be UTF-8")?;
        let archive_path = format!("{archive_root}/{name}");
        if file_type.is_dir() {
            add_directory_files(archive, &entry.path(), &archive_path, options)?;
        } else if file_type.is_file() {
            add_file(archive, &entry.path(), &archive_path, options)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn add_file(
    archive: &mut ZipWriter<fs::File>,
    source: &Path,
    archive_path: &str,
    options: SimpleFileOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = open_regular_file_without_following_symlinks(source)?;
    archive.start_file(archive_path, options)?;
    std::io::copy(&mut input, archive)?;
    Ok(())
}

fn open_regular_file_without_following_symlinks(path: &Path) -> Result<fs::File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(_)
            if fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink()) =>
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("package input `{}` must be a regular file", path.display()),
            ));
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || is_windows_reparse_point(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("package input `{}` must be a regular file", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(not(windows))]
fn replace_package_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_package_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    if !destination.exists() {
        return fs::rename(source, destination);
    }
    let backup = destination.with_extension(format!(
        "bkmqb-backup-{}-{}",
        std::process::id(),
        timestamp_nanos()
    ));
    fs::rename(destination, &backup)?;
    if let Err(error) = fs::rename(source, destination) {
        let _ = fs::rename(&backup, destination);
        return Err(error);
    }
    fs::remove_file(backup)
}

fn library_artifact_name(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let cargo: toml::Value = toml::from_str(&fs::read_to_string(path.join("Cargo.toml"))?)?;
    if let Some(name) = cargo
        .get("lib")
        .and_then(|library| library.get("name"))
        .and_then(toml::Value::as_str)
    {
        return Ok(name.to_owned());
    }
    cargo
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "Cargo.toml is missing package.name".into())
}

fn default_package_path(directory: &Path, manifest: &PluginManifest) -> PathBuf {
    directory.join(format!("{}-{}.bkm-plugin", manifest.id, manifest.version))
}

fn validate_generated_values(
    plugin_id: &str,
    class_name: &str,
    locale: &str,
    command: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    plugin_api::PluginId::new(plugin_id)?;
    if !is_rust_identifier(class_name) {
        return Err("class name must be a valid Rust identifier".into());
    }
    if command.is_empty()
        || !command
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("command must contain lowercase ASCII letters, digits, or hyphens".into());
    }
    let probe = manifest_template(plugin_id, locale, "Plugin", "", command);
    PluginManifest::from_toml(&probe)?;
    Ok(())
}

fn is_rust_identifier(value: &str) -> bool {
    if value == "_" {
        return false;
    }
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
        && !matches!(
            value,
            "abstract"
                | "as"
                | "async"
                | "await"
                | "become"
                | "box"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "do"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "final"
                | "fn"
                | "for"
                | "gen"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "macro"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "override"
                | "priv"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "true"
                | "try"
                | "type"
                | "typeof"
                | "union"
                | "unsafe"
                | "unsized"
                | "use"
                | "virtual"
                | "where"
                | "while"
                | "yield"
        )
}

fn timestamp_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn project_slug(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("project directory must end in a UTF-8 name")?;
    let slug = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    if slug.is_empty() {
        return Err("project directory must contain an ASCII letter or digit".into());
    }
    Ok(slug)
}

fn title_case(slug: &str) -> String {
    slug.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut characters = part.chars();
            characters.next().map_or_else(String::new, |first| {
                format!("{}{}", first.to_ascii_uppercase(), characters.as_str())
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn rust_type_name(slug: &str) -> String {
    title_case(slug).replace(' ', "")
}

fn value_or_prompt(
    value: Option<String>,
    interactive: bool,
    label: &str,
    default: &str,
) -> Result<String, dialoguer::Error> {
    if let Some(value) = value {
        Ok(value)
    } else if interactive {
        prompt(label, default)
    } else {
        Ok(default.to_owned())
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    path.canonicalize()
}

fn shell_display_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[derive(Debug)]
struct ContextualError {
    context: String,
    source: Box<dyn std::error::Error>,
}

impl std::fmt::Display for ContextualError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.context, self.source)
    }
}

impl std::error::Error for ContextualError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn contextual_error(
    context: impl Into<String>,
    source: Box<dyn std::error::Error>,
) -> Box<dyn std::error::Error> {
    Box::new(ContextualError {
        context: context.into(),
        source,
    })
}

fn command_failure(command: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("`{command}` exited with {}", output.status)
    } else {
        format!("`{command}` failed: {stderr}")
    }
}

fn command_status(
    program: &str,
    arguments: &[&str],
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(path)
        .output()
        .map_err(|error| format!("could not run `{program}`: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure(&format!("{program} {}", arguments.join(" ")), &output).into())
    }
}

async fn async_command_output(
    program: &str,
    path: &Path,
    arguments: &[&str],
) -> Result<Output, Box<dyn std::error::Error>> {
    async_command_output_with_timeout(program, path, arguments, READ_COMMAND_TIMEOUT).await
}

async fn async_git_output(
    path: &Path,
    arguments: &[&str],
) -> Result<String, Box<dyn std::error::Error>> {
    let output = async_command_output("git", path, arguments).await?;
    if !output.status.success() {
        return Err(command_failure(&format!("git {}", arguments.join(" ")), &output).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn async_git_output_lines(
    path: &Path,
    arguments: &[&str],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(async_git_output(path, arguments)
        .await?
        .lines()
        .map(str::to_owned)
        .collect())
}

async fn async_command_output_with_timeout(
    program: &str,
    path: &Path,
    arguments: &[&str],
    command_timeout: Duration,
) -> Result<Output, Box<dyn std::error::Error>> {
    let mut command = TokioCommand::new(program);
    command.args(arguments).current_dir(path).kill_on_drop(true);
    timeout(command_timeout, command.output())
        .await
        .map_err(|_| {
            format!(
                "`{program}` exceeded the {} second network command timeout",
                command_timeout.as_secs()
            )
        })?
        .map_err(|error| format!("could not run `{program}`: {error}").into())
}

async fn async_command_status_with_timeout(
    program: &str,
    path: &Path,
    arguments: &[&str],
    command_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let output =
        async_command_output_with_timeout(program, path, arguments, command_timeout).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure(&format!("{program} {}", arguments.join(" ")), &output).into())
    }
}

fn git_status(path: &Path, arguments: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    command_status("git", arguments, path)
}

fn git_output(path: &Path, arguments: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(path)
        .output()
        .map_err(|error| format!("could not run `git`: {error}"))?;
    if !output.status.success() {
        return Err(command_failure(&format!("git {}", arguments.join(" ")), &output).into());
    }
    let mut output = String::from_utf8(output.stdout)?;
    if output.ends_with('\n') {
        output.pop();
        if output.ends_with('\r') {
            output.pop();
        }
    }
    Ok(output)
}

fn git_output_lines(
    path: &Path,
    arguments: &[&str],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(git_output(path, arguments)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn git_command_succeeds(path: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .args(arguments)
        .current_dir(path)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn github_repository(remote: &str) -> Option<String> {
    if remote_url_has_credentials(remote) {
        return None;
    }
    if let Some(repository) = remote.strip_prefix("git@github.com:") {
        if repository.contains(['?', '#']) || repository.chars().any(char::is_control) {
            return None;
        }
        return github_repository_path(repository);
    }
    let url = Url::parse(remote).ok()?;
    let supported = url.scheme() == "https"
        || (url.scheme() == "ssh" && url.username() == "git" && url.password().is_none());
    let default_port = match url.scheme() {
        "https" => 443,
        "ssh" => 22,
        _ => return None,
    };
    if !supported
        || !url.host_str()?.eq_ignore_ascii_case("github.com")
        || url.port().is_some_and(|port| port != default_port)
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    github_repository_path(url.path().trim_start_matches('/'))
}

fn github_repository_path(repository: &str) -> Option<String> {
    let repository = repository.trim_end_matches('/');
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    let mut segments = repository.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    let valid_owner = owner
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    let valid_name = name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if owner.is_empty()
        || name.is_empty()
        || !valid_owner
        || !valid_name
        || segments.next().is_some()
    {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

fn remote_url_has_credentials(remote: &str) -> bool {
    if let Ok(url) = Url::parse(remote) {
        if matches!(url.scheme(), "http" | "https" | "ssh") {
            return url.password().is_some()
                || match url.scheme() {
                    "ssh" => !url.username().is_empty() && url.username() != "git",
                    _ => !url.username().is_empty(),
                };
        }
    }
    if let Some((user, _)) = scp_remote_parts(remote) {
        return user != "git";
    }
    http_authority(remote).is_some_and(|authority| authority.contains('@'))
}

fn safe_remote_display(remote: &str) -> String {
    let suffix_start = remote
        .find(|character: char| matches!(character, '?' | '#') || character.is_control())
        .unwrap_or(remote.len());
    let remote = &remote[..suffix_start];
    let Some(scheme_end) = remote.find("://") else {
        if let Some((user, host_and_path)) = scp_remote_parts(remote) {
            if user != "git" {
                return format!("***@{host_and_path}");
            }
        }
        return remote.to_owned();
    };
    let authority_start = scheme_end + 3;
    let authority_end = remote[authority_start..]
        .find(['/', '?', '#'])
        .map_or(remote.len(), |offset| authority_start + offset);
    let authority = &remote[authority_start..authority_end];
    if let Some((_, host)) = authority.rsplit_once('@') {
        return format!(
            "{}***@{}{}",
            &remote[..authority_start],
            host,
            &remote[authority_end..]
        );
    }
    remote.to_owned()
}

fn scp_remote_parts(remote: &str) -> Option<(&str, &str)> {
    let (user, host_and_path) = remote.split_once('@')?;
    (!user.is_empty() && host_and_path.contains(':')).then_some((user, host_and_path))
}

fn http_authority(remote: &str) -> Option<&str> {
    let scheme_end = remote.find("://")?;
    if !matches_ignore_ascii_case(&remote[..scheme_end], &["http", "https"]) {
        return None;
    }
    let rest = &remote[scheme_end + 3..];
    Some(rest.split(['/', '?', '#']).next().unwrap_or(rest))
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn sha256_file(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create(prefix: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let base = std::env::temp_dir();
        for attempt in 0..100_u32 {
            let candidate = base.join(format!(
                "{prefix}-{}-{}-{attempt}",
                std::process::id(),
                timestamp_nanos()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => return Ok(Self(candidate)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err("could not allocate a temporary directory".into())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ReleaseWorktree {
    repository: PathBuf,
    checkout: PathBuf,
    _temporary_directory: TemporaryDirectory,
}

impl ReleaseWorktree {
    fn create(repository: &Path, source_commit: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let temporary_directory = TemporaryDirectory::create("bkmqb-release-worktree")?;
        let checkout = temporary_directory.path().join("checkout");
        git_status(
            repository,
            &[
                "worktree",
                "add",
                "--detach",
                checkout
                    .to_str()
                    .ok_or("temporary worktree path is not valid UTF-8")?,
                source_commit,
            ],
        )?;
        Ok(Self {
            repository: repository.to_path_buf(),
            checkout,
            _temporary_directory: temporary_directory,
        })
    }

    fn path(&self) -> &Path {
        &self.checkout
    }
}

impl Drop for ReleaseWorktree {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&self.checkout)
            .current_dir(&self.repository)
            .output();
    }
}

fn prompt(label: &str, default: &str) -> Result<String, dialoguer::Error> {
    Input::<String>::new()
        .with_prompt(label)
        .default(default.to_owned())
        .allow_empty(true)
        .interact_text()
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

fn manifest_template(
    plugin_id: &str,
    locale: &str,
    display_name: &str,
    description: &str,
    command: &str,
) -> String {
    format!(
        "manifest_version = 1\nid = {}\nversion = \"0.1.0\"\nprotocol = \">=1.1,<2.0\"\nentry = \"component.wasm\"\n\n[metadata]\ndefault_locale = {}\n\n[metadata.locales.{}]\nname = {}\ndescription = {}\n\n[[subscriptions]]\nid = \"message-handler\"\nevent = \"message.created\"\npriority = 0\nscopes = [\"group\", \"private\", \"channel\"]\n\n[[commands]]\nname = {}\naliases = []\ndescription = {}\n\n[permissions]\nactions = [\"message.reply\"]\n",
        toml_string(plugin_id),
        toml_string(locale),
        toml_string(locale),
        toml_string(display_name),
        toml_string(description),
        toml_string(command),
        toml_string(description),
    )
}

fn cargo_template(crate_name: &str, library_name: &str) -> String {
    format!(
        "[package]\nname = {}\nversion = \"0.1.0\"\nedition = \"2024\"\nrust-version = \"1.85\"\npublish = false\n\n[lib]\nname = {}\ncrate-type = [\"cdylib\"]\n\n[dependencies]\nwit-bindgen = \"0.46\"\n\n[workspace]\n",
        toml_string(crate_name),
        toml_string(library_name),
    )
}

fn source_template(class_name: &str, command: &str) -> String {
    include_str!("plugin_template.rs.txt")
        .replace("__CLASS_NAME__", class_name)
        .replace("__COMMAND__", command)
}

fn readme_template(display_name: &str, command: &str) -> String {
    format!(
        "# {display_name}\n\nGenerated BPP 1.2 plugin.\n\n```bash\nbkmqb plugin check\nbkmqb plugin build\nbkmqb plugin inspect target/bkmqb/*.bkm-plugin\nbkmqb plugin install target/bkmqb/*.bkm-plugin\nbkmqb plugin publish\n```\n\nThe starter command is `/{command}`.\n"
    )
}

fn release_workflow_template(
    revision: Option<&str>,
    source_repository: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let revision = revision.ok_or(
        "cannot generate the GitHub Actions workflow because this bkmqb build has no source revision; install bkmqb from the official repository or set BKMQB_GIT_REV when building",
    )?;
    let source_repository = source_repository.ok_or(
        "cannot generate the GitHub Actions workflow because this bkmqb build has no source repository provenance",
    )?;
    Ok(r#"name: Release plugin

on:
  push:
    tags:
      - "v*.*.*"

permissions:
  contents: write

jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4
        with:
          persist-credentials: false

      - name: Detect local publishing mode
        id: release-mode
        shell: bash
        run: |
          if git for-each-ref --format='%(contents)' "refs/tags/$GITHUB_REF_NAME" | grep -Fq '[bkmqb-local-release]'; then
            echo "local=true" >> "$GITHUB_OUTPUT"
          else
            echo "local=false" >> "$GITHUB_OUTPUT"
          fi

      - name: Install Rust
        if: steps.release-mode.outputs.local != 'true'
        uses: dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4 # stable
        with:
          targets: wasm32-unknown-unknown

      - name: Install bkmqb
        if: steps.release-mode.outputs.local != 'true'
        run: cargo install --locked --git __BKMQB_GIT_REPOSITORY__ --rev __BKMQB_GIT_REV__ --bin bkmqb

      - name: Verify release version
        if: steps.release-mode.outputs.local != 'true'
        shell: python
        run: |
          import os
          import tomllib

          with open("plugin.toml", "rb") as manifest_file:
              manifest = tomllib.load(manifest_file)
          expected = f"v{manifest['version']}"
          actual = os.environ["GITHUB_REF_NAME"]
          if actual != expected:
              raise SystemExit(f"release tag {actual!r} does not match manifest version {expected!r}")

      - name: Build plugin package
        if: steps.release-mode.outputs.local != 'true'
        run: bkmqb plugin build

      - name: Create GitHub Release
        if: steps.release-mode.outputs.local != 'true'
        env:
          GH_TOKEN: ${{ github.token }}
          GH_HOST: github.com
        run: gh release create "$GITHUB_REF_NAME" target/bkmqb/*.bkm-plugin --verify-tag --generate-notes --title "$GITHUB_REF_NAME"
"#
    .replace("__BKMQB_GIT_REV__", revision)
    .replace("__BKMQB_GIT_REPOSITORY__", source_repository))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_GIT_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
    const TEST_GIT_REPOSITORY: &str = "https://github.com/example/bkmqb";

    fn create_test_project(options: NewOptions) -> Result<(), Box<dyn std::error::Error>> {
        create_project_with_revision(options, Some(TEST_GIT_REVISION), Some(TEST_GIT_REPOSITORY))
    }

    fn nonce() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn commit_test_project_with_remote(
        directory: &Path,
        remote: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        create_test_project(NewOptions {
            path: Some(directory.to_path_buf()),
            interactive: Some(false),
            initialize_git: true,
            ..NewOptions::default()
        })?;
        git_status(directory, &["config", "user.name", "BKM Test"])?;
        git_status(directory, &["config", "user.email", "test@example.invalid"])?;
        git_status(
            directory,
            &[
                "add",
                "Cargo.toml",
                "plugin.toml",
                "src/lib.rs",
                "wit/bkm-plugin.wit",
                "README.md",
                ".gitignore",
                ".github/workflows/release.yml",
            ],
        )?;
        git_status(directory, &["commit", "-m", "Initial plugin"])?;
        fs::create_dir_all(remote)?;
        git_status(remote, &["init", "--bare"])?;
        git_status(
            directory,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().ok_or("test remote path is not UTF-8")?,
            ],
        )?;
        Ok(())
    }

    #[test]
    fn non_interactive_scaffold_uses_defaults() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-new-{}-{}",
            std::process::id(),
            nonce()
        ));
        create_test_project(NewOptions {
            path: Some(directory.clone()),
            interactive: Some(false),
            ..NewOptions::default()
        })
        .unwrap();
        let manifest =
            PluginManifest::from_toml(&fs::read_to_string(directory.join("plugin.toml")).unwrap())
                .unwrap();
        assert_eq!(manifest.metadata.default_locale, "en");
        assert!(directory.join("src/lib.rs").is_file());
        assert!(directory.join(".github/workflows/release.yml").is_file());
        assert!(
            fs::read_to_string(directory.join("README.md"))
                .unwrap()
                .contains("Generated BPP 1.2 plugin")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn new_options_initialize_git_by_default_and_allow_opt_out() {
        assert!(NewOptions::parse(&[]).unwrap().initialize_git);
        assert!(
            !NewOptions::parse(&["--no-git".to_owned()])
                .unwrap()
                .initialize_git
        );
    }

    #[test]
    fn scaffold_can_initialize_main_git_repository() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-git-{}-{}",
            std::process::id(),
            nonce()
        ));
        create_test_project(NewOptions {
            path: Some(directory.clone()),
            interactive: Some(false),
            initialize_git: true,
            ..NewOptions::default()
        })
        .unwrap();
        assert!(directory.join(".git").is_dir());
        assert_eq!(
            git_output(&directory, &["branch", "--show-current"]).unwrap(),
            "main"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn release_tags_require_exact_stable_semver() {
        assert_eq!(
            validate_release_tag("v1.2.3").unwrap(),
            Version::new(1, 2, 3)
        );
        for invalid in ["1.2.3", "v1.2", "v01.2.3", "v1.2.3-rc.1", "latest"] {
            assert!(validate_release_tag(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn github_repository_supports_common_remote_formats() {
        for remote in [
            "git@github.com:author/weather.git",
            "https://github.com/author/weather.git",
            "https://github.com:443/author/weather.git",
            "ssh://git@github.com/author/weather.git",
            "ssh://git@github.com:22/author/weather.git",
        ] {
            assert_eq!(github_repository(remote).as_deref(), Some("author/weather"));
        }
        assert_eq!(
            github_repository("git@github.com:author/weather.git.git").as_deref(),
            Some("author/weather.git")
        );
        assert_eq!(
            github_repository("https://gitlab.com/author/weather.git"),
            None
        );
        assert_eq!(
            github_repository("https://github.com:1234/author/weather.git"),
            None
        );
        assert!(!remote_url_has_credentials(
            "ssh://git@github.com:22/author/weather.git"
        ));
        assert_eq!(
            github_repository("https://secret@github.com/author/weather.git"),
            None
        );
        assert_eq!(
            safe_remote_display("https://secret@github.com/author/weather.git"),
            "https://***@github.com/author/weather.git"
        );
        assert_eq!(
            safe_remote_display("HTTPS://secret@github.com/author/weather.git"),
            "HTTPS://***@github.com/author/weather.git"
        );
        assert!(remote_url_has_credentials(
            "HTTPS://secret@github.com/author/weather.git"
        ));
        assert!(remote_url_has_credentials(
            "secret@github.com:author/weather.git"
        ));
        assert_eq!(
            safe_remote_display("secret@github.com:author/weather.git"),
            "***@github.com:author/weather.git"
        );
        assert_eq!(
            safe_remote_display("git@github.com:author/weather.git"),
            "git@github.com:author/weather.git"
        );
        for remote in [
            "https://github.com/author/weather.git?token=secret",
            "https://github.com/author/weather.git#secret",
            "git@github.com:author/weather.git?token=secret",
            "git@github.com:author/weather.git#secret",
            "git@github.com:author/weather.git\nsecret",
        ] {
            assert_eq!(github_repository(remote), None);
            assert!(!safe_remote_display(remote).contains("secret"));
        }
    }

    #[test]
    fn publish_options_parse_non_interactive_dry_run() {
        let options = PublishOptions::parse(&[
            "plugin-dir".to_owned(),
            "--tag".to_owned(),
            "v2.3.4".to_owned(),
            "--method".to_owned(),
            "manual".to_owned(),
            "--remote".to_owned(),
            "upstream".to_owned(),
            "--no-interactive".to_owned(),
            "--dry-run".to_owned(),
            "--yes".to_owned(),
        ])
        .unwrap();
        assert_eq!(options.path.as_deref(), Some(Path::new("plugin-dir")));
        assert_eq!(options.tag.as_deref(), Some("v2.3.4"));
        assert_eq!(options.method, Some(PublishMethod::Manual));
        assert_eq!(options.remote.as_deref(), Some("upstream"));
        assert_eq!(options.interactive, Some(false));
        assert!(options.dry_run);
        assert!(options.yes);
        assert!(PublishOptions::parse(&["--remote".to_owned(), "--dry-run".to_owned()]).is_err());
        assert!(PublishOptions::parse(&["--remote".to_owned(), "-dangerous".to_owned()]).is_err());
        for invalid in [".", "origin.", "bad..name"] {
            assert!(validate_remote_name(invalid).is_err());
        }
    }

    #[test]
    fn paginated_issue_json_lines_support_gh_without_slurp() {
        let issues = parse_issue_json_lines(
            br#"{"title":"first"}
{"title":"second"}
"#,
        )
        .unwrap();
        assert_eq!(issues.as_array().unwrap().len(), 2);
        assert_eq!(issues[0]["title"], "first");
        assert_eq!(issues[1]["title"], "second");
    }

    #[test]
    fn marketplace_state_rejects_matching_issue_mixed_with_conflict() {
        let issues = serde_json::json!([
            {
                "title": "Publish weather v1.2.3",
                "body": "expected",
                "state": "OPEN",
                "url": "https://github.com/example/marketplace/issues/1",
                "user": {"login": "publisher"}
            },
            {
                "title": "Publish weather v1.2.3",
                "body": "different",
                "state": "OPEN",
                "url": "https://github.com/example/marketplace/issues/2",
                "user": {"login": "publisher"}
            }
        ]);
        assert!(matches!(
            classify_marketplace_issues(&issues, "Publish weather v1.2.3", "expected", "publisher")
                .unwrap(),
            MarketplaceIssueState::Conflict(_)
        ));
    }

    #[test]
    fn marketplace_state_ignores_pull_requests() {
        let issues = serde_json::json!([
            {
                "title": "Publish weather v1.2.3",
                "body": "expected",
                "state": "open",
                "html_url": "https://github.com/example/marketplace/pull/1",
                "user": {"login": "publisher"},
                "pull_request": {"url": "https://api.github.com/repos/example/marketplace/pulls/1"}
            }
        ]);
        assert!(matches!(
            classify_marketplace_issues(&issues, "Publish weather v1.2.3", "expected", "publisher")
                .unwrap(),
            MarketplaceIssueState::Missing
        ));
    }

    #[test]
    fn marketplace_state_ignores_untrusted_authors() {
        let issues = serde_json::json!([
            {
                "title": "Publish weather v1.2.3",
                "body": "expected",
                "state": "open",
                "html_url": "https://github.com/example/marketplace/issues/1",
                "user": {"login": "stranger"}
            }
        ]);
        assert!(matches!(
            classify_marketplace_issues(&issues, "Publish weather v1.2.3", "expected", "publisher")
                .unwrap(),
            MarketplaceIssueState::Missing
        ));
    }

    #[test]
    fn marketplace_duplicate_rollback_keeps_lowest_issue_number() {
        let duplicates = MarketplaceDuplicates(vec![
            "https://github.com/example/marketplace/issues/42".to_owned(),
            "https://github.com/example/marketplace/issues/7".to_owned(),
        ]);
        assert_eq!(duplicates.canonical_number(), Some(7));
        assert_eq!(
            duplicates.canonical_url(),
            Some("https://github.com/example/marketplace/issues/7")
        );
        assert_eq!(
            issue_number("https://github.com/example/marketplace/issues/42/"),
            Some(42)
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_output_rejects_symlinked_directory_and_destination() {
        use std::os::unix::fs::symlink;

        let repository = std::env::temp_dir().join(format!(
            "bkmqb-release-output-{}-{}",
            std::process::id(),
            nonce()
        ));
        let outside = repository.with_extension("outside");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, repository.join("target")).unwrap();
        assert!(
            ensure_safe_package_output_directory(&repository, &repository.join("target/bkmqb"))
                .is_err()
        );
        fs::remove_file(repository.join("target")).unwrap();
        fs::create_dir_all(repository.join("target/bkmqb")).unwrap();
        let source = repository.join("source.bkm-plugin");
        let destination = repository.join("target/bkmqb/plugin.bkm-plugin");
        let outside_file = outside.join("outside.bkm-plugin");
        fs::write(&source, b"package").unwrap();
        fs::write(&outside_file, b"outside").unwrap();
        symlink(&outside_file, &destination).unwrap();
        assert!(copy_package_atomically(&source, &destination).is_err());
        assert_eq!(fs::read(&outside_file).unwrap(), b"outside");
        fs::remove_dir_all(repository).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn release_workflow_builds_and_uploads_tagged_package() {
        let workflow =
            release_workflow_template(Some(TEST_GIT_REVISION), Some(TEST_GIT_REPOSITORY)).unwrap();
        let revision = TEST_GIT_REVISION;
        assert!(workflow.contains("tags:\n      - \"v*.*.*\""));
        assert!(workflow.contains("bkmqb plugin build"));
        assert!(workflow.contains("gh release create"));
        assert!(workflow.contains(LOCAL_RELEASE_TAG_MARKER));
        assert!(workflow.contains("steps.release-mode.outputs.local != 'true'"));
        assert!(workflow.contains("persist-credentials: false"));
        assert!(workflow.contains("tomllib.load"));
        assert!(workflow.contains("actions/checkout@11d5960a326750d5838078e36cf38b85af677262"));
        assert!(
            workflow.contains("dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4")
        );
        assert!(workflow.contains(&format!("--rev {revision} --bin bkmqb")));
        assert!(workflow.contains(&format!("--git {TEST_GIT_REPOSITORY}")));
        assert_eq!(revision.len(), 40);
        assert!(!workflow.contains("actions/checkout@v4"));
        assert!(!workflow.contains("rust-toolchain@stable"));
    }

    #[test]
    fn release_workflow_requires_a_source_revision_without_creating_files() {
        assert!(release_workflow_template(None, Some(TEST_GIT_REPOSITORY)).is_err());
        assert!(release_workflow_template(Some(TEST_GIT_REVISION), None).is_err());
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-no-revision-{}-{}",
            std::process::id(),
            nonce()
        ));
        let error = create_project_with_revision(
            NewOptions {
                path: Some(directory.clone()),
                interactive: Some(false),
                initialize_git: false,
                ..NewOptions::default()
            },
            None,
            Some(TEST_GIT_REPOSITORY),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no source revision"));
        assert!(!directory.exists());
    }

    #[tokio::test]
    async fn manual_dry_run_does_not_build_package() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-dry-run-{}-{}",
            std::process::id(),
            nonce()
        ));
        let remote = directory.with_extension("remote.git");
        commit_test_project_with_remote(&directory, &remote).unwrap();
        publish_project(PublishOptions {
            path: Some(directory.clone()),
            tag: Some("v0.1.0".to_owned()),
            method: Some(PublishMethod::Manual),
            interactive: Some(false),
            dry_run: true,
            ..PublishOptions::default()
        })
        .await
        .unwrap();
        assert!(!directory.join("target").exists());
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(remote).unwrap();
    }

    #[tokio::test]
    async fn manual_dry_run_accepts_detached_head_like_real_manual_publish() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-detached-dry-run-{}-{}",
            std::process::id(),
            nonce()
        ));
        let remote = directory.with_extension("remote.git");
        commit_test_project_with_remote(&directory, &remote).unwrap();
        git_status(&directory, &["checkout", "--detach"]).unwrap();

        publish_project(PublishOptions {
            path: Some(directory.clone()),
            tag: Some("v0.1.0".to_owned()),
            method: Some(PublishMethod::Manual),
            interactive: Some(false),
            dry_run: true,
            ..PublishOptions::default()
        })
        .await
        .unwrap();

        assert!(!directory.join("target").exists());
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(remote).unwrap();
    }

    #[tokio::test]
    async fn dry_run_rejects_existing_local_or_remote_release_tag() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-occupied-tag-{}-{}",
            std::process::id(),
            nonce()
        ));
        let remote = directory.with_extension("remote.git");
        commit_test_project_with_remote(&directory, &remote).unwrap();
        git_status(&directory, &["tag", "v0.1.0"]).unwrap();

        let local_error =
            ensure_release_tag_available(&directory, remote.to_str().unwrap(), "v0.1.0")
                .await
                .unwrap_err()
                .to_string();
        assert!(local_error.contains("already exists locally"));

        git_status(&directory, &["push", "origin", "refs/tags/v0.1.0"]).unwrap();
        git_status(&directory, &["tag", "-d", "v0.1.0"]).unwrap();
        let remote_error =
            ensure_release_tag_available(&directory, remote.to_str().unwrap(), "v0.1.0")
                .await
                .unwrap_err()
                .to_string();
        assert!(remote_error.contains("already exists on the publish remote"));

        let missing_repository_error = ensure_release_tag_available(
            &directory.join("missing"),
            remote.to_str().unwrap(),
            "v0.2.0",
        )
        .await
        .unwrap_err();
        assert!(
            missing_repository_error
                .to_string()
                .contains("could not check local release tag")
        );
        assert!(missing_repository_error.source().is_some());

        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(remote).unwrap();
    }

    #[test]
    fn automatic_publish_rejects_plugin_subdirectory_repository() {
        let repository = std::env::temp_dir().join(format!(
            "bkmqb-plugin-monorepo-{}-{}",
            std::process::id(),
            nonce()
        ));
        let plugin = repository.join("plugin");
        fs::create_dir_all(&plugin).unwrap();
        initialize_git_repository(&repository).unwrap();
        let error = prepare_git_publish(&plugin, "origin", false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Git repository root"));
        fs::remove_dir_all(repository).unwrap();
    }

    #[test]
    fn automatic_publish_uses_effective_push_url() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-push-url-{}-{}",
            std::process::id(),
            nonce()
        ));
        create_test_project(NewOptions {
            path: Some(directory.clone()),
            interactive: Some(false),
            initialize_git: true,
            ..NewOptions::default()
        })
        .unwrap();
        git_status(&directory, &["config", "user.name", "BKM Test"]).unwrap();
        git_status(
            &directory,
            &["config", "user.email", "test@example.invalid"],
        )
        .unwrap();
        git_status(
            &directory,
            &[
                "add",
                "Cargo.toml",
                "plugin.toml",
                "src/lib.rs",
                "wit/bkm-plugin.wit",
                "README.md",
                ".gitignore",
                ".github/workflows/release.yml",
            ],
        )
        .unwrap();
        git_status(&directory, &["commit", "-m", "Initial plugin"]).unwrap();
        git_status(
            &directory,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/example/read-only.git",
            ],
        )
        .unwrap();
        git_status(
            &directory,
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                "git@github.com:example/publish-target.git",
            ],
        )
        .unwrap();
        let context = prepare_git_publish(&directory, "origin", false).unwrap();
        assert_eq!(
            context.remote_url,
            "git@github.com:example/publish-target.git"
        );
        git_status(
            &directory,
            &[
                "remote",
                "set-url",
                "--add",
                "--push",
                "origin",
                "git@github.com:example/second-target.git",
            ],
        )
        .unwrap();
        assert!(
            prepare_git_publish(&directory, "origin", false)
                .unwrap_err()
                .to_string()
                .contains("exactly one effective push URL")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn clean_tree_check_detects_generated_lockfile() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-lockfile-{}-{}",
            std::process::id(),
            nonce()
        ));
        fs::create_dir_all(&directory).unwrap();
        initialize_git_repository(&directory).unwrap();
        fs::write(directory.join("Cargo.lock"), "generated").unwrap();
        assert!(ensure_clean_git_tree(&directory).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn explicit_language_and_region_form_canonical_locale() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-zh-{}-{}",
            std::process::id(),
            nonce()
        ));
        create_test_project(NewOptions {
            path: Some(directory.clone()),
            language: Some("zh".to_owned()),
            region: Some("cn".to_owned()),
            interactive: Some(false),
            ..NewOptions::default()
        })
        .unwrap();
        let manifest =
            PluginManifest::from_toml(&fs::read_to_string(directory.join("plugin.toml")).unwrap())
                .unwrap();
        assert_eq!(manifest.metadata.default_locale, "zh-CN");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn numeric_directory_generates_valid_rust_type_name() {
        let directory =
            std::env::temp_dir().join(format!("123-plugin-{}-{}", std::process::id(), nonce()));
        create_test_project(NewOptions {
            path: Some(directory.clone()),
            interactive: Some(false),
            ..NewOptions::default()
        })
        .unwrap();
        let source = fs::read_to_string(directory.join("src/lib.rs")).unwrap();
        assert!(source.contains("struct Plugin123Plugin"));
        assert!(
            fs::read_to_string(directory.join("Cargo.toml"))
                .unwrap()
                .contains("name = \"plugin_123_plugin")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn wasm_target_directory_must_exist_and_contain_files() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-empty-target-{}-{}",
            std::process::id(),
            nonce()
        ));
        fs::create_dir_all(&directory).unwrap();
        assert!(!target_libdir_is_populated(directory.to_str().unwrap()));
        fs::write(directory.join("libstd.rlib"), b"fixture").unwrap();
        assert!(target_libdir_is_populated(directory.to_str().unwrap()));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn check_explains_when_current_directory_is_not_a_plugin_project() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-not-plugin-{}-{}",
            std::process::id(),
            nonce()
        ));
        fs::create_dir_all(&directory).unwrap();
        let error = check_project(&directory).unwrap_err().to_string();
        assert!(error.contains("not a plugin project"));
        assert!(error.contains("bkmqb plugin new"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn library_artifact_name_prefers_explicit_lib_name() {
        let directory =
            std::env::temp_dir().join(format!("bkmqb-lib-name-{}-{}", std::process::id(), nonce()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("Cargo.toml"),
            "[package]\nname = \"package-name\"\nversion = \"0.1.0\"\n[lib]\nname = \"custom_guest\"\n",
        )
        .unwrap();
        assert_eq!(library_artifact_name(&directory).unwrap(), "custom_guest");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rust_keywords_are_not_valid_generated_type_names() {
        assert!(!is_rust_identifier("Self"));
        assert!(!is_rust_identifier("async"));
        assert!(!is_rust_identifier("gen"));
        assert!(!is_rust_identifier("struct"));
        assert!(!is_rust_identifier("_"));
        assert!(is_rust_identifier("WeatherPlugin"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn package_failure_preserves_existing_artifact_and_rejects_symlink_input() {
        use std::os::unix::fs::symlink;

        let directory = std::env::temp_dir().join(format!(
            "bkmqb-package-atomic-{}-{}",
            std::process::id(),
            nonce()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("plugin.toml"),
            manifest_template("dev.bkm.atomic", "en", "Atomic", "", "atomic"),
        )
        .unwrap();
        let outside = directory.join("outside.wasm");
        fs::write(&outside, b"not a component").unwrap();
        let component = directory.join("component-link.wasm");
        symlink(&outside, &component).unwrap();
        let output = directory.join("target.bkm-plugin");
        fs::write(&output, b"previous valid artifact").unwrap();

        let error = write_package(&directory, &component, &output)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("regular file"));
        assert_eq!(fs::read(&output).unwrap(), b"previous valid artifact");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn package_rejects_symlinked_asset_root() {
        use std::os::unix::fs::symlink;

        let directory = std::env::temp_dir().join(format!(
            "bkmqb-package-assets-{}-{}",
            std::process::id(),
            nonce()
        ));
        let outside = std::env::temp_dir().join(format!(
            "bkmqb-package-outside-{}-{}",
            std::process::id(),
            nonce()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(directory.join("plugin.toml"), b"fixture").unwrap();
        fs::write(directory.join("component.wasm"), b"fixture").unwrap();
        symlink(&outside, directory.join("assets")).unwrap();
        let output = fs::File::create(directory.join("output.bkm-plugin")).unwrap();
        let error = write_package_archive(&directory, &directory.join("component.wasm"), output)
            .unwrap_err();
        assert!(error.to_string().contains("regular directory"));
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}
