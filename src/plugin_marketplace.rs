//! Static marketplace index and hash-pinned GitHub Release downloads.

use std::{collections::BTreeSet, time::Duration};

use plugin_api::PluginId;
use plugin_host::ValidatedPluginPackage;
use reqwest::{Url, redirect::Policy};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

pub(crate) const DEFAULT_MARKETPLACE_URL: &str =
    "https://bkmqb-plugins.github.io/marketplace/index.json";
const MAX_MARKETPLACE_INDEX_BYTES: usize = 1024 * 1024;
const MAX_PLUGIN_PACKAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarketplaceIndex {
    schema_version: u32,
    plugins: Vec<MarketplacePlugin>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarketplacePlugin {
    pub(crate) slug: String,
    pub(crate) plugin_id: PluginId,
    pub(crate) name: String,
    pub(crate) summary: String,
    pub(crate) trust: String,
    pub(crate) repository: String,
    #[serde(default)]
    pub(crate) commands: Vec<String>,
    pub(crate) latest: MarketplaceRelease,
    pub(crate) capabilities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarketplaceRelease {
    pub(crate) version: String,
    pub(crate) protocol: String,
    pub(crate) download: String,
    pub(crate) sha256: String,
}

#[derive(Debug)]
pub(crate) struct MarketplaceClient {
    index_url: Url,
    index_client: reqwest::Client,
    download_client: reqwest::Client,
}

impl MarketplaceClient {
    pub(crate) fn new(index_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let index_url = parse_https_url(index_url, "marketplace index")?;
        let index_host = index_url
            .host_str()
            .ok_or("marketplace index URL must include a host")?
            .to_ascii_lowercase();
        let index_port = index_url
            .port_or_known_default()
            .ok_or("marketplace index URL must include a port")?;
        let index_client = reqwest::Client::builder()
            .https_only(true)
            .redirect(Policy::custom(move |attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    return attempt.error("marketplace redirect limit exceeded");
                }
                let target = attempt.url();
                let host = target.host_str().unwrap_or_default();
                if target.scheme() != "https"
                    || !host.eq_ignore_ascii_case(&index_host)
                    || target.port_or_known_default() != Some(index_port)
                {
                    return attempt.error("marketplace redirect target is not allowed");
                }
                attempt.follow()
            }))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("BroKnowMyQQBot/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let download_client = reqwest::Client::builder()
            .https_only(true)
            .redirect(Policy::custom(|attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    return attempt.error("plugin download redirect limit exceeded");
                }
                let target = attempt.url();
                if target.scheme() != "https"
                    || !target.host_str().is_some_and(is_github_download_host)
                    || target.port_or_known_default() != Some(443)
                {
                    return attempt.error("plugin download redirect target is not allowed");
                }
                attempt.follow()
            }))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("BroKnowMyQQBot/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            index_url,
            index_client,
            download_client,
        })
    }

    pub(crate) async fn fetch_index(&self) -> Result<MarketplaceIndex, Box<dyn std::error::Error>> {
        let response = self
            .index_client
            .get(self.index_url.clone())
            .send()
            .await?
            .error_for_status()?;
        let bytes = read_bounded_response(response, MAX_MARKETPLACE_INDEX_BYTES, "index").await?;
        let mut index: MarketplaceIndex = serde_json::from_slice(&bytes)?;
        index.validate()?;
        if self.index_url.as_str() != DEFAULT_MARKETPLACE_URL {
            index.mark_review_labels_untrusted();
        }
        Ok(index)
    }

    pub(crate) async fn download(
        &self,
        plugin: &MarketplacePlugin,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let download = validate_release_url(&plugin.latest.download)?;
        let response = self
            .download_client
            .get(download)
            .send()
            .await?
            .error_for_status()?;
        let bytes =
            read_bounded_response(response, MAX_PLUGIN_PACKAGE_BYTES, "plugin package").await?;
        let actual_sha256 = hex::encode(Sha256::digest(&bytes));
        if actual_sha256 != plugin.latest.sha256 {
            return Err(format!(
                "marketplace package checksum mismatch: expected {}, got {actual_sha256}",
                plugin.latest.sha256
            )
            .into());
        }
        Ok(bytes)
    }
}

