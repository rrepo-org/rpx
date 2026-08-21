use miette::Diagnostic;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};
use thiserror::Error;

pub const LOCKFILE_NAME: &str = "rpx.lock";

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
    use crate::repository::parse_repository_url;
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
    use url::Url;

    pub fn serialize<S>(url: &Url, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        url.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Url, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_repository_url(&value).map_err(D::Error::custom)
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

#[derive(Debug, Error, Diagnostic)]
pub enum LockfileReadError {
    #[error("failed to read rpx.lock at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::lockfile_read_failed))]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse rpx.lock at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::lockfile_parse_failed))]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("{} needs to be updated", path.display())]
    #[diagnostic(
        code(rpx::project::lockfile_outdated),
        help("Run `rpx lock` to update it.")
    )]
    OutdatedLockfile { path: PathBuf },

    #[error("{} was created by a newer version of rpx", path.display())]
    #[diagnostic(
        code(rpx::project::lockfile_from_newer_rpx),
        help("Update rpx and try again.")
    )]
    NewerLockfile { path: PathBuf },
}

pub fn read_lockfile(path: &PathBuf) -> Result<Lockfile, LockfileReadError> {
    let path = path.join(LOCKFILE_NAME);
    let contents = fs::read_to_string(&path).map_err(|source| LockfileReadError::Read {
        path: path.clone(),
        source,
    })?;

    let header = serde_json::from_str::<LockfileHeader>(&contents).map_err(|source| {
        LockfileReadError::Parse {
            path: path.clone(),
            source,
        }
    })?;
    if header.version < LOCKFILE_VERSION {
        return Err(LockfileReadError::OutdatedLockfile { path: path });
    }
    if header.version > LOCKFILE_VERSION {
        return Err(LockfileReadError::NewerLockfile { path: path });
    }

    let lockfile = serde_json::from_str::<Lockfile>(&contents)
        .map_err(|source| LockfileReadError::Parse { path: path, source })?;

    Ok(lockfile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use relative_path::RelativePathBuf;
    use serde::de::DeserializeOwned;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    const SYSREQ_COMMIT: &str = "1111111111111111111111111111111111111111";
    const GIT_COMMIT: &str = "2222222222222222222222222222222222222222";
    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let unique = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rpx-lockfile-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn assert_rejected<T: DeserializeOwned>(value: serde_json::Value, field: &str) {
        assert!(
            serde_json::from_value::<T>(value).is_err(),
            "invalid or missing {field} should fail"
        );
    }

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
    fn serializes_current_lockfile_wire_shape() {
        let actual = serde_json::to_value(sample_lockfile()).expect("lockfile should serialize");

        assert_eq!(
            actual,
            json!({
                "version": LOCKFILE_VERSION,
                "revision": LOCKFILE_REVISION,
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
    fn round_trips_current_lockfile() {
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
    fn uses_exact_git_reference_wire_shapes() {
        for (reference, expected) in [
            (
                GitReference::DefaultBranch,
                json!({ "type": "default-branch" }),
            ),
            (
                GitReference::Named {
                    value: "refs/tags/v1.0.0".to_string(),
                },
                json!({ "type": "named", "value": "refs/tags/v1.0.0" }),
            ),
            (GitReference::Commit, json!({ "type": "commit" })),
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

            assert_eq!(json["reference"], expected);
            assert_eq!(parsed, repository);
            assert!(json.get("subdirectory").is_none());
        }

        assert_eq!(
            serde_json::to_value(ArchiveSupport::Unavailable)
                .expect("archive support should serialize"),
            "unavailable"
        );
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
    fn serializes_canonical_repository_urls_and_canonicalizes_deserialization() {
        let repository = Repository::Rrepo {
            url: url("https://api.rrepo.org/cran"),
        };
        let package = Package {
            version: package_version("1.0.0"),
            repository: url("https://api.rrepo.org/cran"),
            dependencies: BTreeSet::new(),
        };

        let repository_json =
            serde_json::to_value(&repository).expect("repository should serialize");
        let package_json = serde_json::to_value(&package).expect("package should serialize");

        assert_eq!(repository_json["url"], "https://api.rrepo.org/cran");
        assert_eq!(package_json["repository"], "https://api.rrepo.org/cran");
        assert_eq!(
            serde_json::from_value::<Repository>(json!({
                "kind": "rrepo",
                "url": "https://api.rrepo.org/cran/"
            }))
            .expect("repository should parse"),
            Repository::Rrepo {
                url: url("https://api.rrepo.org/cran")
            }
        );
        assert_eq!(
            serde_json::from_value::<Package>(json!({
                "version": "1.0.0",
                "repository": "https://api.rrepo.org/cran/",
                "dependencies": []
            }))
            .expect("package should parse")
            .repository,
            url("https://api.rrepo.org/cran")
        );
    }

    #[test]
    fn serializes_system_requirement_rule_package_map() {
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
    fn round_trips_root_requirements_and_locked_dependencies() {
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
    fn round_trips_present_oid_fields() {
        let lockfile = sample_lockfile();
        let json = serde_json::to_value(&lockfile).expect("lockfile should serialize");
        let parsed = serde_json::from_value::<Lockfile>(json).expect("lockfile should parse");

        assert_eq!(parsed.sysreqs.db_commit, Some(oid(SYSREQ_COMMIT)));
        assert!(matches!(
            &parsed.repos[2],
            Repository::Git { commit, .. } if *commit == oid(GIT_COMMIT)
        ));
    }

    #[test]
    fn read_lockfile_reports_missing_file_as_read_error() {
        let directory = TestDirectory::new("missing");

        let error = read_lockfile(&directory.0).expect_err("missing lockfile should fail");

        assert!(matches!(
            &error,
            LockfileReadError::Read { path, source }
                if path == &directory.0.join(LOCKFILE_NAME)
                    && source.kind() == std::io::ErrorKind::NotFound
        ));
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("rpx::project::lockfile_read_failed")
        );
    }

    #[test]
    fn read_lockfile_reports_header_parse_error() {
        let directory = TestDirectory::new("invalid-header");
        fs::write(directory.0.join(LOCKFILE_NAME), r#"{"revision":0}"#)
            .expect("lockfile should be written");

        let error = read_lockfile(&directory.0).expect_err("header should be invalid");

        assert!(matches!(
            &error,
            LockfileReadError::Parse { path, .. }
                if path == &directory.0.join(LOCKFILE_NAME)
        ));
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("rpx::project::lockfile_parse_failed")
        );
    }

    #[test]
    fn read_lockfile_rejects_outdated_version_before_full_schema_parse() {
        let directory = TestDirectory::new("outdated");
        fs::write(
            directory.0.join(LOCKFILE_NAME),
            format!(r#"{{"version":{},"obsolete":true}}"#, LOCKFILE_VERSION - 1),
        )
        .expect("lockfile should be written");

        let error = read_lockfile(&directory.0).expect_err("old lockfile should fail");

        assert!(matches!(
            &error,
            LockfileReadError::OutdatedLockfile { path }
                if path == &directory.0.join(LOCKFILE_NAME)
        ));
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("rpx::project::lockfile_outdated")
        );
    }

    #[test]
    fn read_lockfile_rejects_newer_version_before_full_schema_parse() {
        let directory = TestDirectory::new("newer");
        fs::write(
            directory.0.join(LOCKFILE_NAME),
            format!(r#"{{"version":{},"future":true}}"#, LOCKFILE_VERSION + 1),
        )
        .expect("lockfile should be written");

        let error = read_lockfile(&directory.0).expect_err("new lockfile should fail");

        assert!(matches!(
            &error,
            LockfileReadError::NewerLockfile { path }
                if path == &directory.0.join(LOCKFILE_NAME)
        ));
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("rpx::project::lockfile_from_newer_rpx")
        );
    }

    #[test]
    fn read_lockfile_reports_full_schema_parse_error_for_current_version() {
        let directory = TestDirectory::new("invalid-current");
        fs::write(
            directory.0.join(LOCKFILE_NAME),
            format!(r#"{{"version":{LOCKFILE_VERSION}}}"#),
        )
        .expect("lockfile should be written");

        let error = read_lockfile(&directory.0).expect_err("schema should be incomplete");

        assert!(matches!(
            &error,
            LockfileReadError::Parse { path, .. }
                if path == &directory.0.join(LOCKFILE_NAME)
        ));
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("rpx::project::lockfile_parse_failed")
        );
    }

    #[test]
    fn omits_and_defaults_absent_system_requirements_commit() {
        let requirements = SystemRequirements {
            db_commit: None,
            rules: BTreeMap::new(),
        };

        let json =
            serde_json::to_value(&requirements).expect("system requirements should serialize");
        let parsed = serde_json::from_value::<SystemRequirements>(json!({ "rules": {} }))
            .expect("missing commit should default");

        assert!(json.get("db_commit").is_none());
        assert_eq!(parsed.db_commit, None);
    }

    #[test]
    fn rejects_null_and_malformed_system_requirements_commit() {
        assert_rejected::<SystemRequirements>(
            json!({ "db_commit": null, "rules": {} }),
            "null db_commit",
        );
        assert_rejected::<SystemRequirements>(
            json!({ "db_commit": "not-an-oid", "rules": {} }),
            "malformed db_commit",
        );
    }

    #[test]
    fn rejects_malformed_custom_lockfile_scalars() {
        let lockfile = serde_json::to_value(sample_lockfile()).expect("lockfile should serialize");
        let cases = [
            ("requirement", {
                let mut value = lockfile.clone();
                value["requirements"] = json!(["curl (>= invalid)"]);
                value
            }),
            ("Git commit", {
                let mut value = lockfile.clone();
                value["repos"][2]["commit"] = json!("not-an-oid");
                value
            }),
            ("system requirements commit", {
                let mut value = lockfile.clone();
                value["sysreqs"]["db_commit"] = json!("not-an-oid");
                value
            }),
            ("package version", {
                let mut value = lockfile.clone();
                value["packages"]["curl"]["version"] = json!("invalid");
                value
            }),
            ("repository URL", {
                let mut value = lockfile.clone();
                value["repos"][0]["url"] = json!("not-a-url");
                value
            }),
        ];

        for (field, value) in cases {
            assert_rejected::<Lockfile>(value, field);
        }
    }

    #[test]
    fn requires_nested_lockfile_fields() {
        assert_rejected::<SystemRequirements>(json!({}), "sysreqs.rules");
        assert_rejected::<Repository>(json!({ "kind": "rrepo" }), "rrepo.url");
        assert_rejected::<Repository>(
            json!({ "kind": "cran-like", "archive_support": "available" }),
            "cran-like.url",
        );
        assert_rejected::<Repository>(
            json!({ "kind": "cran-like", "url": "https://example.com" }),
            "cran-like.archive_support",
        );
        assert_rejected::<Repository>(
            json!({
                "kind": "git",
                "reference": { "type": "commit" },
                "commit": GIT_COMMIT
            }),
            "git.url",
        );
        assert_rejected::<Repository>(
            json!({
                "kind": "git",
                "url": "https://example.com/repository.git",
                "commit": GIT_COMMIT
            }),
            "git.reference",
        );
        assert_rejected::<Repository>(
            json!({
                "kind": "git",
                "url": "https://example.com/repository.git",
                "reference": { "type": "commit" }
            }),
            "git.commit",
        );
        assert_rejected::<GitReference>(json!({ "type": "named" }), "named reference value");
        assert_rejected::<Package>(
            json!({
                "repository": "https://example.com",
                "dependencies": []
            }),
            "package.version",
        );
        assert_rejected::<Package>(
            json!({ "version": "1.0.0", "dependencies": [] }),
            "package.repository",
        );
        assert_rejected::<Package>(
            json!({
                "version": "1.0.0",
                "repository": "https://example.com"
            }),
            "package.dependencies",
        );
    }

    #[test]
    fn repository_url_returns_each_variant_url() {
        for (repository, expected) in [
            (
                Repository::Rrepo {
                    url: url("https://rrepo.example/cran"),
                },
                url("https://rrepo.example/cran"),
            ),
            (
                Repository::CranLike {
                    url: url("https://cran.example"),
                    archive_support: ArchiveSupport::Unavailable,
                },
                url("https://cran.example"),
            ),
            (
                Repository::Git {
                    url: url("https://git.example/repository.git"),
                    reference: GitReference::Commit,
                    commit: oid(GIT_COMMIT),
                    subdirectory: None,
                },
                url("https://git.example/repository.git"),
            ),
        ] {
            assert_eq!(repository.url(), &expected);
        }
    }
}
