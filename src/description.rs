use miette::{Diagnostic, NamedSource, SourceSpan};
use r_description::{
    AdditionalRepositoriesError, DependsError, FieldMutationError, ImportsError, LinkingToError,
    PackageError, RDescription, Relation, Remote, RemoteMutationError, RemoteSource, RemotesError,
    SuggestsError, Url, UrlMutationError, Version, VersionError,
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

#[derive(Debug, Error, Diagnostic)]
#[error("failed to parse DESCRIPTION ({count} errors)")]
#[diagnostic(code(rpx::description::parse_failed))]
pub struct DescriptionParseError {
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

#[derive(Debug, Error, Diagnostic)]
pub enum DescriptionParseIssue {
    #[error("{error}")]
    Positioned {
        error: Box<dyn std::error::Error + Send + Sync>,

        #[label("{error}")]
        span: SourceSpan,
    },

    #[error("{error}")]
    Unpositioned {
        error: Box<dyn std::error::Error + Send + Sync>,
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
            error: Box::new(error),
            span: span.into(),
        }
    }

    pub fn unpositioned(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Unpositioned {
            error: Box::new(error),
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

pub fn read_description(path: &PathBuf) -> Result<RDescription, DescriptionReadError> {
    let path = path.join(DESCRIPTION_NAME);
    let contents = fs::read_to_string(&path).map_err(|source| DescriptionReadError::Read {
        path: path.clone(),
        source,
    })?;
    let description = RDescription::parse(&contents);
    let issues = description
        .syntax_issues()
        .iter()
        .cloned()
        .map(|issue| {
            let start = usize::from(issue.range.start());
            let end = usize::from(issue.range.end());
            DescriptionParseIssue::positioned(issue, start..end)
        })
        .collect::<Vec<_>>();
    if !issues.is_empty() {
        return Err(
            DescriptionParseError::new(path.display().to_string(), contents, issues).into(),
        );
    }

    Ok(description)
}

#[derive(Debug, Error, Diagnostic)]
pub enum DescriptionWriteError {
    #[error("failed to write DESCRIPTION at {}: {source}", path.display())]
    #[diagnostic(code(rpx::project::description_write_failed))]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn write_description(
    path: &PathBuf,
    description: &RDescription,
) -> Result<(), DescriptionWriteError> {
    let path = path.join(DESCRIPTION_NAME);
    fs::write(&path, description.to_string())
        .map_err(|source| DescriptionWriteError::Write { path, source })
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

#[derive(Debug, Error, Diagnostic)]
pub enum InitialDescriptionError {
    #[error("failed to initialize DESCRIPTION")]
    #[diagnostic(code(rpx::description::initializing_description))]
    FieldMutation(#[from] r_description::FieldMutationError),
}

#[derive(Clone, Copy)]
pub struct InitialDescriptionOptions<'a> {
    pub package_name: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub authors_at_r: &'a str,
    pub license: &'a str,
}

pub fn initial_description(
    options: InitialDescriptionOptions<'_>,
) -> Result<RDescription, InitialDescriptionError> {
    let mut description = RDescription::parse("");
    description.set_package(options.package_name)?;
    let version = "0.1.0".parse().expect("0.1.0 should parse");
    description.set_version(&version);
    description.set_title(options.title)?;
    description.set_description(options.description)?;
    description.set_license(options.license)?;
    description.set_authors_at_r(options.authors_at_r)?;
    Ok(description)
}

pub fn root_package(
    path: &Path,
    description: &RDescription,
) -> Result<(String, Version), DescriptionParseError> {
    let package = description.package();
    let version = description.version();

    let package_issues = package
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            PackageError::Missing => {
                vec![DescriptionParseIssue::missing(error.to_string())]
            }
            PackageError::Duplicate(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect(),
        });

    let version_issues = version
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            VersionError::Missing => {
                vec![DescriptionParseIssue::missing(error.to_string())]
            }
            VersionError::Duplicate(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect(),
            VersionError::Invalid { value, source } => {
                let value_range = value.range();
                let value_start = usize::from(value_range.start());
                let span = source.range().map_or_else(
                    || usize::from(value_range.start())..usize::from(value_range.end()),
                    |range| {
                        value_start + usize::from(range.start())
                            ..value_start + usize::from(range.end())
                    },
                );

                vec![DescriptionParseIssue::positioned(error.clone(), span)]
            }
        });

    let issues = package_issues.chain(version_issues).collect::<Vec<_>>();

    match (package, version) {
        (Ok(package), Ok(version)) => Ok((package, version)),
        _ => Err(DescriptionParseError::new(
            path.join(DESCRIPTION_NAME).display().to_string(),
            description.to_string(),
            issues,
        )),
    }
}

pub fn required_dependencies(
    source_name: impl Into<String>,
    description: &RDescription,
) -> Result<BTreeSet<Relation>, DescriptionParseError> {
    let imports = description.imports();
    let depends = description.depends();
    let linking_to = description.linking_to();

    let imports_issues = imports
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            ImportsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let depends_issues = depends
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            DependsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let linking_to_issues = linking_to
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            LinkingToError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let issues = imports_issues
        .chain(depends_issues)
        .chain(linking_to_issues)
        .collect::<Vec<_>>();

    match (imports, depends, linking_to) {
        (Ok(imports), Ok(depends), Ok(linking_to)) => Ok(imports
            .chain(depends)
            .chain(linking_to)
            .filter(|relation| relation.package() != "R")
            .collect()),
        _ => Err(DescriptionParseError::new(
            source_name,
            description.to_string(),
            issues,
        )),
    }
}

pub fn project_dependencies(
    project_path: &Path,
    description: &RDescription,
) -> Result<BTreeSet<Relation>, DescriptionParseError> {
    let imports = description.imports();
    let depends = description.depends();
    let linking_to = description.linking_to();
    let suggests = description.suggests();

    let imports_issues = imports
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            ImportsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let depends_issues = depends
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            DependsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let linking_to_issues = linking_to
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            LinkingToError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let suggests_issues = suggests
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            SuggestsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let issues = imports_issues
        .chain(depends_issues)
        .chain(linking_to_issues)
        .chain(suggests_issues)
        .collect::<Vec<_>>();

    match (imports, depends, linking_to, suggests) {
        (Ok(imports), Ok(depends), Ok(linking_to), Ok(suggests)) => Ok(imports
            .chain(depends)
            .chain(linking_to)
            .chain(suggests)
            .filter(|relation| relation.package() != "R")
            .collect()),
        _ => Err(DescriptionParseError::new(
            project_path.join(DESCRIPTION_NAME).display().to_string(),
            description.to_string(),
            issues,
        )),
    }
}

pub fn add_dependencies(
    path: &Path,
    description: &mut RDescription,
    dependencies: &BTreeSet<Relation>,
) -> Result<(), DescriptionParseError> {
    if dependencies.is_empty() {
        return Ok(());
    }

    let depends = description.depends();
    let imports = description.imports();
    let linking_to = description.linking_to();
    let suggests = description.suggests();

    let depends_issues = depends
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            DependsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let imports_issues = imports
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            ImportsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let linking_to_issues = linking_to
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            LinkingToError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let suggests_issues = suggests
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            SuggestsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let issues = depends_issues
        .chain(imports_issues)
        .chain(linking_to_issues)
        .chain(suggests_issues)
        .collect::<Vec<_>>();

    let (mut depends, mut imports, mut linking_to, mut suggests) =
        match (depends, imports, linking_to, suggests) {
            (Ok(depends), Ok(imports), Ok(linking_to), Ok(suggests)) => (
                depends.collect::<BTreeSet<_>>(),
                imports.collect::<BTreeSet<_>>(),
                linking_to.collect::<BTreeSet<_>>(),
                suggests.collect::<BTreeSet<_>>(),
            ),
            _ => {
                return Err(DescriptionParseError::new(
                    path.join(DESCRIPTION_NAME).display().to_string(),
                    description.to_string(),
                    issues,
                )
                .into());
            }
        };

    let added_packages = dependencies
        .iter()
        .map(|dependency| dependency.package().to_string())
        .collect::<BTreeSet<_>>();

    depends.retain(|dependency| !added_packages.contains(dependency.package()));
    imports.retain(|dependency| !added_packages.contains(dependency.package()));
    imports.extend(dependencies.iter().cloned());
    linking_to.retain(|dependency| !added_packages.contains(dependency.package()));
    suggests.retain(|dependency| !added_packages.contains(dependency.package()));

    description.set_depends(depends);
    description.set_imports(imports);
    description.set_linking_to(linking_to);
    description.set_suggests(suggests);

    Ok(())
}

pub fn remove_dependencies(
    path: &Path,
    description: &mut RDescription,
    packages: &BTreeSet<String>,
) -> Result<(), DescriptionParseError> {
    if packages.is_empty() {
        return Ok(());
    }

    let depends = description.depends();
    let imports = description.imports();
    let linking_to = description.linking_to();
    let suggests = description.suggests();

    let depends_issues = depends
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            DependsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let imports_issues = imports
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            ImportsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let linking_to_issues = linking_to
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            LinkingToError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let suggests_issues = suggests
        .as_ref()
        .err()
        .into_iter()
        .flat_map(|error| match error {
            SuggestsError::Invalid(occurrences) => occurrences
                .iter()
                .map(|occurrence| {
                    let range = occurrence.range();
                    DescriptionParseIssue::positioned(
                        error.clone(),
                        usize::from(range.start())..usize::from(range.end()),
                    )
                })
                .collect::<Vec<_>>(),
        });

    let issues = depends_issues
        .chain(imports_issues)
        .chain(linking_to_issues)
        .chain(suggests_issues)
        .collect::<Vec<_>>();

    let (mut depends, mut imports, mut linking_to, mut suggests) =
        match (depends, imports, linking_to, suggests) {
            (Ok(depends), Ok(imports), Ok(linking_to), Ok(suggests)) => (
                depends.collect::<BTreeSet<_>>(),
                imports.collect::<BTreeSet<_>>(),
                linking_to.collect::<BTreeSet<_>>(),
                suggests.collect::<BTreeSet<_>>(),
            ),
            _ => {
                return Err(DescriptionParseError::new(
                    path.join(DESCRIPTION_NAME).display().to_string(),
                    description.to_string(),
                    issues,
                )
                .into());
            }
        };

    depends.retain(|dependency| !packages.contains(dependency.package()));
    imports.retain(|dependency| !packages.contains(dependency.package()));
    linking_to.retain(|dependency| !packages.contains(dependency.package()));
    suggests.retain(|dependency| !packages.contains(dependency.package()));

    description.set_depends(depends);
    description.set_imports(imports);
    description.set_linking_to(linking_to);
    description.set_suggests(suggests);

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
    description: &RDescription,
) -> Result<Vec<Remote>, DescriptionParseError> {
    let remotes = description
        .remotes()
        .map_err(|source| {
            let issues = match &source {
                RemotesError::Invalid(occurrences) => occurrences
                    .iter()
                    .map(|occurrence| {
                        let range = occurrence.range();

                        DescriptionParseIssue::positioned(
                            source.clone(),
                            usize::from(range.start())..usize::from(range.end()),
                        )
                    })
                    .collect(),
            };

            DescriptionParseError::new(
                path.join(DESCRIPTION_NAME).display().to_string(),
                description.to_string(),
                issues,
            )
        })?
        .collect::<Vec<_>>();

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
    description: &RDescription,
) -> Result<Vec<Url>, DescriptionParseError> {
    description
        .additional_repositories()
        .map(|repositories| {
            repositories
                .map(|mut repository| {
                    if let Ok(mut segments) = repository.path_segments_mut() {
                        segments.pop_if_empty();
                    }
                    repository
                })
                .collect()
        })
        .map_err(|source| {
            let issues = match &source {
                AdditionalRepositoriesError::Invalid(occurrences) => occurrences
                    .iter()
                    .map(|occurrence| {
                        let range = occurrence.range();
                        DescriptionParseIssue::positioned(
                            source.clone(),
                            usize::from(range.start())..usize::from(range.end()),
                        )
                    })
                    .collect(),
            };

            DescriptionParseError::new(
                path.join(DESCRIPTION_NAME).display().to_string(),
                description.to_string(),
                issues,
            )
        })
}

pub fn base_repository(
    path: &Path,
    description: &RDescription,
) -> Result<Option<Url>, DescriptionParseError> {
    let values = description.field_values(BASE_REPOSITORY_FIELD);
    let duplicate = values.len() > 1;
    let (repository, issues) =
        values
            .into_iter()
            .fold((None, Vec::new()), |(mut repository, mut issues), value| {
                let range = value.range();
                let span = usize::from(range.start())..usize::from(range.end());

                if duplicate {
                    issues.push(DescriptionParseIssue::Positioned {
                        error: "Config/rpx/base-repository field is declared multiple times".into(),
                        span: span.clone().into(),
                    });
                }

                match parse_repository_url(value.value()) {
                    Ok(url) => repository = Some(url),
                    Err(source) => issues.push(DescriptionParseIssue::Positioned {
                        error: Box::new(source),
                        span: span.into(),
                    }),
                }

                (repository, issues)
            });

    if issues.is_empty() {
        Ok(repository)
    } else {
        Err(DescriptionParseError::new(
            path.join(DESCRIPTION_NAME).display().to_string(),
            description.to_string(),
            issues,
        ))
    }
}

pub fn set_base_repository(
    description: &mut RDescription,
    repository: &Url,
) -> Result<(), FieldMutationError> {
    description.set_field(BASE_REPOSITORY_FIELD, repository.as_str())
}

pub fn reset_base_repository(description: &mut RDescription) {
    description.remove_field(BASE_REPOSITORY_FIELD);
}

#[derive(Debug, Error, Diagnostic)]
pub enum RepositoryMutationError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Description(#[from] DescriptionParseError),

    #[error("failed to update Additional_repositories: {source}")]
    #[diagnostic(code(rpx::description::additional_repositories_update_failed))]
    Additional {
        #[from]
        source: UrlMutationError,
    },

    #[error("failed to update Remotes: {source}")]
    #[diagnostic(code(rpx::description::remotes_update_failed))]
    Remote {
        #[from]
        source: RemoteMutationError,
    },
}

pub fn add_additional_repository(
    path: &Path,
    description: &mut RDescription,
    repository: Url,
) -> Result<bool, RepositoryMutationError> {
    let mut repositories = additional_repositories(path, description)?;
    if repositories.contains(&repository) {
        return Ok(false);
    }

    repositories.push(repository);
    description.set_additional_repositories(repositories)?;
    Ok(true)
}

pub fn remove_additional_repository(
    path: &Path,
    description: &mut RDescription,
    repository: &Url,
) -> Result<bool, RepositoryMutationError> {
    let mut repositories = additional_repositories(path, description)?;
    let previous_len = repositories.len();
    repositories.retain(|existing| existing != repository);
    if repositories.len() == previous_len {
        return Ok(false);
    }

    description.set_additional_repositories(repositories)?;
    Ok(true)
}

pub fn add_remote_repository(
    path: &Path,
    description: &mut RDescription,
    remote: Remote,
) -> Result<bool, RepositoryMutationError> {
    let mut configured = remotes(path, description)?;
    if configured.contains(&remote) {
        return Ok(false);
    }

    configured.push(remote);
    let mut updated = description.clone();
    updated.set_remotes(configured)?;
    remotes(path, &updated)?;
    *description = updated;
    Ok(true)
}

pub fn remove_remote_repository(
    path: &Path,
    description: &mut RDescription,
    remote: &Remote,
) -> Result<bool, RepositoryMutationError> {
    let mut configured = remotes(path, description)?;
    let previous_len = configured.len();
    configured.retain(|existing| existing != remote);
    if configured.len() == previous_len {
        return Ok(false);
    }

    let mut updated = description.clone();
    updated.set_remotes(configured)?;
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
    description: &RDescription,
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
    description: &RDescription,
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

    fn relation_strings(
        relations: Result<std::vec::IntoIter<Relation>, impl std::fmt::Debug>,
    ) -> Vec<String> {
        relations
            .expect("relations should parse")
            .map(|relation| relation.to_string())
            .collect()
    }

    fn relation_set(relations: &[&str]) -> BTreeSet<Relation> {
        relations
            .iter()
            .map(|relation| relation.parse().expect("relation should parse"))
            .collect()
    }

    #[test]
    fn derives_initial_description_from_package_name() {
        let description = initial_description(InitialDescriptionOptions {
            package_name: "my.package",
            title: "My Package",
            description: "Describe what this package does.",
            authors_at_r: r#"person(given = "Package Author", email = "author@example.com", role = c("aut", "cre"))"#,
            license: "MIT + file LICENSE",
        })
        .expect("description should initialize");

        assert_eq!(description.package().unwrap(), "my.package");
        assert_eq!(description.version().unwrap().to_string(), "0.1.0");
        assert_eq!(description.title().unwrap(), "My Package");
        assert_eq!(
            description.description().unwrap(),
            "Describe what this package does."
        );
        assert_eq!(description.license().unwrap(), "MIT + file LICENSE");
        let rendered = description.to_string();
        assert!(rendered.contains(
            "Authors@R: person(given = \"Package Author\", email = \"author@example.com\", role = c(\"aut\", \"cre\"))"
        ));
        assert!(!rendered.lines().any(|line| line.starts_with("Author:")));
        assert!(!rendered.lines().any(|line| line.starts_with("Maintainer:")));
    }

    #[test]
    fn uses_explicit_initial_description_metadata() {
        let description = initial_description(InitialDescriptionOptions {
            package_name: "custom.pkg",
            title: "Custom Package",
            description: "A custom package description.",
            authors_at_r: r#"person(given = "Example Author", email = "author@example.com", role = c("aut", "cre"))"#,
            license: "Apache License (== 2.0)",
        })
        .expect("description should initialize");

        assert_eq!(description.package().unwrap(), "custom.pkg");
        assert_eq!(description.title().unwrap(), "Custom Package");
        assert_eq!(
            description.description().unwrap(),
            "A custom package description."
        );
        assert_eq!(description.license().unwrap(), "Apache License (== 2.0)");
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

        let expected = RDescription::parse("Package: project\nVersion: 2.0.0\nImports: cli\n");
        write_description(&directory.0, &expected).expect("DESCRIPTION should be written");
        let actual = read_description(&directory.0).expect("DESCRIPTION should be reread");

        assert_eq!(actual.to_string(), expected.to_string());
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
        let description = RDescription::parse("Package: project\nVersion: 1.2.3\n");

        let (package, version) =
            root_package(Path::new("."), &description).expect("root should parse");

        assert_eq!(package, "project");
        assert_eq!(version.to_string(), "1.2.3");
    }

    #[test]
    fn reports_all_invalid_root_fields() {
        let missing = RDescription::parse("");
        let error = root_package(Path::new("."), &missing).expect_err("root should be invalid");
        assert_eq!(error.count, 2);

        let invalid = RDescription::parse("Package: one\nPackage: two\nVersion: invalid\n");
        let error = root_package(Path::new("."), &invalid).expect_err("root should be invalid");
        assert_eq!(error.count, 3);
    }

    #[test]
    fn collects_project_dependencies_with_current_field_semantics() {
        let description = RDescription::parse(
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
        let description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nImports: cli (>= invalid)\nSuggests: testthat (< invalid)\n",
        );

        let error = project_dependencies(Path::new("."), &description)
            .expect_err("dependencies should be invalid");

        assert_eq!(error.count, 2);
    }

    #[test]
    fn add_moves_packages_to_imports_and_leaves_enhances_untouched() {
        let mut description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nDepends: dplyr (>= 1.0.0), keepDepends\nImports: dplyr (< 2.0.0), keepImports\nLinkingTo: dplyr, keepLinking\nSuggests: dplyr, keepSuggests\nEnhances: dplyr, keepEnhances\n",
        );

        add_dependencies(
            Path::new("."),
            &mut description,
            &relation_set(&["dplyr (== 1.1.0)"]),
        )
        .expect("dependency should be added");

        assert_eq!(relation_strings(description.depends()), ["keepDepends"]);
        assert_eq!(
            relation_strings(description.imports()),
            ["dplyr (== 1.1.0)", "keepImports"]
        );
        assert_eq!(relation_strings(description.linking_to()), ["keepLinking"]);
        assert_eq!(relation_strings(description.suggests()), ["keepSuggests"]);
        assert_eq!(
            relation_strings(description.enhances()),
            ["dplyr", "keepEnhances"]
        );
    }

    #[test]
    fn add_semantically_deduplicates_every_rewritten_field() {
        let mut description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nDepends: alpha, alpha\nImports: beta (>= 1.0), beta (>= 1.0.0), zoo\nLinkingTo: gamma, gamma\nSuggests: delta, delta\n",
        );

        add_dependencies(
            Path::new("."),
            &mut description,
            &relation_set(&["askpass", "cli"]),
        )
        .expect("dependencies should be added");

        assert_eq!(relation_strings(description.depends()), ["alpha"]);
        assert_eq!(
            relation_strings(description.imports()),
            ["askpass", "beta (>= 1.0.0)", "cli", "zoo"]
        );
        assert_eq!(relation_strings(description.linking_to()), ["gamma"]);
        assert_eq!(relation_strings(description.suggests()), ["delta"]);
    }

    #[test]
    fn add_preserves_distinct_requirements_for_the_same_package() {
        let mut description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nImports: cli (>= 1.0.0), cli (< 2.0.0)\n",
        );

        add_dependencies(Path::new("."), &mut description, &relation_set(&["digest"]))
            .expect("dependency should be added");

        assert_eq!(
            relation_strings(description.imports()),
            ["cli (< 2.0.0)", "cli (>= 1.0.0)", "digest"]
        );
    }

    #[test]
    fn add_is_transactional_when_any_dependency_field_is_invalid() {
        let mut description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nDepends: cli\nImports: digest (>= invalid)\n",
        );
        let original = description.to_string();

        assert!(
            add_dependencies(
                Path::new("."),
                &mut description,
                &relation_set(&["jsonlite"]),
            )
            .is_err()
        );
        assert_eq!(description.to_string(), original);
    }

    #[test]
    fn empty_add_does_not_rewrite_existing_fields() {
        let mut description =
            RDescription::parse("Package: project\nVersion: 1.0.0\nImports: cli, cli\n");
        let original = description.to_string();

        add_dependencies(Path::new("."), &mut description, &BTreeSet::new())
            .expect("empty add should succeed");

        assert_eq!(description.to_string(), original);
    }

    #[test]
    fn remove_normalizes_managed_fields_and_leaves_enhances_untouched() {
        let mut description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nDepends: R (>= 4.2), removeMe, keepDepends, keepDepends\nImports: removeMe, keepImports, keepImports\nLinkingTo: removeMe, keepLinking, keepLinking\nSuggests: removeMe, keepSuggests, keepSuggests\nEnhances: removeMe, keepEnhances, keepEnhances\n",
        );

        remove_dependencies(
            Path::new("."),
            &mut description,
            &BTreeSet::from(["removeMe".to_string()]),
        )
        .expect("dependency should be removed");

        assert_eq!(
            relation_strings(description.depends()),
            ["R (>= 4.2)", "keepDepends"]
        );
        assert_eq!(relation_strings(description.imports()), ["keepImports"]);
        assert_eq!(relation_strings(description.linking_to()), ["keepLinking"]);
        assert_eq!(relation_strings(description.suggests()), ["keepSuggests"]);
        assert_eq!(
            relation_strings(description.enhances()),
            ["removeMe", "keepEnhances", "keepEnhances"]
        );
    }

    #[test]
    fn remove_is_transactional_when_any_dependency_field_is_invalid() {
        let mut description = RDescription::parse(
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
        let description = RDescription::parse(
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
            RDescription::parse("Package: project\nVersion: 1.0.0\nRemotes: github::owner\n");
        assert!(remotes(Path::new("."), &malformed).is_err());

        let invalid = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nRemotes: dependency=url::https://example.com/pkg.tar.gz, dependency=owner/repository\n",
        );
        let error = remotes(Path::new("."), &invalid).expect_err("remotes should be invalid");
        assert_eq!(error.count, 2);
    }

    #[test]
    fn parses_additional_repositories_in_source_order() {
        let description = RDescription::parse(
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
        let mut description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nConfig/rpx/base-repository: https://first.example/cran\nconfig/RPX/base-repository: https://second.example/cran\n",
        );
        assert!(base_repository(Path::new("."), &description).is_err());

        let expected = parse_repository_url("https://replacement.example/cran").unwrap();
        set_base_repository(&mut description, &expected).expect("base repository should be set");

        assert_eq!(description.field_values(BASE_REPOSITORY_FIELD).len(), 1);
        assert_eq!(
            base_repository(Path::new("."), &description).unwrap(),
            Some(expected)
        );

        reset_base_repository(&mut description);
        assert_eq!(base_repository(Path::new("."), &description).unwrap(), None);
    }

    #[test]
    fn adds_and_removes_normalized_additional_repositories() {
        let mut description = RDescription::parse(
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
    fn adds_and_removes_remote_repositories_without_losing_source_details() {
        let mut description = RDescription::parse(
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
    fn unsupported_remote_addition_does_not_mutate_description() {
        let mut description = RDescription::parse(
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
        let base = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nConfig/rpx/base-repository: not-a-url\n",
        );
        assert!(base_repository(Path::new("."), &base).is_err());

        let additional = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nAdditional_repositories: not-a-url\n",
        );
        assert!(additional_repositories(Path::new("."), &additional).is_err());
    }

    #[test]
    fn configures_base_git_and_additional_repositories_in_order() {
        let description = RDescription::parse(
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
        let repositories = configured_repositories(Path::new("."), &RDescription::parse(""))
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
        let description = RDescription::parse(
            "Package: project\nVersion: 1.0.0\nConfig/rpx/base-repository: not-a-url\nRemotes: archive=url::https://example.com/pkg.tar.gz\nAdditional_repositories: also-not-a-url\n",
        );

        let error = configured_repositories(Path::new("."), &description)
            .expect_err("repositories should be invalid");

        assert_eq!(error.count, 3);
    }

    #[test]
    fn serializes_empty_dependency_fields_as_parseable_description() {
        let mut description = RDescription::parse(
            "Package: testpkg\nVersion: 0.1.0\nTitle: Test Package\nDescription: Test package for unit tests.\nLicense: MIT\nImports: digest\n",
        );
        description.set_imports([]);

        let contents = description.to_string();
        assert!(
            RDescription::parse(&contents).syntax_issues().is_empty(),
            "serialized DESCRIPTION should parse:\n{contents}"
        );
    }
}