impl MarketplaceIndex {
    fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.schema_version != 1 {
            return Err(format!(
                "unsupported marketplace schema version {}",
                self.schema_version
            )
            .into());
        }
        let mut slugs = BTreeSet::new();
        let mut plugin_ids = BTreeSet::new();
        for plugin in &self.plugins {
            plugin.validate()?;
            if !slugs.insert(plugin.slug.as_str()) {
                return Err(format!("duplicate marketplace slug `{}`", plugin.slug).into());
            }
            if !plugin_ids.insert(plugin.plugin_id.as_str()) {
                return Err(
                    format!("duplicate marketplace plugin ID `{}`", plugin.plugin_id).into(),
                );
            }
        }
        Ok(())
    }

    pub(crate) fn find(
        &self,
        selector: &str,
    ) -> Result<&MarketplacePlugin, Box<dyn std::error::Error>> {
        validate_selector(selector)?;
        let normalized = selector.to_ascii_lowercase();
        self.plugins
            .iter()
            .find(|plugin| plugin.slug == normalized || plugin.plugin_id.as_str() == selector)
            .ok_or_else(|| format!("plugin `{selector}` was not found in the marketplace").into())
    }

    pub(crate) fn search<'a>(&'a self, query: &str) -> Vec<&'a MarketplacePlugin> {
        let query = query.trim().to_ascii_lowercase();
        self.plugins
            .iter()
            .filter(|plugin| {
                query.is_empty() || plugin.search_text().to_ascii_lowercase().contains(&query)
            })
            .collect()
    }

    fn mark_review_labels_untrusted(&mut self) {
        for plugin in &mut self.plugins {
            "untrusted".clone_into(&mut plugin.trust);
        }
    }
}

pub(crate) fn validate_selector(selector: &str) -> Result<(), Box<dyn std::error::Error>> {
    if valid_slug(&selector.to_ascii_lowercase()) || PluginId::new(selector.to_owned()).is_ok() {
        Ok(())
    } else {
        Err(format!("invalid marketplace plugin selector `{selector}`").into())
    }
}

impl MarketplacePlugin {
    fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        if !valid_slug(&self.slug) {
            return Err(format!("invalid marketplace slug `{}`", self.slug).into());
        }
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || contains_unsafe_display_characters(&self.name)
        {
            return Err(format!("marketplace plugin `{}` has an invalid name", self.slug).into());
        }
        if self.summary.len() > 4096 {
            return Err(format!("marketplace plugin `{}` summary is too large", self.slug).into());
        }
        if contains_unsafe_display_characters(&self.summary) {
            return Err(format!(
                "marketplace plugin `{}` summary contains unsafe display characters",
                self.slug
            )
            .into());
        }
        if !matches!(self.trust.as_str(), "official" | "verified" | "community") {
            return Err(format!(
                "marketplace plugin `{}` has an invalid trust label",
                self.slug
            )
            .into());
        }
        let repository = parse_github_repository_url(&self.repository)?;
        let release = parse_github_release_url(&self.latest.download)?;
        if !repository.0.eq_ignore_ascii_case(&release.0)
            || !repository.1.eq_ignore_ascii_case(&release.1)
        {
            return Err(format!(
                "marketplace plugin `{}` download does not belong to its declared repository",
                self.slug
            )
            .into());
        }
        if self.latest.version.trim().is_empty()
            || self.latest.version.len() > 128
            || contains_unsafe_display_characters(&self.latest.version)
            || self.latest.protocol.trim().is_empty()
            || self.latest.protocol.len() > 128
            || contains_unsafe_display_characters(&self.latest.protocol)
        {
            return Err(format!(
                "marketplace plugin `{}` has incomplete version metadata",
                self.slug
            )
            .into());
        }
        if !release_tag_matches_version(&release.2, &self.latest.version) {
            return Err(format!(
                "marketplace plugin `{}` download tag does not match version `{}`",
                self.slug, self.latest.version
            )
            .into());
        }
        if self.latest.sha256.len() != 64
            || !self
                .latest
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(
                format!("marketplace plugin `{}` has an invalid SHA-256", self.slug).into(),
            );
        }
        if self.commands.iter().any(|command| {
            canonical_command(command).is_empty()
                || command.len() > 128
                || contains_unsafe_display_characters(command)
        }) || self.capabilities.iter().any(|capability| {
            capability.is_empty()
                || capability.len() > 256
                || contains_unsafe_display_characters(capability)
        }) {
            return Err(format!(
                "marketplace plugin `{}` contains malformed command or capability metadata",
                self.slug
            )
            .into());
        }
        if contains_canonical_command_duplicates(&self.commands)
            || contains_duplicates(&self.capabilities)
        {
            return Err(format!(
                "marketplace plugin `{}` contains duplicate list entries",
                self.slug
            )
            .into());
        }
        Ok(())
    }

    fn search_text(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.slug,
            self.plugin_id,
            self.name,
            self.summary,
            self.commands.join(" "),
            self.capabilities.join(" ")
        )
    }

    pub(crate) fn validate_package(
        &self,
        package: &ValidatedPluginPackage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let manifest = package.manifest();
        if manifest.id != self.plugin_id {
            return Err(format!(
                "marketplace plugin ID mismatch: index has `{}`, package has `{}`",
                self.plugin_id, manifest.id
            )
            .into());
        }
        if manifest.version != self.latest.version {
            return Err(format!(
                "marketplace plugin version mismatch: index has `{}`, package has `{}`",
                self.latest.version, manifest.version
            )
            .into());
        }
        if manifest.protocol != self.latest.protocol {
            return Err(format!(
                "marketplace protocol requirement mismatch: index has `{}`, package has `{}`",
                self.latest.protocol, manifest.protocol
            )
            .into());
        }
        if package.package_sha256() != self.latest.sha256 {
            return Err("marketplace package hash changed after validation".into());
        }
        let packaged_command_values = manifest
            .commands
            .iter()
            .flat_map(|command| std::iter::once(&command.name).chain(&command.aliases))
            .map(|command| canonical_command(command))
            .collect::<Vec<_>>();
        let packaged_commands = packaged_command_values
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if packaged_commands.len() != packaged_command_values.len() {
            return Err(format!(
                "package manifest contains duplicate runtime commands for `{}`",
                self.slug
            )
            .into());
        }
        let indexed_commands = self
            .commands
            .iter()
            .map(|command| canonical_command(command))
            .collect::<BTreeSet<_>>();
        if packaged_commands != indexed_commands {
            return Err(format!(
                "marketplace command summary does not match package manifest for `{}`",
                self.slug
            )
            .into());
        }
        let requested = manifest.requested_capabilities();
        let indexed = self.capabilities.iter().cloned().collect::<BTreeSet<_>>();
        if requested != indexed {
            return Err(format!(
                "marketplace capability summary does not match package manifest for `{}`",
                self.slug
            )
            .into());
        }
        Ok(())
    }
}

