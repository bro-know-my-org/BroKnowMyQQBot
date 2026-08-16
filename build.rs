use std::{env, fs, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=BKMQB_GIT_REV");
    println!("cargo:rerun-if-env-changed=BKMQB_GIT_REPOSITORY");
    let provenance = match env::var("BKMQB_GIT_REV") {
        Ok(value) if is_full_git_revision(&value) => {
            let repository = source_repository_from_environment().unwrap_or_else(|| {
                panic!(
                    "BKMQB_GIT_REV requires BKMQB_GIT_REPOSITORY or a supported GitHub CARGO_PKG_REPOSITORY"
                )
            });
            Some((value, repository))
        }
        Ok(_) | Err(env::VarError::NotUnicode(_)) => {
            panic!("BKMQB_GIT_REV must be a 40-character hexadecimal commit SHA")
        }
        Err(env::VarError::NotPresent) => {
            println!("cargo:rerun-if-changed=.cargo_vcs_info.json");
            watch_git_metadata();
            watch_worktree_inputs();
            match git_revision_source() {
                GitRevisionSource::Clean(revision, repository) => Some((revision, repository)),
                GitRevisionSource::NotRepository => {
                    packaged_git_revision().zip(source_repository_from_environment())
                }
                GitRevisionSource::UnusableRepository => None,
            }
        }
    };
    if let Some((revision, repository)) = provenance {
        println!("cargo:rustc-env=BKMQB_GIT_REV={revision}");
        println!("cargo:rustc-env=BKMQB_GIT_REPOSITORY={repository}");
    }
}

fn watch_git_metadata() {
    for git_path in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git_output(&["rev-parse", "--git-path", git_path]) {
            if std::path::Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    if let Some(reference) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git_output(&["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

fn watch_worktree_inputs() {
    if let Ok(manifest_directory) = env::var("CARGO_MANIFEST_DIR") {
        println!("cargo:rerun-if-changed={manifest_directory}");
    }
}

enum GitRevisionSource {
    Clean(String, String),
    NotRepository,
    UnusableRepository,
}

fn git_revision_source() -> GitRevisionSource {
    let Some(manifest_directory) = env::var("CARGO_MANIFEST_DIR").ok() else {
        return GitRevisionSource::UnusableRepository;
    };
    let Some(repository_root) = git_output(&["rev-parse", "--show-toplevel"]) else {
        return match has_git_marker(std::path::Path::new(&manifest_directory)) {
            Ok(true) | Err(_) => GitRevisionSource::UnusableRepository,
            Ok(false) => GitRevisionSource::NotRepository,
        };
    };
    match (
        fs::canonicalize(repository_root),
        fs::canonicalize(manifest_directory),
    ) {
        (Ok(repository_root), Ok(manifest_directory)) if repository_root == manifest_directory => {}
        _ => return GitRevisionSource::UnusableRepository,
    }
    if !git_command_succeeds(&["diff-index", "--quiet", "HEAD", "--"])
        || !git_output(&["status", "--porcelain", "--untracked-files=normal"])
            .is_some_and(|status| status.is_empty())
    {
        return GitRevisionSource::UnusableRepository;
    }
    let Some(revision) = git_output(&["rev-parse", "HEAD"]) else {
        return GitRevisionSource::UnusableRepository;
    };
    let Some(repository) = git_output(&["remote", "get-url", "origin"])
        .and_then(|remote| normalize_repository_url(&remote))
    else {
        return GitRevisionSource::UnusableRepository;
    };
    if is_full_git_revision(&revision) {
        GitRevisionSource::Clean(revision, repository)
    } else {
        GitRevisionSource::UnusableRepository
    }
}

fn has_git_marker(path: &std::path::Path) -> Result<bool, std::io::Error> {
    for ancestor in path.ancestors() {
        if ancestor.join(".git").try_exists()? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn source_repository_from_environment() -> Option<String> {
    env::var("BKMQB_GIT_REPOSITORY")
        .ok()
        .or_else(|| env::var("CARGO_PKG_REPOSITORY").ok())
        .and_then(|repository| normalize_repository_url(&repository))
}

fn normalize_repository_url(repository: &str) -> Option<String> {
    let path = repository
        .strip_prefix("https://github.com/")
        .or_else(|| repository.strip_prefix("git@github.com:"))?
        .trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    if owner.is_empty()
        || name.is_empty()
        || segments.next().is_some()
        || !owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some(format!("https://github.com/{owner}/{name}"))
}

fn packaged_git_revision() -> Option<String> {
    let manifest_directory = env::var("CARGO_MANIFEST_DIR").ok()?;
    let metadata =
        fs::read_to_string(std::path::Path::new(&manifest_directory).join(".cargo_vcs_info.json"))
            .ok()?;
    let metadata: serde_json::Value = serde_json::from_str(&metadata).ok()?;
    if metadata
        .get("git")?
        .get("dirty")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return None;
    }
    let revision = metadata.get("git")?.get("sha1")?.as_str()?.to_owned();
    is_full_git_revision(&revision).then_some(revision)
}

fn git_output(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut output = String::from_utf8(output.stdout).ok()?;
    if output.ends_with('\n') {
        output.pop();
        if output.ends_with('\r') {
            output.pop();
        }
    }
    Some(output)
}

fn git_command_succeeds(arguments: &[&str]) -> bool {
    Command::new("git")
        .args(arguments)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn is_full_git_revision(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
