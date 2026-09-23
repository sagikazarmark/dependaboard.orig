//! Parses the YAML metadata block Dependabot appends to its commit messages,
//! falling back to semver-diffing the pull request title.
//!
//! Only the GitHub sync path needs this, and it drags `regex`, `semver` and a
//! YAML parser into any crate that links it, so it lives here in the GitHub
//! client rather than in the core crate the browser bundle links.

use std::sync::LazyLock;

use dependaboard_core::{DependencyUpdate, UpdateType};
use regex::Regex;
use semver::Version;
use serde::Deserialize;

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
        .and_then(|yaml| serde_norway::from_str::<DependabotMetadata>(yaml).ok())
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
    use dependaboard_core::highest_update_type;

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

    /// Every semver step, from a title alone. The fallback is what a pull
    /// request gets when the metadata block is missing or says nothing useful,
    /// and the badge it produces is what a person reads before deciding
    /// whether to look. `malformed_metadata_falls_back_to_title` reaches this
    /// function too, but only ever over a minor bump — the one step that stays
    /// where it is if the major and patch arms are transposed, which would
    /// badge a `1.x` to `2.x` bump as a patch in a dashboard whose whole
    /// business is merging patches without reading them.
    ///
    /// The `v` prefixes are here because Go modules and GitHub Actions write
    /// their versions that way, and a version that does not parse falls to
    /// `Unknown` — dropping such a bump out of a filter on its real kind.
    #[test]
    fn a_titles_versions_say_which_step_it_is_and_an_unparsable_pair_says_nothing() {
        let step = |from: &str, to: &str| {
            parse_dependabot_metadata("", &format!("Bump thing from {from} to {to}"))
                .first()
                .map(|update| update.update_type)
        };

        assert_eq!(step("1.2.3", "2.0.0"), Some(UpdateType::Major));
        assert_eq!(step("1.2.3", "1.3.0"), Some(UpdateType::Minor));
        assert_eq!(step("1.2.3", "1.2.4"), Some(UpdateType::Patch));
        assert_eq!(
            step("v1.2.3", "v2.0.0"),
            Some(UpdateType::Major),
            "a go module"
        );
        assert_eq!(step("v3", "v4"), Some(UpdateType::Unknown), "an action tag");
        assert_eq!(step("21.0.0.1", "21.0.0.2"), Some(UpdateType::Unknown));
    }

    /// The metadata block is Dependabot's own word on what kind of update this
    /// is; the title is a sentence it wrote for people. Where both are present
    /// the block wins, and the title is read only for the versions it names —
    /// otherwise an ordinary four-part or date-shaped version, which no semver
    /// parser accepts, would overwrite a perfectly good `major` with nothing.
    #[test]
    fn the_metadata_block_says_which_step_it_is_and_the_title_only_says_the_versions() {
        let message = r#"Bump thing from 21.0.0.1 to 22.0.0.1

---
updated-dependencies:
- dependency-name: thing
  update-type: version-update:semver-major
..."#;

        let updates = parse_dependabot_metadata(message, "Bump thing from 21.0.0.1 to 22.0.0.1");

        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].update_type,
            UpdateType::Major,
            "the block is believed over a title semver cannot read"
        );
        assert_eq!(updates[0].from_version.as_deref(), Some("21.0.0.1"));
        assert_eq!(updates[0].to_version.as_deref(), Some("22.0.0.1"));
    }

    /// A block whose `update-type` is missing or unrecognised says nothing
    /// about the step, and nothing is what it must be reported as. The field
    /// is `#[serde(default)]` precisely because it goes missing, so this is a
    /// path Dependabot takes rather than one only a corrupt message reaches —
    /// and every other answer is a claim the block did not make.
    #[test]
    fn a_block_that_names_no_step_is_unknown_rather_than_the_gentlest_guess() {
        let without = r#"---
updated-dependencies:
- dependency-name: thing
..."#;
        let updates = parse_dependabot_metadata(without, "a title semver cannot read");
        assert_eq!(updates[0].update_type, UpdateType::Unknown);

        let unrecognised = r#"---
updated-dependencies:
- dependency-name: thing
  update-type: version-update:calendar
..."#;
        let updates = parse_dependabot_metadata(unrecognised, "a title semver cannot read");
        assert_eq!(updates[0].update_type, UpdateType::Unknown);
    }
}