fn valid_slug(slug: &str) -> bool {
    (1..=64).contains(&slug.len())
        && slug
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn contains_duplicates(values: &[String]) -> bool {
    let unique = values.iter().collect::<BTreeSet<_>>();
    unique.len() != values.len()
}

fn contains_canonical_command_duplicates(values: &[String]) -> bool {
    let unique = values
        .iter()
        .map(|value| canonical_command(value))
        .collect::<BTreeSet<_>>();
    unique.len() != values.len()
}

fn canonical_command(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn contains_unsafe_display_characters(value: &str) -> bool {
    value.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '\u{00ad}'
                    | '\u{061c}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{feff}'
            )
    })
}

fn parse_https_url(value: &str, label: &str) -> Result<Url, Box<dyn std::error::Error>> {
    let url = Url::parse(value)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || url.fragment().is_some()
    {
        return Err(format!("{label} must be an HTTPS URL without credentials or fragment").into());
    }
    Ok(url)
}

pub(crate) fn validate_index_url(value: &str) -> Result<(), String> {
    parse_https_url(value, "marketplace index")
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn validate_release_url(value: &str) -> Result<Url, Box<dyn std::error::Error>> {
    let url = parse_https_url(value, "plugin download")?;
    parse_github_release_url(value)?;
    Ok(url)
}

fn parse_github_repository_url(
    value: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let url = parse_https_url(value, "plugin repository")?;
    let segments = url
        .path_segments()
        .ok_or("plugin repository URL must have path segments")?
        .collect::<Vec<_>>();
    if !url
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("github.com"))
        || url.port_or_known_default() != Some(443)
        || url.query().is_some()
        || segments.len() != 2
        || !valid_github_owner(segments[0])
        || !valid_github_repository(segments[1])
    {
        return Err("plugin repository must be an exact github.com owner/repository URL".into());
    }
    Ok((segments[0].to_owned(), segments[1].to_owned()))
}

