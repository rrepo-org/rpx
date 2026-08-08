use crate::repository::ArchiveSupport;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const LOCKFILE_VERSION: u32 = 4;
pub const LOCKFILE_REVISION: u32 = 0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lockfile {
    pub version: u32,
    #[serde(default)]
    pub revision: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<LockedRepository>,
    #[serde(default)]
    pub r: LockedR,
    #[serde(default)]
    pub sysreqs: LockedSystemRequirements,
    pub roots: Vec<LockedRoot>,
    pub packages: BTreeMap<String, LockedPackage>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct LockedRepository {
    pub url: String,
    pub kind: LockedRepositoryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cran_archive_support: Option<ArchiveSupport>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum LockedRepositoryKind {
    Rrepo,
    CranLike,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum LockedCranArchiveSupport {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedR {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_packages: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedSystemRequirements {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub db_commit: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub packages: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedRoot {
    pub package: String,
    pub constraint: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct LockedPackage {
    pub package: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<LockedDependency>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct LockedDependency {
    pub package: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_version_exclusive: Option<String>,
}

pub(crate) fn locked_repository_for_source<'a>(
    source_url: &str,
    repositories: &'a [LockedRepository],
) -> Option<&'a LockedRepository> {
    repositories
        .iter()
        .filter(|repository| source_url.starts_with(&repository.url))
        .max_by_key(|repository| repository.url.len())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        LOCKFILE_REVISION, LOCKFILE_VERSION, LockedDependency, LockedPackage, LockedR,
        LockedRepository, LockedRepositoryKind, LockedRoot, LockedSystemRequirements, Lockfile,
    };
    use crate::repository::ArchiveSupport;

    #[test]
    fn serializes_new_registry_lockfile_shape() {
        let lockfile = Lockfile {
            version: LOCKFILE_VERSION,
            revision: LOCKFILE_REVISION,
            repositories: vec![LockedRepository {
                url: "https://api.rrepo.org".to_string(),
                kind: LockedRepositoryKind::Rrepo,
                cran_archive_support: None,
            }],
            r: LockedR {
                version: "4.4.1".to_string(),
                base_packages: vec!["utils".to_string()],
            },
            sysreqs: LockedSystemRequirements {
                db_commit: "abc123".to_string(),
                rules: vec!["libcurl".to_string()],
                packages: BTreeMap::from([("digest".to_string(), vec!["libcurl".to_string()])]),
            },
            roots: vec![LockedRoot {
                package: "digest".to_string(),
                constraint: "*".to_string(),
            }],
            packages: BTreeMap::from([(
                "digest".to_string(),
                LockedPackage {
                    package: "digest".to_string(),
                    version: "0.6.37".to_string(),
                    source: Some("registry".to_string()),
                    source_url: Some(
                        "https://api.rrepo.org/packages/digest/versions/0.6.37/source".to_string(),
                    ),
                    dependencies: vec![LockedDependency {
                        package: "utils".to_string(),
                        kind: "Imports".to_string(),
                        min_version: None,
                        max_version_exclusive: None,
                    }],
                },
            )]),
        };

        let json = serde_json::to_string_pretty(&lockfile).expect("lockfile should serialize");

        assert!(json.contains("\"version\": 4"));
        assert!(json.contains("\"revision\": 0"));
        assert!(json.contains("\"repositories\""));
        assert!(json.contains("\"r\""));
        assert!(json.contains("\"sysreqs\""));
        assert!(json.contains("\"base_packages\""));
        assert!(json.contains("\"db_commit\""));
        assert!(json.contains("\"roots\""));
        assert!(json.contains("\"source_url\""));
        assert!(json.contains("\"dependencies\""));
        assert!(!json.contains("\"use_default_repository\""));
        assert!(!json.contains("\"registry\":"));
    }

    #[test]
    fn round_trips_repository_metadata() {
        let lockfile = Lockfile {
            version: LOCKFILE_VERSION,
            revision: LOCKFILE_REVISION,
            repositories: vec![LockedRepository {
                url: "https://cran.example".to_string(),
                kind: LockedRepositoryKind::CranLike,
                cran_archive_support: Some(ArchiveSupport::Unavailable),
            }],
            r: LockedR::default(),
            sysreqs: LockedSystemRequirements::default(),
            roots: vec![],
            packages: BTreeMap::new(),
        };

        let json = serde_json::to_string(&lockfile).expect("lockfile should serialize");
        let parsed: Lockfile = serde_json::from_str(&json).expect("lockfile should parse");

        assert_eq!(parsed.repositories, lockfile.repositories);
    }

    #[test]
    fn reads_repository_list_as_source_of_truth() {
        let json = r#"{
  "version": 4,
  "revision": 0,
  "repositories": [
    {
      "url": "https://cran.example",
      "kind": "cran-like"
    }
  ],
  "roots": [],
  "packages": {}
}"#;

        let lockfile: Lockfile = serde_json::from_str(json).expect("lockfile should parse");

        assert_eq!(lockfile.repositories.len(), 1);
        assert_eq!(lockfile.repositories[0].url, "https://cran.example");
    }

    #[test]
    fn omits_optional_source_url_when_missing() {
        let package = LockedPackage {
            package: "digest".to_string(),
            version: "0.6.37".to_string(),
            source: None,
            source_url: None,
            dependencies: vec![],
        };

        let json = serde_json::to_string(&package).expect("package should serialize");

        assert!(!json.contains("source_url"));
        assert!(!json.contains("source"));
    }

    #[test]
    fn reads_lockfile_without_runtime_requirements() {
        let json = r#"{
  "version": 4,
  "roots": [],
  "packages": {}
}"#;

        let lockfile: Lockfile = serde_json::from_str(json).expect("lockfile should parse");

        assert_eq!(lockfile.r, LockedR::default());
        assert_eq!(lockfile.sysreqs, LockedSystemRequirements::default());
    }

    #[test]
    fn reads_lockfile_without_revision_as_revision_zero() {
        let json = r#"{
  "version": 4,
  "roots": [],
  "packages": {}
}"#;

        let lockfile: Lockfile = serde_json::from_str(json).expect("lockfile should parse");

        assert_eq!(lockfile.revision, 0);
    }
}
