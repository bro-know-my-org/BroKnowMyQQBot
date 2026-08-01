//! Third-party BPP plugin project scaffolding and build pipeline.

use std::{
    fs::{self, OpenOptions},
    io::IsTerminal as _,
    path::{Path, PathBuf},
    process::Command,
};

use dialoguer::Input;
use plugin_api::PluginManifest;
use plugin_host::{ValidatedPluginPackage, WasmPlugin};
use wit_component::ComponentEncoder;
use zip::{ZipWriter, write::SimpleFileOptions};

const WIT_SOURCE: &str = include_str!("../crates/plugin-api/wit/bkm-plugin.wit");

#[derive(Debug, Default)]
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
        "build" => build_project(optional_path(arguments)?).await?,
        "package" => package_project(optional_path(arguments)?).await?,
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
                unknown => return Err(format!("unknown plugin new option `{unknown}`").into()),
            };
            index += 1;
            *target = Some(
                arguments
                    .get(index)
                    .ok_or_else(|| format!("{argument} requires a value"))?
                    .clone(),
            );
            index += 1;
        }
        Ok(options)
    }
}

fn create_project(mut options: NewOptions) -> Result<(), Box<dyn std::error::Error>> {
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
    let locale = if let Some(locale) = options.locale {
        locale
    } else {
        let language =
            value_or_prompt(options.language, interactive, "Language", "en")?.to_ascii_lowercase();
        let region = value_or_prompt(options.region, interactive, "Region (optional)", "")?
            .to_ascii_uppercase();
        if region.is_empty() {
            language
        } else {
            format!("{language}-{region}")
        }
    };
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

    fs::create_dir_all(path.join("src"))?;
    fs::create_dir_all(path.join("wit"))?;
    fs::create_dir_all(path.join("assets"))?;
    fs::write(path.join("Cargo.toml"), cargo)?;
    fs::write(path.join("plugin.toml"), manifest)?;
    fs::write(path.join("src/lib.rs"), source)?;
    fs::write(path.join("wit/bkm-plugin.wit"), WIT_SOURCE)?;
    fs::write(path.join("README.md"), readme)?;
    fs::write(path.join(".gitignore"), "/target\n")?;

    println!("Created plugin project `{}`", path.display());
    println!("  cd {}", path.display());
    println!("  bkmqb plugin build");
    Ok(())
}

fn optional_path(arguments: &[String]) -> Result<&Path, Box<dyn std::error::Error>> {
    if arguments.len() > 1 {
        return Err("expected at most one project path".into());
    }
    Ok(arguments.first().map_or_else(|| Path::new("."), Path::new))
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

async fn build_project(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
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

async fn package_project(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = check_project(path)?;
    let component_path = path.join("target/bkmqb/component.wasm");
    if !component_path.is_file() {
        return Err("component.wasm is missing; run `bkmqb plugin build` first".into());
    }
    let package_path = default_package_path(&path.join("target/bkmqb"), &manifest);
    write_package(path, &component_path, &package_path).await?;
    println!("Packaged plugin `{}`", package_path.display());
    Ok(())
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
    if let Ok(metadata) = fs::symlink_metadata(output)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(format!(
            "plugin package destination `{}` must be a regular file",
            output.display()
        )
        .into());
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
        "manifest_version = 1\nid = {}\nversion = \"0.1.0\"\nprotocol = \">=1.0,<2.0\"\nentry = \"component.wasm\"\n\n[metadata]\ndefault_locale = {}\n\n[metadata.locales.{}]\nname = {}\ndescription = {}\n\n[[subscriptions]]\nid = \"message-handler\"\nevent = \"message.created\"\npriority = 0\nscopes = [\"group\", \"private\", \"channel\"]\n\n[[commands]]\nname = {}\naliases = []\ndescription = {}\n\n[permissions]\nactions = [\"message.reply\"]\n",
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
        "# {display_name}\n\nGenerated BPP 1.0 plugin.\n\n```bash\nbkmqb plugin check\nbkmqb plugin build\nbkmqb plugin inspect target/bkmqb/*.bkm-plugin\nbkmqb plugin install target/bkmqb/*.bkm-plugin\n```\n\nThe starter command is `/{command}`.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonce() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn non_interactive_scaffold_uses_defaults() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-new-{}-{}",
            std::process::id(),
            nonce()
        ));
        create_project(NewOptions {
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
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn explicit_language_and_region_form_canonical_locale() {
        let directory = std::env::temp_dir().join(format!(
            "bkmqb-plugin-zh-{}-{}",
            std::process::id(),
            nonce()
        ));
        create_project(NewOptions {
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
        create_project(NewOptions {
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
