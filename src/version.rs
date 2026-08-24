//! Build provenance exposed by the CLI and startup logs.

const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_REVISION: Option<&str> = option_env!("BKMQB_GIT_REV");
const BUILD_DATE: &str = env!("BKMQB_BUILD_DATE");

pub(crate) fn display() -> String {
    format_version(PACKAGE_VERSION, GIT_REVISION, BUILD_DATE)
}

fn format_version(version: &str, revision: Option<&str>, build_date: &str) -> String {
    let revision = revision.map_or("unknown", |revision| revision.get(..12).unwrap_or(revision));
    format!("bkmqb {version} (git {revision}, built {build_date})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_includes_short_git_revision_and_build_date() {
        assert_eq!(
            format_version(
                "1.2.3",
                Some("0123456789abcdef0123456789abcdef01234567"),
                "2026-08-24"
            ),
            "bkmqb 1.2.3 (git 0123456789ab, built 2026-08-24)"
        );
    }

    #[test]
    fn version_marks_unavailable_git_revision_as_unknown() {
        assert_eq!(
            format_version("1.2.3", None, "2026-08-24"),
            "bkmqb 1.2.3 (git unknown, built 2026-08-24)"
        );
    }
}
