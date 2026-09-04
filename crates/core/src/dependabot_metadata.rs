//! Parses the YAML metadata block Dependabot appends to its commit messages,
//! falling back to semver-diffing the pull request title.
//!
//! Only the GitHub sync path needs this, and it drags `regex`, `semver` and a
//! YAML parser into any crate that links it. It is gated behind the
//! `dependabot-metadata` feature so the browser bundle can leave it out.

use std::sync::LazyLock;

use regex::Regex;
use semver::Version;
use serde::Deserialize;

use crate::{DependencyUpdate, UpdateType};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct DependabotMetadata {
    #[serde(default)]
    updated_dependencies: Vec<MetadataDependency>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct MetadataDependency {
    dependency_name: String,
    #[serde(default)]
    update_type: String,
}

pub fn parse_dependabot_metadata(message: &str, title: &str) -> Vec<DependencyUpdate> {
    let yaml_updates = metadata_block(message)
        .and_then(|yaml| serde_yml::from_str::<DependabotMetadata>(yaml).ok())
        .map(|metadata| {
            metadata
                .updated_dependencies
                .into_iter()
                .map(|dependency| DependencyUpdate {
                    name: dependency.dependency_name,
                    from_version: None,
                    to_version: None,
                    update_type: metadata_update_type(&dependency.update_type),
                })
                .collect::<Vec<_>>()
        })
        .filter(|updates| !updates.is_empty());

    if let Some(mut updates) = yaml_updates {
        if updates.len() == 1
            && let Some((name, from, to, update_type)) = parse_title_update(title)
            && updates[0].name == name
        {
            updates[0].from_version = Some(from);
            updates[0].to_version = Some(to);
            if updates[0].update_type == UpdateType::Unknown {
                updates[0].update_type = update_type;
            }
        }
        return updates;
    }

    parse_title_update(title)
        .map(|(name, from, to, update_type)| {
            vec![DependencyUpdate {
                name,
                from_version: Some(from),
                to_version: Some(to),
                update_type,
            }]
        })
        .unwrap_or_default()
}

fn metadata_block(message: &str) -> Option<&str> {
    let rest = if let Some(rest) = message.strip_prefix("---") {
        rest
    } else {
        let start = message.find("\n---")? + 4;
        &message[start..]
    };
    let end = rest.find("\n...")?;
    Some(&rest[..end])
}

fn metadata_update_type(value: &str) -> UpdateType {
    match value.rsplit(':').next() {
        Some("semver-major") => UpdateType::Major,
        Some("semver-minor") => UpdateType::Minor,
        Some("semver-patch") => UpdateType::Patch,
        _ => UpdateType::Unknown,
    }
}

/// Matches the "Bump <name> from <from> to <to>" subject Dependabot writes for
/// single-dependency updates. Compiled once per process; the pattern is a
/// literal, so a compile failure is a programming error rather than bad input.
static TITLE_UPDATE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bbump\s+(.+?)\s+from\s+(\S+)\s+to\s+(\S+)")
        .expect("Dependabot title pattern is a valid regex")
});

fn parse_title_update(title: &str) -> Option<(String, String, String, UpdateType)> {
    let captures = TITLE_UPDATE_PATTERN.captures(title)?;
    let name = captures.get(1)?.as_str().trim().to_owned();
    let from = captures.get(2)?.as_str().trim_matches('`').to_owned();
    let to = captures
        .get(3)?
        .as_str()
        .trim_matches(|c: char| c == '`' || c == '.' || c == ',')
        .to_owned();
    let update_type = semver_update_type(&from, &to);
    Some((name, from, to, update_type))
}

fn semver_update_type(from: &str, to: &str) -> UpdateType {
    let parse = |value: &str| Version::parse(value.trim_start_matches('v'));
    let (Ok(from), Ok(to)) = (parse(from), parse(to)) else {
        return UpdateType::Unknown;
    };
    if from.major != to.major {
        UpdateType::Major
    } else if from.minor != to.minor {
        UpdateType::Minor
    } else if from.patch != to.patch {
        UpdateType::Patch
    } else {
        UpdateType::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highest_update_type;

    #[test]
    fn parses_single_dependency_metadata_and_versions() {
        let message = r#"Bump tokio from 1.0.0 to 2.0.0

---
updated-dependencies:
- dependency-name: tokio
  dependency-type: direct:production
  update-type: version-update:semver-major
..."#;
        let updates = parse_dependabot_metadata(message, "Bump tokio from 1.0.0 to 2.0.0");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].name, "tokio");
        assert_eq!(updates[0].from_version.as_deref(), Some("1.0.0"));
        assert_eq!(updates[0].to_version.as_deref(), Some("2.0.0"));
        assert_eq!(updates[0].update_type, UpdateType::Major);
    }

    #[test]
    fn grouped_metadata_keeps_every_dependency_and_highest_type() {
        let message = r#"---

---
updated-dependencies:
- dependency-name: tokio
  update-type: version-update:semver-minor
- dependency-name: serde
  update-type: version-update:semver-major
..."#;
        let updates = parse_dependabot_metadata(message, "Bump the rust group");
        assert_eq!(updates.len(), 2);
        assert_eq!(highest_update_type(&updates), UpdateType::Major);
        assert!(updates.iter().all(|update| update.from_version.is_none()));
    }

    #[test]
    fn malformed_metadata_falls_back_to_title() {
        let message = "subject\n\n---\nthis: [is invalid\n...";
        let updates = parse_dependabot_metadata(message, "Bump serde from 1.0.0 to 1.1.0");
        assert_eq!(updates[0].update_type, UpdateType::Minor);
    }

    #[test]
    fn metadata_can_start_at_the_first_byte() {
        let message = "---\nupdated-dependencies:\n- dependency-name: serde\n  update-type: version-update:semver-patch\n...";
        let updates = parse_dependabot_metadata(message, "group update");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].name, "serde");
        assert_eq!(updates[0].update_type, UpdateType::Patch);
    }
}