fn parse_github_release_url(
    value: &str,
) -> Result<(String, String, String, String), Box<dyn std::error::Error>> {
    let url = parse_https_url(value, "plugin download")?;
    let segments = url
        .path_segments()
        .ok_or("plugin download URL must have path segments")?
        .collect::<Vec<_>>();
    if !url
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("github.com"))
        || url.port_or_known_default() != Some(443)
        || url.query().is_some()
        || segments.len() != 6
        || segments[2] != "releases"
        || segments[3] != "download"
        || !valid_github_owner(segments[0])
        || !valid_github_repository(segments[1])
    {
        return Err(
            "plugin download must be an exact fixed-version github.com Release asset URL".into(),
        );
    }
    let tag = decode_url_segment(segments[4], "plugin release tag")?;
    let asset = decode_url_segment(segments[5], "plugin release asset")?;
    if tag.eq_ignore_ascii_case("latest")
        || !valid_release_tag(&tag)
        || !valid_release_asset(&asset)
    {
        return Err("plugin download must contain a valid fixed release tag and asset name".into());
    }
    Ok((segments[0].to_owned(), segments[1].to_owned(), tag, asset))
}

fn valid_github_owner(value: &str) -> bool {
    (1..=39).contains(&value.len())
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn valid_github_repository(value: &str) -> bool {
    (1..=100).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn decode_url_segment(value: &str, label: &str) -> Result<String, Box<dyn std::error::Error>> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(encoded) = bytes.get(index + 1..index + 3) else {
                return Err(format!("{label} contains incomplete percent encoding").into());
            };
            let high = hex_digit(encoded[0])
                .ok_or_else(|| format!("{label} contains invalid percent encoding"))?;
            let low = hex_digit(encoded[1])
                .ok_or_else(|| format!("{label} contains invalid percent encoding"))?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| format!("{label} is not UTF-8").into())
}

const fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn valid_release_tag(value: &str) -> bool {
    (1..=255).contains(&value.len())
        && value != "@"
        && !value.starts_with('/')
        && !value.ends_with(['/', '.'])
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains("//")
        && !value.split('/').any(|component| {
            component.is_empty()
                || component.starts_with('.')
                || component
                    .rsplit_once('.')
                    .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("lock"))
        })
        && !value.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
}

fn valid_release_asset(value: &str) -> bool {
    (1..=255).contains(&value.len())
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\' | '?' | '#'))
}

fn release_tag_matches_version(tag: &str, version: &str) -> bool {
    tag == version
        || tag == format!("v{version}")
        || tag.ends_with(&format!("/{version}"))
        || tag.ends_with(&format!("/v{version}"))
}

fn is_github_download_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "github.com"
            | "objects.githubusercontent.com"
            | "release-assets.githubusercontent.com"
            | "github-releases.githubusercontent.com"
    )
}

