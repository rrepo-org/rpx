use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const LOCKFILE_VERSION: u32 = 5;
pub const LOCKFILE_REVISION: u32 = 0;

#[derive(Debug, Deserialize)]
pub(crate) struct LockfileHeader {
    pub version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lockfile {
    pub version: u32,
    pub revision: u32,
    pub r: semver::Version,
    pub sysreqs: SystemRequirements,
    pub repos: Vec<Repository>,
    #[serde(with = "relation_set")]
    pub requirements: BTreeSet<r_description::Relation>,
    pub packages: BTreeMap<String, Package>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SystemRequirements {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_git_oid"
    )]
    pub db_commit: Option<git2::Oid>,
    pub rules: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Repository {
    Rrepo {
        #[serde(with = "repository_url")]
        url: url::Url,
    },
    CranLike {
        #[serde(with = "repository_url")]
        url: url::Url,
        archive_support: ArchiveSupport,
    },
    Git {
        #[serde(with = "repository_url")]
        url: url::Url,
        reference: GitReference,
        #[serde(with = "git_oid")]
        commit: git2::Oid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subdirectory: Option<relative_path::RelativePathBuf>,
    },
}

impl Repository {
    pub fn url(&self) -> &url::Url {
        match self {
            Self::Rrepo { url } | Self::CranLike { url, .. } | Self::Git { url, .. } => url,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveSupport {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum GitReference {
    DefaultBranch,
    Named { value: String },
    Commit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Package {
    #[serde(with = "package_version")]
    pub version: r_description::Version,
    #[serde(with = "repository_url")]
    pub repository: url::Url,
    #[serde(with = "relation_set")]
    pub dependencies: BTreeSet<r_description::Relation>,
}

mod relation_set {
    use r_description::Relation;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    use std::collections::BTreeSet;

    pub fn serialize<S>(relations: &BTreeSet<Relation>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(relations.iter().map(ToString::to_string))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BTreeSet<Relation>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<String>::deserialize(deserializer)?
            .into_iter()
            .map(|value| value.parse().map_err(D::Error::custom))
            .collect()
    }
}

mod repository_url {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use url::Url;

    pub fn serialize<S>(url: &Url, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        canonicalize_repository_url(url.clone()).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Url, D::Error>
    where
        D: Deserializer<'de>,
    {
        Url::deserialize(deserializer).map(canonicalize_repository_url)
    }

    fn canonicalize_repository_url(mut url: Url) -> Url {
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty();
        }
        url
    }
}

mod git_oid {
    use git2::Oid;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S>(oid: &Oid, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(oid)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Oid, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

mod optional_git_oid {
    use git2::Oid;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S>(oid: &Option<Oid>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match oid {
            Some(oid) => serializer.collect_str(oid),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Oid>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map(Some).map_err(D::Error::custom)
    }
}

mod package_version {
    use r_description::Version;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S>(version: &Version, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(version)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Version, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relative_path::RelativePathBuf;
    use serde_json::json;

    const SYSREQ_COMMIT: &str = "1111111111111111111111111111111111111111";
    const GIT_COMMIT: &str = "2222222222222222222222222222222222222222";

    fn oid(value: &str) -> git2::Oid {
        value.parse().expect("OID should parse")
    }

    fn url(value: &str) -> url::Url {
        value.parse().expect("URL should parse")
    }

    fn relation(value: &str) -> r_description::Relation {
        value.parse().expect("relation should parse")
    }

    fn package_version(value: &str) -> r_description::Version {
        value.parse().expect("package version should parse")
    }

    fn sample_lockfile() -> Lockfile {
        Lockfile {
            version: LOCKFILE_VERSION,
            revision: LOCKFILE_REVISION,
            r: semver::Version::new(4, 4, 1),
            sysreqs: SystemRequirements {
                db_commit: Some(oid(SYSREQ_COMMIT)),
                rules: BTreeMap::from([(
                    "libcurl".to_string(),
                    BTreeSet::from(["curl".to_string()]),
                )]),
            },
            repos: vec![
                Repository::Rrepo {
                    url: url("https://api.rrepo.org/cran"),
                },
                Repository::CranLike {
                    url: url("https://cran.example/"),
                    archive_support: ArchiveSupport::Available,
                },
                Repository::Git {
                    url: url("https://github.com/example/repository.git"),
                    reference: GitReference::Named {
                        value: "main".to_string(),
                    },
                    commit: oid(GIT_COMMIT),
                    subdirectory: Some(RelativePathBuf::from("packages/example")),
                },
            ],
            requirements: BTreeSet::from([relation("curl (>= 6.0.0)")]),
            packages: BTreeMap::from([(
                "curl".to_string(),
                Package {
                    version: package_version("6.2.3"),
                    repository: url("https://api.rrepo.org/cran"),
                    dependencies: BTreeSet::from([
                        relation("R (>= 4.1.0)"),
                        relation("jsonlite (>= 1.8.0)"),
                        relation("methods"),
                    ]),
                },
            )]),
        }
    }

    fn minimal_lockfile() -> Lockfile {
        Lockfile {
            version: LOCKFILE_VERSION,
            revision: LOCKFILE_REVISION,
            r: semver::Version::new(4, 4, 1),
            sysreqs: SystemRequirements {
                db_commit: Some(oid(SYSREQ_COMMIT)),
                rules: BTreeMap::new(),
            },
            repos: vec![],
            requirements: BTreeSet::new(),
            packages: BTreeMap::new(),
        }
    }

    #[test]
    fn serializes_v5_lockfile_shape() {
        let actual = serde_json::to_value(sample_lockfile()).expect("lockfile should serialize");

        assert_eq!(
            actual,
            json!({
                "version": 5,
                "revision": 0,
                "r": "4.4.1",
                "sysreqs": {
                    "db_commit": SYSREQ_COMMIT,
                    "rules": {
                        "libcurl": ["curl"]
                    }
                },
                "repos": [
                    {
                        "kind": "rrepo",
                        "url": "https://api.rrepo.org/cran"
                    },
                    {
                        "kind": "cran-like",
                        "url": "https://cran.example/",
                        "archive_support": "available"
                    },
                    {
                        "kind": "git",
                        "url": "https://github.com/example/repository.git",
                        "reference": {
                            "type": "named",
                            "value": "main"
                        },
                        "commit": GIT_COMMIT,
                        "subdirectory": "packages/example"
                    }
                ],
                "requirements": ["curl (>= 6.0.0)"],
                "packages": {
                    "curl": {
                        "version": "6.2.3",
                        "repository": "https://api.rrepo.org/cran",
                        "dependencies": [
                            "R (>= 4.1.0)",
                            "jsonlite (>= 1.8.0)",
                            "methods"
                        ]
                    }
                }
            })
        );
    }

    #[test]
    fn round_trips_v5_lockfile() {
        let lockfile = sample_lockfile();
        let json = serde_json::to_string(&lockfile).expect("lockfile should serialize");
        let parsed = serde_json::from_str::<Lockfile>(&json).expect("lockfile should parse");

        assert_eq!(parsed, lockfile);
    }

    #[test]
    fn serializes_empty_required_collections() {
        let actual = serde_json::to_value(minimal_lockfile()).expect("lockfile should serialize");

        assert_eq!(actual["repos"], json!([]));
        assert_eq!(actual["requirements"], json!([]));
        assert_eq!(actual["packages"], json!({}));
        assert_eq!(actual["sysreqs"]["rules"], json!({}));
    }

    #[test]
    fn requires_top_level_fields() {
        let value = serde_json::to_value(minimal_lockfile()).expect("lockfile should serialize");

        for field in [
            "version",
            "revision",
            "r",
            "sysreqs",
            "repos",
            "requirements",
            "packages",
        ] {
            let mut missing = value.clone();
            missing
                .as_object_mut()
                .expect("lockfile should be an object")
                .remove(field);
            assert!(
                serde_json::from_value::<Lockfile>(missing).is_err(),
                "missing {field} should fail"
            );
        }
    }

    #[test]
    fn round_trips_repository_variants_in_order() {
        let repositories = sample_lockfile().repos;
        let json = serde_json::to_string(&repositories).expect("repositories should serialize");
        let parsed =
            serde_json::from_str::<Vec<Repository>>(&json).expect("repositories should parse");

        assert_eq!(parsed, repositories);
    }

    #[test]
    fn round_trips_git_reference_variants() {
        for reference in [
            GitReference::DefaultBranch,
            GitReference::Named {
                value: "refs/tags/v1.0.0".to_string(),
            },
            GitReference::Commit,
        ] {
            let repository = Repository::Git {
                url: url("https://github.com/example/repository.git"),
                reference,
                commit: oid(GIT_COMMIT),
                subdirectory: None,
            };
            let json = serde_json::to_value(&repository).expect("repository should serialize");
            let parsed = serde_json::from_value::<Repository>(json.clone())
                .expect("repository should parse");

            assert_eq!(parsed, repository);
            assert!(json.get("subdirectory").is_none());
        }
    }

    #[test]
    fn round_trips_git_subdirectory() {
        let repository = Repository::Git {
            url: url("https://github.com/example/repository.git"),
            reference: GitReference::DefaultBranch,
            commit: oid(GIT_COMMIT),
            subdirectory: Some(RelativePathBuf::from("packages/example")),
        };

        let json = serde_json::to_value(&repository).expect("repository should serialize");
        let parsed =
            serde_json::from_value::<Repository>(json.clone()).expect("repository should parse");

        assert_eq!(json["subdirectory"], "packages/example");
        assert_eq!(parsed, repository);
    }

    #[test]
    fn canonicalizes_repository_urls() {
        let repository = Repository::Rrepo {
            url: url("https://api.rrepo.org/cran/"),
        };
        let package = Package {
            version: package_version("1.0.0"),
            repository: url("https://api.rrepo.org/cran/"),
            dependencies: BTreeSet::new(),
        };

        let repository_json =
            serde_json::to_value(&repository).expect("repository should serialize");
        let package_json = serde_json::to_value(&package).expect("package should serialize");

        assert_eq!(repository_json["url"], "https://api.rrepo.org/cran");
        assert_eq!(package_json["repository"], "https://api.rrepo.org/cran");
        assert_eq!(
            serde_json::from_value::<Repository>(repository_json).expect("repository should parse"),
            Repository::Rrepo {
                url: url("https://api.rrepo.org/cran")
            }
        );
    }

    #[test]
    fn serializes_inverse_system_requirement_rules() {
        let requirements = SystemRequirements {
            db_commit: Some(oid(SYSREQ_COMMIT)),
            rules: BTreeMap::from([(
                "libcurl".to_string(),
                BTreeSet::from(["httr".to_string(), "curl".to_string()]),
            )]),
        };

        assert_eq!(
            serde_json::to_value(requirements).expect("system requirements should serialize"),
            json!({
                "db_commit": SYSREQ_COMMIT,
                "rules": {
                    "libcurl": ["curl", "httr"]
                }
            })
        );
    }

    #[test]
    fn round_trips_package_version_semantics() {
        for version in ["1.2", "2.5-1", "1.2.3.9000"] {
            let package = Package {
                version: package_version(version),
                repository: url("https://api.rrepo.org/cran"),
                dependencies: BTreeSet::new(),
            };

            let json = serde_json::to_value(&package).expect("package should serialize");
            let parsed =
                serde_json::from_value::<Package>(json.clone()).expect("package should parse");

            assert_eq!(json["version"], version);
            assert_eq!(parsed, package);
        }
    }

    #[test]
    fn round_trips_manifest_and_package_relations() {
        let requirements = BTreeSet::from([
            relation("cli"),
            relation("digest (>= 0.6.37)"),
            relation("jsonlite (== 1.8.9)"),
        ]);
        let dependencies = BTreeSet::from([
            relation("R (>= 4.1.0)"),
            relation("methods"),
            relation("rlang (!= 1.0.0)"),
        ]);
        let mut lockfile = minimal_lockfile();
        lockfile.requirements = requirements.clone();
        lockfile.packages.insert(
            "example".to_string(),
            Package {
                version: package_version("1.0.0"),
                repository: url("https://api.rrepo.org/cran"),
                dependencies: dependencies.clone(),
            },
        );

        let json = serde_json::to_string(&lockfile).expect("lockfile should serialize");
        let parsed = serde_json::from_str::<Lockfile>(&json).expect("lockfile should parse");

        assert_eq!(parsed.requirements, requirements);
        assert_eq!(parsed.packages["example"].dependencies, dependencies);
    }

    #[test]
    fn package_name_exists_only_as_map_key() {
        let json = serde_json::to_value(sample_lockfile()).expect("lockfile should serialize");
        let package = json["packages"]["curl"]
            .as_object()
            .expect("package should be an object");

        assert!(!package.contains_key("package"));
        assert!(!package.contains_key("name"));
    }

    #[test]
    fn round_trips_oid_fields() {
        let lockfile = sample_lockfile();
        let json = serde_json::to_value(&lockfile).expect("lockfile should serialize");
        let parsed = serde_json::from_value::<Lockfile>(json).expect("lockfile should parse");

        assert_eq!(parsed.sysreqs.db_commit, Some(oid(SYSREQ_COMMIT)));
        assert!(matches!(
            &parsed.repos[2],
            Repository::Git { commit, .. } if *commit == oid(GIT_COMMIT)
        ));
    }
}
