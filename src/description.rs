use miette::{Diagnostic, NamedSource, SourceSpan};
use r_description::{
    AdditionalRepositoriesError, DependsError, FieldMutationError, ImportsError, LinkingToError,
    PackageError, RDescription, Relation, Remote, RemoteSource, RemotesError, SuggestsError, Url,
    Version, VersionError,
};
use std::{
    collections::BTreeSet,
    env,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;

use crate::repository::{
    BUILT_IN_REPOSITORY_BASE_URL, GitRepository, PackageRepository, RepositoryError,
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

pub fn initial_description(package_name: &str) -> Result<RDescription, InitialDescriptionError> {
    let mut description = RDescription::parse("");
    description.set_package(package_name)?;
    let version = "0.1.0".parse().expect("0.1.0 should parse");
    description.set_version(&version);
    description.set_title(&title_from_package_name(package_name))?;
    description.set_description("Add a package description.")?;
    description.set_license("MIT")?;
    description.set_authors_at_r(
        r#"person("First", "Last", email = "you@example.com", role = c("aut", "cre"))"#,
    )?;
    description.set_maintainer("Your Name <you@example.com>")?;
    Ok(description)
}

#[derive(Debug, Error, Diagnostic)]
pub enum PackageNameDerivationError {
    #[error("failed to derive a package name from {}", path.display())]
    #[diagnostic(code(rpx::description::package_name_missing))]
    MissingDirectoryName { path: PathBuf },

    #[error("project directory name is not valid UTF-8: {}", path.display())]
    #[diagnostic(code(rpx::description::package_name_invalid_utf8))]
    InvalidUtf8 { path: PathBuf },

    #[error("directory name `{directory_name}` does not produce a valid package name")]
    #[diagnostic(
        code(rpx::description::package_name_empty),
        help("Use at least one ASCII letter in the directory name.")
    )]
    Empty { directory_name: String },

    #[error("derived package name `{package_name}` must start with a letter")]
    #[diagnostic(
        code(rpx::description::package_name_invalid_start),
        help("Rename the directory so its name starts with an ASCII letter.")
    )]
    MustStartWithLetter { package_name: String },
}

pub fn derive_package_name(path: &Path) -> Result<String, PackageNameDerivationError> {
    let directory_name = path
        .file_name()
        .ok_or_else(|| PackageNameDerivationError::MissingDirectoryName {
            path: path.to_path_buf(),
        })?
        .to_str()
        .ok_or_else(|| PackageNameDerivationError::InvalidUtf8 {
            path: path.to_path_buf(),
        })?;

    let mut package_name = String::new();

    for character in directory_name.chars() {
        match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' => package_name.push(character),
            '-' | '_' | ' ' | '.' if !package_name.ends_with('.') => {
                package_name.push('.');
            }
            _ => {}
        }
    }

    let package_name = package_name.trim_matches('.').to_string();

    let Some(first) = package_name.chars().next() else {
        return Err(PackageNameDerivationError::Empty {
            directory_name: directory_name.to_string(),
        });
    };

    if !first.is_ascii_alphabetic() {
        return Err(PackageNameDerivationError::MustStartWithLetter { package_name });
    }

    Ok(package_name)
}

fn title_from_package_name(package_name: &str) -> String {
    package_name
        .split('.')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut characters = part.chars();
            let Some(first) = characters.next() else {
                return String::new();
            };

            format!("{}{}", first.to_ascii_uppercase(), characters.as_str())
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    project_path: &Path,
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
            project_path.join(DESCRIPTION_NAME).display().to_string(),
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
        .map(|repositories| repositories.collect())
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
                        error: "Config/rpx/base-repository field is declared multiple times"
                            .into(),
                        span: span.clone().into(),
                    });
                }

                match Url::parse(value.value()) {
                    Ok(url) => repository = Some(url),
                    Err(source) => issues.push(DescriptionParseIssue::Positioned {
                        error: format!(
                            "Config/rpx/base-repository contains an invalid URL: {source}"
                        )
                        .into(),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfiguredRepository {
    Base(Url),
    Git(Remote),
    Additional(Url),
}

#[derive(Debug, Error, Diagnostic)]
pub enum ConfiguredRepositoriesError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Description(#[from] DescriptionParseError),

    #[error("RPX_REGISTRY_BASE_URL contains an invalid URL: {source}")]
    #[diagnostic(code(rpx::description::invalid_base_repository_environment))]
    InvalidEnvironmentUrl {
        #[source]
        source: url::ParseError,
    },

    #[error("RPX_REGISTRY_BASE_URL is not valid Unicode")]
    #[diagnostic(code(rpx::description::invalid_base_repository_environment))]
    EnvironmentNotUnicode,
}

pub fn configured_repositories(
    path: &Path,
    description: &RDescription,
) -> Result<Vec<ConfiguredRepository>, ConfiguredRepositoriesError> {
    let environment_base = match env::var("RPX_REGISTRY_BASE_URL") {
        Ok(value) => Some(
            Url::parse(&value)
                .map_err(|source| ConfiguredRepositoriesError::InvalidEnvironmentUrl { source })?,
        ),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(ConfiguredRepositoriesError::EnvironmentNotUnicode);
        }
    };
    let configured_base = environment_base
        .is_none()
        .then(|| base_repository(path, description));
    let configured_remotes = remotes(path, description);
    let configured_additional = additional_repositories(path, description);
    let mut issues = Vec::new();

    let configured_base = match configured_base {
        Some(Ok(repository)) => repository,
        Some(Err(DescriptionParseError {
            issues: base_issues,
            ..
        })) => {
            issues.extend(base_issues);
            None
        }
        None => None,
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
        )
        .into());
    }

    let base = environment_base.or(configured_base).unwrap_or_else(|| {
        Url::parse(BUILT_IN_REPOSITORY_BASE_URL)
            .expect("built-in repository URL should be valid")
    });

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
    Configuration(#[from] ConfiguredRepositoriesError),

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
    futures_util::future::join_all(
        configured_repositories(path, description)?
            .into_iter()
            .map(|repository| async move {
                match repository {
                    ConfiguredRepository::Base(url) => {
                        <dyn PackageRepository>::from_url(url.as_str())
                            .await
                            .map_err(|source| RepositoriesFromDescriptionError::Repository {
                                kind: "base",
                                source,
                            })
                    }
                    ConfiguredRepository::Git(remote) => GitRepository::new(remote)
                        .map(|repository| {
                            Arc::new(repository) as Arc<dyn PackageRepository>
                        })
                        .map_err(|source| RepositoriesFromDescriptionError::Repository {
                            kind: "Git",
                            source,
                        }),
                    ConfiguredRepository::Additional(url) => {
                        <dyn PackageRepository>::from_url(url.as_str())
                            .await
                            .map_err(|source| RepositoriesFromDescriptionError::Repository {
                                kind: "additional",
                                source,
                            })
                    }
                }
            }),
    )
    .await
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::title_from_package_name;
    use r_description::RDescription;

    #[test]
    fn derives_title_from_package_name() {
        assert_eq!(
            title_from_package_name("my.package.name"),
            "My Package Name"
        );
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
