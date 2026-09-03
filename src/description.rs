use miette::{Diagnostic, NamedSource, SourceSpan};
use r_description::{
    CollectionEditError, CollectionResult, Description, EditError, FieldName, LogicalValue,
};
use r_metadata::{
    PositionedRelationParseError, Relation, Remote, RemoteSource, RequirementVersion, Url, Version,
    VersionRequirement,
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;

use crate::repository::{
    GitRepository, PackageRepository, RepositoryError, built_in_repository_url,
    parse_repository_url,
};

pub const DESCRIPTION_NAME: &str = "DESCRIPTION";
pub const BASE_REPOSITORY_FIELD: &str = "Config/rpx/base-repository";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DependencyField {
    Depends,
    #[default]
    Imports,
    LinkingTo,
    Suggests,
}

#[derive(Clone, Debug, Error, Diagnostic)]
#[error("failed to parse DESCRIPTION ({count} errors)")]
#[diagnostic(code(rpx::description::parse_failed))]
pub struct DescriptionParseError {
    count: usize,

    #[source_code]
    source_code: NamedSource<String>,

    #[related]
    issues: Vec<DescriptionParseIssue>,
}

#[derive(Clone, Debug, Error, Diagnostic)]
#[error("failed to normalize DESCRIPTION ({count} errors)")]
#[diagnostic(code(rpx::description::normalization_failed))]
pub struct DescriptionNormalizationError {
    count: usize,

    #[source_code]
    source_code: NamedSource<String>,

    #[related]
    issues: Vec<DescriptionParseIssue>,
}

impl DescriptionParseError {
    pub fn new(
        source_name: impl Into<String>,
        source: String,
        issues: Vec<DescriptionParseIssue>,
    ) -> Self {
        let source_name = source_name.into();
        Self {
            count: issues.len(),
            source_code: NamedSource::new(source_name, source),
            issues,
        }
    }

    pub fn messages(&self) -> Vec<String> {
        self.issues.iter().map(ToString::to_string).collect()
    }
}

#[derive(Clone, Debug, Error, Diagnostic)]
pub enum DescriptionParseIssue {
    #[error("{error}")]
    Positioned {
        error: Arc<dyn std::error::Error + Send + Sync>,

        #[label("{error}")]
        span: SourceSpan,
    },

    #[error("{error}")]
    Unpositioned {
        error: Arc<dyn std::error::Error + Send + Sync>,
    },

    #[error("{message}")]
    Missing { message: String },
}

impl DescriptionParseIssue {
    pub fn positioned(
        error: impl std::error::Error + Send + Sync + 'static,
        span: impl Into<SourceSpan>,
    ) -> Self {
        Self::Positioned {
            error: Arc::new(error),
            span: span.into(),
        }
    }

    pub fn unpositioned(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Unpositioned {
            error: Arc::new(error),
        }
    }

    pub fn missing(message: impl Into<String>) -> Self {
        Self::Missing {
            message: message.into(),
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
pub enum DescriptionReadError {
    #[error("failed to read DESCRIPTION at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::description_read_failed))]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Parse(#[from] DescriptionParseError),
}

pub fn read_description(path: &PathBuf) -> Result<Description, DescriptionReadError> {
    let path = path.join(DESCRIPTION_NAME);
    let contents = fs::read_to_string(&path).map_err(|source| DescriptionReadError::Read {
        path: path.clone(),
        source,
    })?;
    let description = Description::parse(&contents);
    let issues = description
        .diagnostics()
        .iter()
        .cloned()
        .map(|issue| {
            let start = issue.span().start;
            let end = issue.span().end;
            DescriptionParseIssue::positioned(std::io::Error::other(issue.message()), start..end)
        })
        .collect::<Vec<_>>();
    if !issues.is_empty() {
        return Err(
            DescriptionParseError::new(path.display().to_string(), contents, issues).into(),
        );
    }

    Ok(description)
}

pub fn normalize_description(
    path: &Path,
    description: &Description,
) -> Result<Description, DescriptionNormalizationError> {
    let source = description.to_string();
    description.normalize().map_err(|error| {
        let issues = error
            .into_diagnostics()
            .into_iter()
            .map(|diagnostic| {
                let span = diagnostic.span();
                DescriptionParseIssue::positioned(
                    std::io::Error::other(format!(
                        "{}: {}",
                        diagnostic.code(),
                        diagnostic.message()
                    )),
                    span.start..span.end,
                )
            })
            .collect::<Vec<_>>();
        DescriptionNormalizationError {
            count: issues.len(),
            source_code: NamedSource::new(
                path.join(DESCRIPTION_NAME).display().to_string(),
                source,
            ),
            issues,
        }
    })
}

#[derive(Debug, Error, Diagnostic)]
pub enum NamespaceWriteError {
    #[error("failed to write NAMESPACE at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::namespace_write_failed))]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn write_namespace_if_missing(path: &Path) -> Result<(), NamespaceWriteError> {
    let path = path.join("NAMESPACE");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(NamespaceWriteError::Write { path, source }),
    }
}

pub fn root_package(
    path: &Path,
    description: &Description,
) -> Result<(String, Version), DescriptionParseError> {
    description_identity(
        path.join(DESCRIPTION_NAME).display().to_string(),
        description,
    )
}

pub(crate) fn description_identity(
    source_name: impl Into<String>,
    description: &Description,
) -> Result<(String, Version), DescriptionParseError> {
    let package = description.package().map(|value| value.as_str().to_owned());
    let version = description.version_parsed();
    let has_multiple_records = description.records().nth(1).is_some();
    let mut issues = Vec::new();

    issues.extend(
        description
            .validate()
            .into_issues()
            .into_iter()
            .filter(|issue| {
                issue.code().starts_with("syntax.")
                    || (issue.code() == "record-count" && has_multiple_records)
                    || (issue.code() == "invalid-package-name"
                        && package
                            .as_deref()
                            .is_some_and(|package| !package.is_empty()))
            })
            .map(|issue| {
                let span = issue.span();
                DescriptionParseIssue::positioned(
                    std::io::Error::other(format!("{}: {}", issue.code(), issue.message())),
                    span.start..span.end,
                )
            }),
    );
    match package.as_deref() {
        None => issues.push(DescriptionParseIssue::missing("Package field is missing")),
        Some("") => issues.push(DescriptionParseIssue::missing("Package field is empty")),
        Some(_) => {}
    }
    match &version {
        None => issues.push(DescriptionParseIssue::missing("Version field is missing")),
        Some(Ok(version)) if version.as_str().is_empty() => {
            issues.push(DescriptionParseIssue::missing("Version field is empty"));
        }
        Some(Err(error)) => {
            let field = description
                .field("Version")
                .expect("parsed Version has a field");
            let value = field.value();
            let range = value.source_range();
            let span = error.span().map_or(range.start..range.end, |span| {
                range.start + span.start()..range.start + span.end()
            });
            issues.push(DescriptionParseIssue::positioned(error.clone(), span));
        }
        Some(Ok(_)) => {}
    }
    if issues.is_empty() {
        Ok((
            package.expect("validated Package"),
            version.expect("validated Version").expect("valid Version"),
        ))
    } else {
        Err(DescriptionParseError::new(
            source_name,
            description.to_string(),
            issues,
        ))
    }
}

#[derive(Debug, Error, Diagnostic)]
pub enum DependencyMutationError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Description(#[from] DescriptionParseError),

    #[error("failed to update dependency metadata: {source}")]
    #[diagnostic(code(rpx::description::dependency_update_failed))]
    Mutation {
        #[from]
        source: CollectionEditError,
    },
}

pub fn required_dependencies(
    source_name: impl Into<String>,
    description: &Description,
) -> Result<BTreeSet<Relation>, DescriptionParseError> {
    dependencies_from_fields(
        source_name,
        description,
        [
            ("Imports", description.imports_parsed()),
            ("Depends", description.depends_parsed()),
            ("LinkingTo", description.linking_to_parsed()),
        ],
    )
    .map(|dependencies| {
        dependencies
            .into_iter()
            .filter(|relation| relation.package() != "R")
            .collect()
    })
}

pub fn project_dependencies(
    project_path: &Path,
    description: &Description,
) -> Result<BTreeSet<Relation>, DescriptionParseError> {
    dependencies_from_fields(
        project_path.join(DESCRIPTION_NAME).display().to_string(),
        description,
        [
            ("Imports", description.imports_parsed()),
            ("Depends", description.depends_parsed()),
            ("LinkingTo", description.linking_to_parsed()),
            ("Suggests", description.suggests_parsed()),
        ],
    )
    .map(|dependencies| {
        dependencies
            .into_iter()
            .filter(|relation| relation.package() != "R")
            .collect()
    })
}

pub(crate) fn dependencies_from_fields(
    source_name: impl Into<String>,
    description: &Description,
    fields: impl IntoIterator<
        Item = (
            &'static str,
            CollectionResult<Relation, PositionedRelationParseError>,
        ),
    >,
) -> Result<BTreeSet<Relation>, DescriptionParseError> {
    let fields = fields.into_iter().collect::<Vec<_>>();
    let dependencies = fields
        .iter()
        .flat_map(|(_, parsed)| parsed.values().cloned())
        .collect();
    let issues = fields
        .iter()
        .flat_map(|(field, parsed)| {
            parsed.issues().iter().map(move |issue| {
                DescriptionParseIssue::positioned(
                    std::io::Error::other(format!("{field}: {}", issue.error)),
                    issue.field_span.start..issue.field_span.end,
                )
            })
        })
        .chain(fields.iter().flat_map(|(field, parsed)| {
            parsed.entries().iter().filter_map(move |entry| {
                let uses_revision = matches!(
                    entry.value.requirement(),
                    VersionRequirement::Equal(RequirementVersion::Revision(_))
                        | VersionRequirement::NotEqual(RequirementVersion::Revision(_))
                        | VersionRequirement::GreaterThan(RequirementVersion::Revision(_))
                        | VersionRequirement::GreaterThanEqual(RequirementVersion::Revision(_))
                        | VersionRequirement::LessThan(RequirementVersion::Revision(_))
                        | VersionRequirement::LessThanEqual(RequirementVersion::Revision(_))
                );
                (entry.value.package() != "R" && uses_revision).then(|| {
                    DescriptionParseIssue::positioned(
                        std::io::Error::other(format!(
                            "{field}: revision requirements are only valid for R"
                        )),
                        entry.field_span.start..entry.field_span.end,
                    )
                })
            })
        }))
        .collect::<Vec<_>>();
    if issues.is_empty() {
        Ok(dependencies)
    } else {
        Err(DescriptionParseError::new(
            source_name,
            description.to_string(),
            issues,
        ))
    }
}

pub fn add_dependencies(
    path: &Path,
    description: &mut Description,
    dependencies: &BTreeSet<Relation>,
    field: DependencyField,
) -> Result<(), DependencyMutationError> {
    if dependencies.is_empty() {
        return Ok(());
    }

    let source_name = path.join(DESCRIPTION_NAME).display().to_string();
    let mut depends = dependencies_from_fields(
        &source_name,
        description,
        [("Depends", description.depends_parsed())],
    )?;
    let mut imports = dependencies_from_fields(
        &source_name,
        description,
        [("Imports", description.imports_parsed())],
    )?;
    let mut linking_to = dependencies_from_fields(
        &source_name,
        description,
        [("LinkingTo", description.linking_to_parsed())],
    )?;
    let mut suggests = dependencies_from_fields(
        &source_name,
        description,
        [("Suggests", description.suggests_parsed())],
    )?;

    let added_packages = dependencies
        .iter()
        .map(|dependency| dependency.package().to_string())
        .collect::<BTreeSet<_>>();

    depends.retain(|dependency| !added_packages.contains(dependency.package()));
    imports.retain(|dependency| !added_packages.contains(dependency.package()));
    linking_to.retain(|dependency| !added_packages.contains(dependency.package()));
    suggests.retain(|dependency| !added_packages.contains(dependency.package()));

    match field {
        DependencyField::Depends => depends.extend(dependencies.iter().cloned()),
        DependencyField::Imports => imports.extend(dependencies.iter().cloned()),
        DependencyField::LinkingTo => linking_to.extend(dependencies.iter().cloned()),
        DependencyField::Suggests => suggests.extend(dependencies.iter().cloned()),
    }

    let updated = description
        .set_depends(&depends)
        .and_then(|description| description.set_imports(&imports))
        .and_then(|description| description.set_linking_to(&linking_to))
        .and_then(|description| description.set_suggests(&suggests))?;
    *description = updated;

    Ok(())
}

pub fn remove_dependencies(
    path: &Path,
    description: &mut Description,
    packages: &BTreeSet<String>,
) -> Result<(), DependencyMutationError> {
    if packages.is_empty() {
        return Ok(());
    }

    let source_name = path.join(DESCRIPTION_NAME).display().to_string();
    let mut depends = dependencies_from_fields(
        &source_name,
        description,
        [("Depends", description.depends_parsed())],
    )?;
    let mut imports = dependencies_from_fields(
        &source_name,
        description,
        [("Imports", description.imports_parsed())],
    )?;
    let mut linking_to = dependencies_from_fields(
        &source_name,
        description,
        [("LinkingTo", description.linking_to_parsed())],
    )?;
    let mut suggests = dependencies_from_fields(
        &source_name,
        description,
        [("Suggests", description.suggests_parsed())],
    )?;

    depends.retain(|dependency| !packages.contains(dependency.package()));
    imports.retain(|dependency| !packages.contains(dependency.package()));
    linking_to.retain(|dependency| !packages.contains(dependency.package()));
    suggests.retain(|dependency| !packages.contains(dependency.package()));

    let updated = description
        .set_depends(&depends)
        .and_then(|description| description.set_imports(&imports))
        .and_then(|description| description.set_linking_to(&linking_to))
        .and_then(|description| description.set_suggests(&suggests))?;
    *description = updated;

    Ok(())
}

#[derive(Debug, Error)]
pub enum RemoteValidationError {
    #[error("unsupported remote `{remote}` of type `{kind}`")]
    Unsupported { remote: Remote, kind: String },

    #[error("duplicate remote package `{package}`")]
    DuplicatePackage { package: String },
}

pub(crate) fn remotes(
    path: &Path,
    description: &Description,
) -> Result<Vec<Remote>, DescriptionParseError> {
    let parsed = description.remotes_parsed();
    let remotes = parsed
        .entries()
        .iter()
        .map(|entry| entry.value.clone())
        .collect::<Vec<_>>();
    if !parsed.issues().is_empty() {
        let issues = parsed
            .issues()
            .iter()
            .map(|issue| {
                DescriptionParseIssue::positioned(
                    issue.error.clone(),
                    issue.field_span.start..issue.field_span.end,
                )
            })
            .collect();
        return Err(DescriptionParseError::new(
            path.join(DESCRIPTION_NAME).display().to_string(),
            description.to_string(),
            issues,
        ));
    }

    let unsupported = remotes.iter().filter_map(|remote| {
        let kind = match &remote.source {
            RemoteSource::GitHub(_)
            | RemoteSource::GitLab(_)
            | RemoteSource::Bitbucket(_)
            | RemoteSource::Git(_) => None,
            RemoteSource::Unspecified(_) => Some("unspecified".to_string()),
            RemoteSource::Cran(_) => Some("cran".to_string()),
            RemoteSource::Url(_) => Some("url".to_string()),
            RemoteSource::Local(_) => Some("local".to_string()),
            RemoteSource::Svn(_) => Some("svn".to_string()),
            RemoteSource::Bioconductor(_) => Some("bioconductor".to_string()),
            RemoteSource::Unknown(remote) => Some(remote.kind.clone()),
        };

        kind.map(|kind| {
            DescriptionParseIssue::unpositioned(RemoteValidationError::Unsupported {
                remote: remote.clone(),
                kind,
            })
        })
    });

    let duplicates = remotes
        .iter()
        .scan(BTreeSet::new(), |packages, remote| {
            Some(remote.package.as_ref().and_then(|package| {
                (!packages.insert(package.clone())).then(|| {
                    DescriptionParseIssue::unpositioned(RemoteValidationError::DuplicatePackage {
                        package: package.clone(),
                    })
                })
            }))
        })
        .flatten();

    let issues = unsupported.chain(duplicates).collect::<Vec<_>>();

    if issues.is_empty() {
        Ok(remotes)
    } else {
        Err(DescriptionParseError::new(
            path.join(DESCRIPTION_NAME).display().to_string(),
            description.to_string(),
            issues,
        ))
    }
}

pub(crate) fn additional_repositories(
    path: &Path,
    description: &Description,
) -> Result<Vec<Url>, DescriptionParseError> {
    let parsed = description.additional_repositories_parsed();
    if !parsed.issues().is_empty() {
        let issues = parsed
            .issues()
            .iter()
            .map(|issue| {
                DescriptionParseIssue::positioned(
                    issue.error.clone(),
                    issue.field_span.start..issue.field_span.end,
                )
            })
            .collect();
        return Err(DescriptionParseError::new(
            path.join(DESCRIPTION_NAME).display().to_string(),
            description.to_string(),
            issues,
        ));
    }
    Ok(parsed
        .entries()
        .iter()
        .map(|entry| {
            let mut repository = entry.value.clone();
            if let Ok(mut segments) = repository.path_segments_mut() {
                segments.pop_if_empty();
            }
            repository
        })
        .collect())
}

pub fn base_repository(
    path: &Path,
    description: &Description,
) -> Result<Option<Url>, DescriptionParseError> {
    let Some(value) = description
        .field(BASE_REPOSITORY_FIELD)
        .map(|field| field.value())
    else {
        return Ok(None);
    };
    parse_repository_url(value.as_str())
        .map(Some)
        .map_err(|source| {
            let range = value.source_range();
            DescriptionParseError::new(
                path.join(DESCRIPTION_NAME).display().to_string(),
                description.to_string(),
                vec![DescriptionParseIssue::positioned(
                    source,
                    range.start..range.end,
                )],
            )
        })
}

pub fn set_base_repository(
    description: &mut Description,
    repository: &Url,
) -> Result<(), EditError> {
    let name = FieldName::new(BASE_REPOSITORY_FIELD).expect("constant field name is valid");
    let value = LogicalValue::new(repository.as_str()).expect("URL is a valid DCF value");
    *description = description.set_field(&name, &value)?;
    Ok(())
}

pub fn reset_base_repository(description: &mut Description) {
    if description.field(BASE_REPOSITORY_FIELD).is_some() {
        *description = description
            .remove_all(BASE_REPOSITORY_FIELD)
            .expect("field exists");
    }
}

#[derive(Debug, Error, Diagnostic)]
pub enum RepositoryMutationError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Description(#[from] DescriptionParseError),

    #[error("failed to update repository metadata: {source}")]
    #[diagnostic(code(rpx::description::repository_metadata_update_failed))]
    Mutation {
        #[from]
        source: CollectionEditError,
    },
}

pub fn add_additional_repository(
    path: &Path,
    description: &mut Description,
    repository: Url,
) -> Result<bool, RepositoryMutationError> {
    let mut repositories = additional_repositories(path, description)?;
    if repositories.contains(&repository) {
        return Ok(false);
    }

    repositories.push(repository);
    *description = description.set_additional_repositories(repositories)?;
    Ok(true)
}

pub fn remove_additional_repository(
    path: &Path,
    description: &mut Description,
    repository: &Url,
) -> Result<bool, RepositoryMutationError> {
    let mut repositories = additional_repositories(path, description)?;
    let previous_len = repositories.len();
    repositories.retain(|existing| existing != repository);
    if repositories.len() == previous_len {
        return Ok(false);
    }

    *description = description.set_additional_repositories(repositories)?;
    Ok(true)
}

pub fn add_remote_repository(
    path: &Path,
    description: &mut Description,
    remote: Remote,
) -> Result<bool, RepositoryMutationError> {
    let mut configured = remotes(path, description)?;
    if configured.contains(&remote) {
        return Ok(false);
    }

    configured.push(remote);
    let updated = description.set_remotes(configured)?;
    remotes(path, &updated)?;
    *description = updated;
    Ok(true)
}

pub fn remove_remote_repository(
    path: &Path,
    description: &mut Description,
    remote: &Remote,
) -> Result<bool, RepositoryMutationError> {
    let mut configured = remotes(path, description)?;
    let previous_len = configured.len();
    configured.retain(|existing| existing != remote);
    if configured.len() == previous_len {
        return Ok(false);
    }

    let updated = description.set_remotes(configured)?;
    remotes(path, &updated)?;
    *description = updated;
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfiguredRepository {
    Base(Url),
    Git(Remote),
    Additional(Url),
}

pub fn configured_repositories(
    path: &Path,
    description: &Description,
) -> Result<Vec<ConfiguredRepository>, DescriptionParseError> {
    let configured_base = base_repository(path, description);
    let configured_remotes = remotes(path, description);
    let configured_additional = additional_repositories(path, description);
    let mut issues = Vec::new();

    let configured_base = match configured_base {
        Ok(repository) => repository,
        Err(DescriptionParseError {
            issues: base_issues,
            ..
        }) => {
            issues.extend(base_issues);
            None
        }
    };
    let configured_remotes = match configured_remotes {
        Ok(remotes) => remotes,
        Err(DescriptionParseError {
            issues: remote_issues,
            ..
        }) => {
            issues.extend(remote_issues);
            Vec::new()
        }
    };
    let configured_additional = match configured_additional {
        Ok(repositories) => repositories,
        Err(DescriptionParseError {
            issues: additional_issues,
            ..
        }) => {
            issues.extend(additional_issues);
            Vec::new()
        }
    };

    if !issues.is_empty() {
        return Err(DescriptionParseError::new(
            path.join(DESCRIPTION_NAME).display().to_string(),
            description.to_string(),
            issues,
        ));
    }

    let base = configured_base.unwrap_or_else(|| built_in_repository_url().clone());

    Ok(std::iter::once(ConfiguredRepository::Base(base))
        .chain(
            configured_remotes
                .into_iter()
                .map(ConfiguredRepository::Git),
        )
        .chain(
            configured_additional
                .into_iter()
                .map(ConfiguredRepository::Additional),
        )
        .collect())
}

#[derive(Debug, Error, Diagnostic)]
pub enum RepositoriesFromDescriptionError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Configuration(#[from] DescriptionParseError),

    #[error("failed to configure {kind} repository: {source}")]
    #[diagnostic(code(rpx::description::repository_configuration_failed))]
    Repository {
        kind: &'static str,
        #[source]
        source: RepositoryError,
    },
}

pub async fn repositories_from_description(
    path: &Path,
    description: &Description,
) -> Result<Vec<Arc<dyn PackageRepository>>, RepositoriesFromDescriptionError> {
    futures_util::future::join_all(configured_repositories(path, description)?.into_iter().map(
        |repository| async move {
            match repository {
                ConfiguredRepository::Base(url) => <dyn PackageRepository>::from_url(url)
                    .await
                    .map_err(|source| RepositoriesFromDescriptionError::Repository {
                        kind: "base",
                        source,
                    }),
                ConfiguredRepository::Git(remote) => GitRepository::new(remote)
                    .map(|repository| Arc::new(repository) as Arc<dyn PackageRepository>)
                    .map_err(|source| RepositoriesFromDescriptionError::Repository {
                        kind: "Git",
                        source,
                    }),
                ConfiguredRepository::Additional(url) => <dyn PackageRepository>::from_url(url)
                    .await
                    .map_err(|source| RepositoriesFromDescriptionError::Repository {
                        kind: "additional",
                        source,
                    }),
            }
        },
    ))
    .await
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let unique = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "rpx-description-{name}-{}-{unique}",
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

    fn relation_strings<E>(relations: r_description::CollectionResult<Relation, E>) -> Vec<String> {
        relations.values().map(ToString::to_string).collect()
    }

    fn relation_set(relations: &[&str]) -> BTreeSet<Relation> {
        relations
            .iter()
            .map(|relation| relation.parse().expect("relation should parse"))
            .collect()
    }

    #[test]
    fn reads_and_writes_description_without_caching_failures() {
        let directory = TestDirectory::new("read-write");
        let path = directory.0.join(DESCRIPTION_NAME);
        fs::write(
            &path,
            "Package: project\nVersion: 1.0.0\nthis line is malformed\n",
        )
        .expect("malformed DESCRIPTION should be written");

        assert!(matches!(
            read_description(&directory.0),
            Err(DescriptionReadError::Parse(_))
        ));

        let expected = Description::parse("Package: project\nVersion: 2.0.0\nImports: cli\n");
        fs::write(&path, expected.to_string()).expect("DESCRIPTION should be written");
        let actual = read_description(&directory.0).expect("DESCRIPTION should be reread");

        assert_eq!(actual.to_string(), expected.to_string());
    }

    #[test]
    fn normalizes_description_and_preserves_repository_priority() {
        let description = Description::parse(
            "Version: 1.0.0\nAdditional_repositories: https://z.example/repo\nImports: zed\nPackage: project\nRemotes: github::z/repo, cran::alpha\nImports: alpha, AzureAuth, zed\nAdditional_repositories: https://a.example/repo, https://z.example/repo\nRemotes: github::z/repo\n",
        );

        let normalized = normalize_description(Path::new("."), &description)
            .expect("DESCRIPTION should normalize");

        assert_eq!(
            normalized.to_string(),
            "Package: project\nVersion: 1.0.0\nImports:\n    alpha,\n    AzureAuth,\n    zed\nRemotes:\n    github::z/repo,\n    cran::alpha\nAdditional_repositories:\n    https://z.example/repo,\n    https://a.example/repo\n"
        );
        assert_eq!(
            normalize_description(Path::new("."), &normalized)
                .expect("normalized DESCRIPTION should normalize again"),
            normalized
        );
    }

    #[test]
    fn reports_positioned_normalization_diagnostics() {
        let description = Description::parse(
            "Package: project\nVersion: 1.0.0\nURL: https://example.com, not-a-url\n",
        );

        let error = normalize_description(Path::new("."), &description)
            .expect_err("invalid URL should prevent normalization");

        assert_eq!(error.count, 1);
        assert!(matches!(
            error.issues[0],
            DescriptionParseIssue::Positioned { .. }
        ));
        assert!(error.issues[0].to_string().contains("invalid-collection"));
    }

    #[test]
    fn creates_namespace_only_when_missing() {
        let directory = TestDirectory::new("namespace");
        let namespace = directory.0.join("NAMESPACE");

        write_namespace_if_missing(&directory.0).expect("NAMESPACE should be created");
        fs::write(&namespace, "export(example)\n").expect("NAMESPACE should be populated");
        write_namespace_if_missing(&directory.0).expect("existing NAMESPACE should be accepted");

        assert_eq!(
            fs::read_to_string(namespace).expect("NAMESPACE should be readable"),
            "export(example)\n"
        );
    }

    #[test]
    fn parses_root_package_and_version() {
        let description = Description::parse("Package: project\nVersion: 1.2.3\n");

        let (package, version) =
            root_package(Path::new("."), &description).expect("root should parse");

        assert_eq!(package, "project");
        assert_eq!(version.to_string(), "1.2.3");
    }

    #[test]
    fn reports_all_invalid_root_fields() {
        let missing = Description::parse("");
        let error = root_package(Path::new("."), &missing).expect_err("root should be invalid");
        assert_eq!(error.count, 2);

        let invalid = Description::parse("Package: one\nPackage: two\nVersion: invalid\n");
        let error = root_package(Path::new("."), &invalid).expect_err("root should be invalid");
        assert_eq!(error.count, 1);
    }

    #[test]
    fn reports_positioned_invalid_package_identity_fields() {
        let description = Description::parse("Package: _bad\nVersion: nope\n");

        let error = root_package(Path::new("."), &description)
            .expect_err("invalid package identity should be rejected");

        assert_eq!(error.count, 2);
        assert!(
            error
                .issues
                .iter()
                .all(|issue| matches!(issue, DescriptionParseIssue::Positioned { .. }))
        );
    }

    #[test]
    fn rejects_empty_root_package_and_version() {
        let description = Description::parse("Package: \nVersion: \n");
        let error = root_package(Path::new("."), &description)
            .expect_err("empty root identity fields should be rejected");

        assert_eq!(error.count, 2);
    }

    #[test]
    fn collects_project_dependencies_with_current_field_semantics() {
        let description = Description::parse(
            "Package: project\nVersion: 1.0.0\nDepends: R (>= 4.2), jsonlite (== 1.8.9)\nImports: cli (>= 3.6.0), digest\nLinkingTo: cpp11\nSuggests: testthat (>= 3.0.0)\nEnhances: shiny\n",
        );

        assert_eq!(
            project_dependencies(Path::new("."), &description)
                .expect("project dependencies should parse")
                .into_iter()
                .map(|relation| relation.to_string())
                .collect::<Vec<_>>(),
            [
                "cli (>= 3.6.0)",
                "cpp11",
                "digest",
                "jsonlite (== 1.8.9)",
                "testthat (>= 3.0.0)",
            ]
        );
        assert_eq!(
            required_dependencies("DESCRIPTION", &description)
                .expect("required dependencies should parse")
                .into_iter()
                .map(|relation| relation.to_string())
                .collect::<Vec<_>>(),
            ["cli (>= 3.6.0)", "cpp11", "digest", "jsonlite (== 1.8.9)",]
        );
    }

    #[test]
    fn rejects_malformed_project_dependency_fields_together() {
        let description = Description::parse(
            "Package: project\nVersion: 1.0.0\nImports: cli (>= invalid)\nSuggests: testthat (< invalid)\n",
        );

        let error = project_dependencies(Path::new("."), &description)
            .expect_err("dependencies should be invalid");

        assert_eq!(error.count, 2);
    }

    #[test]
    fn permits_r_revisions_and_rejects_package_revision_requirements() {
        let r = Description::parse("Package: project\nVersion: 1.0.0\nDepends: R (>= r123)\n");
        assert!(
            required_dependencies("DESCRIPTION", &r)
                .expect("R revision should be accepted")
                .is_empty()
        );

        let package =
            Description::parse("Package: project\nVersion: 1.0.0\nImports: example (>= r123)\n");
        let error = required_dependencies("DESCRIPTION", &package)
            .expect_err("package revision should be rejected");
        assert!(
            error
                .messages()
                .iter()
                .any(|message| message.contains("revision requirements are only valid for R"))
        );
    }

    #[test]
    fn add_moves_packages_to_selected_field_and_leaves_enhances_untouched() {
        for (field, selected_index) in [
            (DependencyField::Depends, 0),
            (DependencyField::Imports, 1),
            (DependencyField::LinkingTo, 2),
            (DependencyField::Suggests, 3),
        ] {
            let mut description = Description::parse(
                "Package: project\nVersion: 1.0.0\nDepends: R (>= 4.2), dplyr (>= 1.0.0), keepDepends\nImports: dplyr (< 2.0.0), keepImports\nLinkingTo: dplyr, keepLinking\nSuggests: dplyr, keepSuggests\nEnhances: dplyr, keepEnhances\n",
            );

            add_dependencies(
                Path::new("."),
                &mut description,
                &relation_set(&["dplyr (== 1.1.0)"]),
                field,
            )
            .expect("dependency should be added");

            let managed_fields = [
                relation_strings(description.depends_parsed()),
                relation_strings(description.imports_parsed()),
                relation_strings(description.linking_to_parsed()),
                relation_strings(description.suggests_parsed()),
            ];
            assert!(managed_fields[selected_index].contains(&"dplyr (== 1.1.0)".to_string()));
            assert!(managed_fields.iter().enumerate().all(|(index, relations)| {
                index == selected_index
                    || !relations
                        .iter()
                        .any(|relation| relation.starts_with("dplyr"))
            }));
            assert!(managed_fields[0].contains(&"R (>= 4.2)".to_string()));
            assert_eq!(
                relation_strings(description.enhances_parsed()),
                ["dplyr", "keepEnhances"]
            );
        }
    }

    #[test]
    fn add_semantically_deduplicates_every_rewritten_field() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nDepends: alpha, alpha\nImports: beta (>= 1.0), beta (>= 1.0.0), zoo\nLinkingTo: gamma, gamma\nSuggests: delta, delta\n",
        );

        add_dependencies(
            Path::new("."),
            &mut description,
            &relation_set(&["askpass", "cli"]),
            DependencyField::Imports,
        )
        .expect("dependencies should be added");

        assert_eq!(relation_strings(description.depends_parsed()), ["alpha"]);
        assert_eq!(
            relation_strings(description.imports_parsed()),
            ["askpass", "beta (>= 1.0.0)", "cli", "zoo"]
        );
        assert_eq!(relation_strings(description.linking_to_parsed()), ["gamma"]);
        assert_eq!(relation_strings(description.suggests_parsed()), ["delta"]);
        assert_eq!(
            description.to_string(),
            "Package: project\nVersion: 1.0.0\nDepends:\n    alpha\nImports:\n    askpass,\n    beta (>= 1.0.0),\n    cli,\n    zoo\nLinkingTo:\n    gamma\nSuggests:\n    delta\n"
        );
    }

    #[test]
    fn add_preserves_distinct_requirements_for_the_same_package() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nImports: cli (>= 1.0.0), cli (< 2.0.0)\n",
        );

        add_dependencies(
            Path::new("."),
            &mut description,
            &relation_set(&["digest"]),
            DependencyField::Imports,
        )
        .expect("dependency should be added");

        assert_eq!(
            relation_strings(description.imports_parsed()),
            ["cli (< 2.0.0)", "cli (>= 1.0.0)", "digest"]
        );
    }

    #[test]
    fn add_is_transactional_when_any_dependency_field_is_invalid() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nDepends: cli\nImports: digest (>= invalid)\n",
        );
        let original = description.to_string();

        assert!(
            add_dependencies(
                Path::new("."),
                &mut description,
                &relation_set(&["jsonlite"]),
                DependencyField::Imports,
            )
            .is_err()
        );
        assert_eq!(description.to_string(), original);
    }

    #[test]
    fn add_reports_edit_failures_as_mutation_errors() {
        let mut description = Description::parse("");

        let error = add_dependencies(
            Path::new("."),
            &mut description,
            &relation_set(&["jsonlite"]),
            DependencyField::Imports,
        )
        .expect_err("a missing record cannot be edited");

        assert!(matches!(error, DependencyMutationError::Mutation { .. }));
    }

    #[test]
    fn empty_add_does_not_rewrite_existing_fields() {
        let mut description =
            Description::parse("Package: project\nVersion: 1.0.0\nImports: cli, cli\n");
        let original = description.to_string();

        add_dependencies(
            Path::new("."),
            &mut description,
            &BTreeSet::new(),
            DependencyField::Suggests,
        )
        .expect("empty add should succeed");

        assert_eq!(description.to_string(), original);
    }

    #[test]
    fn remove_normalizes_managed_fields_and_leaves_enhances_untouched() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nDepends: R (>= 4.2), removeMe, keepDepends, keepDepends\nImports: removeMe, keepImports, keepImports\nLinkingTo: removeMe, keepLinking, keepLinking\nSuggests: removeMe, keepSuggests, keepSuggests\nEnhances: removeMe, keepEnhances, keepEnhances\n",
        );

        remove_dependencies(
            Path::new("."),
            &mut description,
            &BTreeSet::from(["removeMe".to_string()]),
        )
        .expect("dependency should be removed");

        assert_eq!(
            relation_strings(description.depends_parsed()),
            ["keepDepends", "R (>= 4.2)"]
        );
        assert_eq!(
            relation_strings(description.imports_parsed()),
            ["keepImports"]
        );
        assert_eq!(
            relation_strings(description.linking_to_parsed()),
            ["keepLinking"]
        );
        assert_eq!(
            relation_strings(description.suggests_parsed()),
            ["keepSuggests"]
        );
        assert_eq!(
            relation_strings(description.enhances_parsed()),
            ["removeMe", "keepEnhances", "keepEnhances"]
        );
        assert_eq!(
            description.to_string(),
            "Package: project\nVersion: 1.0.0\nDepends:\n    keepDepends,\n    R (>= 4.2)\nImports:\n    keepImports\nLinkingTo:\n    keepLinking\nSuggests:\n    keepSuggests\nEnhances: removeMe, keepEnhances, keepEnhances\n"
        );
    }

    #[test]
    fn remove_dependencies_replaces_duplicate_exact_name_declarations() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nImports: removeMe, keepEarlier\nImports: keepLater\n",
        );

        remove_dependencies(
            Path::new("."),
            &mut description,
            &BTreeSet::from(["removeMe".to_string()]),
        )
        .expect("dependency should be removed");

        assert_eq!(description.fields("Imports").count(), 1);
        assert_eq!(
            relation_strings(description.imports_parsed()),
            ["keepEarlier", "keepLater"]
        );
    }

    #[test]
    fn remove_is_transactional_when_any_dependency_field_is_invalid() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nDepends: cli\nSuggests: testthat (>= invalid)\n",
        );
        let original = description.to_string();

        assert!(
            remove_dependencies(
                Path::new("."),
                &mut description,
                &BTreeSet::from(["cli".to_string()]),
            )
            .is_err()
        );
        assert_eq!(description.to_string(), original);
    }

    #[test]
    fn parses_supported_git_remotes() {
        let description = Description::parse(
            "Package: project\nVersion: 1.0.0\nRemotes: github::owner/github-package@main,\n gitlab@code.example::group/gitlab-package,\n bitbucket::owner/bitbucket-package/subdir@v1,\n generic=git::ssh://git@example.com/team/generic-package.git@develop\n",
        );

        let remotes = remotes(Path::new("."), &description).expect("remotes should parse");

        assert_eq!(remotes.len(), 4);
        assert!(matches!(remotes[0].source, RemoteSource::GitHub(_)));
        assert!(matches!(remotes[1].source, RemoteSource::GitLab(_)));
        assert!(matches!(remotes[2].source, RemoteSource::Bitbucket(_)));
        assert!(matches!(remotes[3].source, RemoteSource::Git(_)));
        assert_eq!(remotes[1].host.as_deref(), Some("code.example"));
        assert_eq!(remotes[3].package.as_deref(), Some("generic"));
    }

    #[test]
    fn rejects_malformed_unsupported_and_duplicate_remotes() {
        let malformed =
            Description::parse("Package: project\nVersion: 1.0.0\nRemotes: github::owner\n");
        assert!(remotes(Path::new("."), &malformed).is_err());

        let invalid = Description::parse(
            "Package: project\nVersion: 1.0.0\nRemotes: dependency=url::https://example.com/pkg.tar.gz, dependency=owner/repository\n",
        );
        let error = remotes(Path::new("."), &invalid).expect_err("remotes should be invalid");
        assert_eq!(error.count, 2);
    }

    #[test]
    fn parses_additional_repositories_in_source_order() {
        let description = Description::parse(
            "Package: project\nVersion: 1.0.0\nAdditional_repositories: https://first.example/cran, https://second.example/cran\n",
        );

        assert_eq!(
            additional_repositories(Path::new("."), &description)
                .expect("repositories should parse")
                .into_iter()
                .map(|url| url.to_string())
                .collect::<Vec<_>>(),
            ["https://first.example/cran", "https://second.example/cran"]
        );
    }

    #[test]
    fn parses_and_sets_base_repository() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nConfig/rpx/base-repository: https://first.example/cran\nconfig/RPX/base-repository: https://second.example/cran\n",
        );
        assert_eq!(
            base_repository(Path::new("."), &description)
                .unwrap()
                .unwrap()
                .as_str(),
            "https://first.example/cran"
        );

        let expected = parse_repository_url("https://replacement.example/cran").unwrap();
        set_base_repository(&mut description, &expected).expect("base repository should be set");

        assert_eq!(description.fields(BASE_REPOSITORY_FIELD).count(), 1);
        assert_eq!(
            base_repository(Path::new("."), &description).unwrap(),
            Some(expected)
        );

        reset_base_repository(&mut description);
        assert_eq!(base_repository(Path::new("."), &description).unwrap(), None);
    }

    #[test]
    fn adds_and_removes_normalized_additional_repositories() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nAdditional_repositories: https://first.example/cran/\n",
        );
        let first = parse_repository_url("https://first.example/cran").unwrap();
        let second = parse_repository_url("https://second.example/cran").unwrap();

        assert!(
            !add_additional_repository(Path::new("."), &mut description, first.clone()).unwrap()
        );
        assert!(
            add_additional_repository(Path::new("."), &mut description, second.clone()).unwrap()
        );
        assert!(remove_additional_repository(Path::new("."), &mut description, &first).unwrap());
        assert_eq!(
            additional_repositories(Path::new("."), &description).unwrap(),
            vec![second]
        );
    }

    #[test]
    fn remove_additional_repository_replaces_duplicate_exact_name_declarations() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nAdditional_repositories: https://remove.example/cran, https://earlier.example/cran\nAdditional_repositories: https://later.example/cran\n",
        );
        let removed = parse_repository_url("https://remove.example/cran").unwrap();

        assert!(remove_additional_repository(Path::new("."), &mut description, &removed).unwrap());

        assert_eq!(description.fields("Additional_repositories").count(), 1);
        assert_eq!(
            additional_repositories(Path::new("."), &description)
                .unwrap()
                .into_iter()
                .map(|repository| repository.to_string())
                .collect::<Vec<_>>(),
            ["https://earlier.example/cran", "https://later.example/cran"]
        );
    }

    #[test]
    fn adds_and_removes_remote_repositories_without_losing_source_details() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nRemotes: github::owner/existing@main\n",
        );
        let remote: Remote = "alias=gitlab@code.example::group/repository/subdir@develop"
            .parse()
            .unwrap();

        assert!(add_remote_repository(Path::new("."), &mut description, remote.clone()).unwrap());
        assert!(!add_remote_repository(Path::new("."), &mut description, remote.clone()).unwrap());
        let configured = remotes(Path::new("."), &description).unwrap();
        assert_eq!(configured[1], remote);
        assert_eq!(configured[1].package.as_deref(), Some("alias"));
        assert_eq!(configured[1].host.as_deref(), Some("code.example"));

        assert!(remove_remote_repository(Path::new("."), &mut description, &remote).unwrap());
        assert_eq!(
            remotes(Path::new("."), &description).unwrap(),
            vec!["github::owner/existing@main".parse::<Remote>().unwrap()]
        );
    }

    #[test]
    fn remove_remote_replaces_duplicate_exact_name_declarations() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nRemotes: github::owner/remove, github::owner/earlier\nRemotes: gitlab::owner/later\n",
        );
        let removed = "github::owner/remove".parse::<Remote>().unwrap();

        assert!(remove_remote_repository(Path::new("."), &mut description, &removed).unwrap());

        assert_eq!(description.fields("Remotes").count(), 1);
        assert_eq!(
            remotes(Path::new("."), &description)
                .unwrap()
                .into_iter()
                .map(|remote| remote.to_string())
                .collect::<Vec<_>>(),
            ["github::owner/earlier", "gitlab::owner/later"]
        );
    }

    #[test]
    fn unsupported_remote_addition_does_not_mutate_description() {
        let mut description = Description::parse(
            "Package: project\nVersion: 1.0.0\nRemotes: github::owner/existing\n",
        );
        let original = description.clone();
        let unsupported = "archive=url::https://example.test/package.tar.gz"
            .parse::<Remote>()
            .unwrap();

        assert!(add_remote_repository(Path::new("."), &mut description, unsupported).is_err());
        assert_eq!(description, original);
    }

    #[test]
    fn rejects_invalid_base_and_additional_repositories() {
        let base = Description::parse(
            "Package: project\nVersion: 1.0.0\nConfig/rpx/base-repository: not-a-url\n",
        );
        assert!(base_repository(Path::new("."), &base).is_err());

        let additional = Description::parse(
            "Package: project\nVersion: 1.0.0\nAdditional_repositories: not-a-url\n",
        );
        assert!(additional_repositories(Path::new("."), &additional).is_err());
    }

    #[test]
    fn configures_base_git_and_additional_repositories_in_order() {
        let description = Description::parse(
            "Package: project\nVersion: 1.0.0\nConfig/rpx/base-repository: https://base.example/cran/\nRemotes: github::owner/repository@main\nAdditional_repositories: https://additional.example/cran/\n",
        );

        let repositories = configured_repositories(Path::new("."), &description)
            .expect("repositories should configure");

        assert!(matches!(
            &repositories[0],
            ConfiguredRepository::Base(url) if url.as_str() == "https://base.example/cran"
        ));
        assert!(matches!(
            &repositories[1],
            ConfiguredRepository::Git(remote)
                if matches!(remote.source, RemoteSource::GitHub(_))
        ));
        assert!(matches!(
            &repositories[2],
            ConfiguredRepository::Additional(url)
                if url.as_str() == "https://additional.example/cran"
        ));
    }

    #[test]
    fn configures_builtin_base_repository_by_default() {
        let repositories = configured_repositories(Path::new("."), &Description::parse(""))
            .expect("default repository should configure");

        assert_eq!(
            repositories,
            [ConfiguredRepository::Base(
                built_in_repository_url().clone()
            )]
        );
    }

    #[test]
    fn aggregates_repository_configuration_errors() {
        let description = Description::parse(
            "Package: project\nVersion: 1.0.0\nConfig/rpx/base-repository: not-a-url\nRemotes: archive=url::https://example.com/pkg.tar.gz\nAdditional_repositories: also-not-a-url\n",
        );

        let error = configured_repositories(Path::new("."), &description)
            .expect_err("repositories should be invalid");

        assert_eq!(error.count, 3);
    }

    #[test]
    fn removes_empty_dependency_fields() {
        let mut description = Description::parse(
            "Package: testpkg\nVersion: 0.1.0\nTitle: Test Package\nDescription: Test package for unit tests.\nLicense: MIT\nImports: digest\n",
        );
        remove_dependencies(
            Path::new("."),
            &mut description,
            &BTreeSet::from(["digest".to_string()]),
        )
        .unwrap();

        let contents = description.to_string();
        assert_eq!(
            contents,
            "Package: testpkg\nVersion: 0.1.0\nTitle: Test Package\nDescription: Test package for unit tests.\nLicense: MIT\n"
        );
        assert!(
            Description::parse(&contents).diagnostics().is_empty(),
            "serialized DESCRIPTION should parse:\n{contents}"
        );
    }
}