async fn read_bounded_response(
    mut response: reqwest::Response,
    limit: usize,
    label: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if response
        .content_length()
        .is_some_and(|length| length > u64::try_from(limit).unwrap_or(u64::MAX))
    {
        return Err(format!("marketplace {label} exceeds {limit} bytes").into());
    }
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(format!("marketplace {label} exceeds {limit} bytes").into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write as _};

    use base64::Engine as _;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::*;

    const VALID_INDEX: &str = r#"{
      "schema_version": 1,
      "plugins": [{
        "slug": "github-issue",
        "plugin_id": "io.github.bkmqb-community.github-issue",
        "name": "GitHub Issue Reporter",
        "summary": "Create issues",
        "trust": "official",
        "repository": "https://github.com/BKMQB-Plugins/github-issue",
        "commands": ["issue"],
        "latest": {
          "version": "0.2.3",
          "protocol": ">=1.1,<2.0",
          "download": "https://github.com/BKMQB-Plugins/github-issue/releases/download/v0.2.3/plugin.bkm-plugin",
          "sha256": "b1fec095e309aed2bf7235902a9f49ba177e5c7767b98a8c6a5c07da68b0a1b5"
        },
        "capabilities": ["message.reply"]
      }]
    }"#;

    fn ping_package_with_manifest(manifest: &str) -> ValidatedPluginPackage {
        let fixture = base64::engine::general_purpose::STANDARD
            .decode(
                include_str!("../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
            )
            .unwrap();
        let component = ValidatedPluginPackage::from_bytes(&fixture)
            .unwrap()
            .component()
            .to_vec();
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        archive.start_file("plugin.toml", options).unwrap();
        archive.write_all(manifest.as_bytes()).unwrap();
        archive.start_file("component.wasm", options).unwrap();
        archive.write_all(&component).unwrap();
        ValidatedPluginPackage::from_bytes(&archive.finish().unwrap().into_inner()).unwrap()
    }

    #[test]
    fn marketplace_index_validates_and_resolves_selectors() {
        let index: MarketplaceIndex = serde_json::from_str(VALID_INDEX).unwrap();
        index.validate().unwrap();
        assert_eq!(index.find("github-issue").unwrap().latest.version, "0.2.3");
        assert_eq!(index.find("GitHub-Issue").unwrap().latest.version, "0.2.3");
        assert!(
            index
                .find("IO.GITHUB.BKMQB-COMMUNITY.GITHUB-ISSUE")
                .is_err()
        );
        assert_eq!(
            index
                .find("io.github.bkmqb-community.github-issue")
                .unwrap()
                .slug,
            "github-issue"
        );
        assert_eq!(index.search("message.reply").len(), 1);
        validate_selector("github-issue").unwrap();
        validate_selector("GitHub-Issue").unwrap();
        validate_selector("io.github.bkmqb-community.github-issue").unwrap();
        assert!(validate_selector("not a selector").is_err());
    }

    #[test]
    fn marketplace_rejects_mutable_or_non_github_downloads() {
        assert!(validate_release_url("https://example.com/plugin.bkm-plugin").is_err());
        assert!(
            validate_release_url("https://github.com/o/r/releases/latest/download/p.bkm-plugin")
                .is_err()
        );
        assert!(
            validate_release_url(
                "https://github.com/o/r/releases/download/%6c%61%74%65%73%74/p.bkm-plugin"
            )
            .is_err()
        );
        assert!(
            validate_release_url("http://github.com/o/r/releases/download/v1/p.bkm-plugin")
                .is_err()
        );
    }

    #[test]
    fn marketplace_rejects_duplicate_identity() {
        let duplicated = VALID_INDEX.replace(
            "]\n    }",
            ", {\"slug\":\"github-issue\",\"plugin_id\":\"dev.bkm.other\",\"name\":\"Other\",\"summary\":\"\",\"trust\":\"community\",\"repository\":\"https://github.com/o/r\",\"commands\":[],\"latest\":{\"version\":\"1.0.0\",\"protocol\":\">=1.1,<2.0\",\"download\":\"https://github.com/o/r/releases/download/v1/p.bkm-plugin\",\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"},\"capabilities\":[]} ]\n    }",
        );
        let index: MarketplaceIndex = serde_json::from_str(&duplicated).unwrap();
        assert!(index.validate().is_err());
    }

    #[test]
    fn marketplace_binds_download_to_repository_and_version() {
        let wrong_repository = VALID_INDEX.replace(
            "BKMQB-Plugins/github-issue/releases/download",
            "attacker/github-issue/releases/download",
        );
        let index: MarketplaceIndex = serde_json::from_str(&wrong_repository).unwrap();
        assert!(index.validate().is_err());

        let wrong_tag = VALID_INDEX.replace("download/v0.2.3/", "download/nightly/");
        let index: MarketplaceIndex = serde_json::from_str(&wrong_tag).unwrap();
        assert!(index.validate().is_err());

        assert!(parse_github_repository_url("https://github.com/owner/repo/extra").is_err());
        assert!(
            validate_release_url(
                "https://github.com/owner/repo/extra/releases/download/v1/plugin.bkm-plugin"
            )
            .is_err()
        );
        assert!(
            validate_release_url(
                "https://github.com:8443/owner/repo/releases/download/v1/plugin.bkm-plugin"
            )
            .is_err()
        );
        let release = parse_github_release_url(
            "https://github.com/owner/repo/releases/download/release%2Fv1.0.0/plugin%20name.bkm-plugin",
        )
        .unwrap();
        assert_eq!(release.2, "release/v1.0.0");
        assert_eq!(release.3, "plugin name.bkm-plugin");
        assert!(release_tag_matches_version(&release.2, "1.0.0"));
        for invalid in [
            "release v1.0.0",
            "release..v1.0.0",
            "foo~1/v1.0.0",
            "release:/v1.0.0",
            ".hidden/v1.0.0",
            "release.lock/v1.0.0",
            "release//v1.0.0",
            "release/v1.0.0.",
        ] {
            assert!(
                !valid_release_tag(invalid),
                "accepted invalid tag {invalid}"
            );
        }
    }

    #[test]
    fn marketplace_rejects_terminal_control_characters() {
        let malicious = VALID_INDEX.replace("Create issues", "Create issues\\u001b[2J");
        let index: MarketplaceIndex = serde_json::from_str(&malicious).unwrap();
        assert!(index.validate().is_err());

        let bidi = VALID_INDEX.replace("Create issues", "Create issues\\u202eofficial");
        let index: MarketplaceIndex = serde_json::from_str(&bidi).unwrap();
        assert!(index.validate().is_err());

        let line_separator = VALID_INDEX.replace("Create issues", "Create issues\\u2028forged");
        let index: MarketplaceIndex = serde_json::from_str(&line_separator).unwrap();
        assert!(index.validate().is_err());

        let zero_width = VALID_INDEX.replace("Create issues", "Create issues\\u200bhidden");
        let index: MarketplaceIndex = serde_json::from_str(&zero_width).unwrap();
        assert!(index.validate().is_err());
    }

    #[test]
    fn custom_marketplaces_cannot_claim_official_review_labels() {
        let mut index: MarketplaceIndex = serde_json::from_str(VALID_INDEX).unwrap();
        index.validate().unwrap();
        index.mark_review_labels_untrusted();
        assert_eq!(index.find("github-issue").unwrap().trust, "untrusted");
    }

    #[test]
    fn marketplace_package_commands_must_match_the_index() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(
                include_str!("../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
            )
            .unwrap();
        let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
        let manifest = package.manifest();
        let commands = manifest
            .commands
            .iter()
            .flat_map(|command| std::iter::once(&command.name).chain(&command.aliases))
            .cloned()
            .collect();
        let plugin = MarketplacePlugin {
            slug: "wasm-ping".to_owned(),
            plugin_id: manifest.id.clone(),
            name: "WASM Ping".to_owned(),
            summary: "Ping fixture".to_owned(),
            trust: "community".to_owned(),
            repository: "https://github.com/example/wasm-ping".to_owned(),
            commands,
            latest: MarketplaceRelease {
                version: manifest.version.clone(),
                protocol: manifest.protocol.clone(),
                download: format!(
                    "https://github.com/example/wasm-ping/releases/download/v{}/plugin.bkm-plugin",
                    manifest.version
                ),
                sha256: package.package_sha256().to_owned(),
            },
            capabilities: manifest.requested_capabilities().into_iter().collect(),
        };
        plugin.validate_package(&package).unwrap();

        let mut differently_cased = plugin.clone();
        differently_cased.commands = differently_cased
            .commands
            .into_iter()
            .map(|command| command.to_ascii_uppercase())
            .collect();
        differently_cased.validate().unwrap();
        differently_cased.validate_package(&package).unwrap();

        let mut mismatched = plugin;
        mismatched.commands.push("not-in-package".to_owned());
        assert!(mismatched.validate_package(&package).is_err());
    }

    #[test]
    fn marketplace_rejects_package_commands_that_collide_at_runtime() {
        let manifest_source = include_str!("../test-support/wasm-plugins/ping/plugin.toml")
            .replacen(
                "name = \"ping\"",
                "name = \"ping\"\naliases = [\"PING\"]",
                1,
            );
        let package = ping_package_with_manifest(&manifest_source);
        let manifest = package.manifest();
        let plugin = MarketplacePlugin {
            slug: "wasm-ping".to_owned(),
            plugin_id: manifest.id.clone(),
            name: "WASM Ping".to_owned(),
            summary: "Ping fixture".to_owned(),
            trust: "community".to_owned(),
            repository: "https://github.com/example/wasm-ping".to_owned(),
            commands: vec!["ping".to_owned()],
            latest: MarketplaceRelease {
                version: manifest.version.clone(),
                protocol: manifest.protocol.clone(),
                download: format!(
                    "https://github.com/example/wasm-ping/releases/download/v{}/plugin.bkm-plugin",
                    manifest.version
                ),
                sha256: package.package_sha256().to_owned(),
            },
            capabilities: manifest.requested_capabilities().into_iter().collect(),
        };

        let error = plugin.validate_package(&package).unwrap_err().to_string();
        assert!(error.contains("duplicate runtime commands"));
    }

    #[test]
    fn marketplace_command_duplicates_use_runtime_command_semantics() {
        let mut index: MarketplaceIndex = serde_json::from_str(VALID_INDEX).unwrap();
        index.plugins[0].commands = vec!["aliasrepo".to_owned(), "AliasRepo".to_owned()];
        assert!(index.validate().is_err());
    }
}
