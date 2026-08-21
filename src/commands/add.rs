use crate::{
    LockError, SyncError,
    description::{DependencyField, DescriptionParseError, add_dependencies, project_dependencies},
    output::status,
    project::{
        ProjectLoadError, ProjectWriteError, ResolutionPolicy, load_project,
        pin_unconstrained_relations, resolve_project, write_project_metadata,
    },
    repository::RepositoryError,
    sync::{ProjectPackageMode, SyncProjectOptions, SystemSyncMode, sync_resolved_project},
};
use miette::Diagnostic;
use r_description::{Relation, Version, VersionRequirement};
use std::collections::BTreeSet;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLoad(#[from] ProjectLoadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    PackageParse(#[from] AddPackagesParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Lock(#[from] LockError),

    #[error("failed to load resolved package metadata: {source}")]
    #[diagnostic(code(rpx::add::package_metadata_failed))]
    PackageMetadata {
        #[source]
        source: RepositoryError,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectWrite(#[from] ProjectWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Install(#[from] SyncError),
}

pub(crate) async fn run(
    packages: &[String],
    dependency_field: DependencyField,
    no_install_project: bool,
) -> Result<(), Error> {
    let mut project = load_project()?;
    let added_relations = parse_add_packages(packages)?;
    let unconstrained_packages = added_relations
        .iter()
        .filter(|relation| matches!(relation.requirement(), VersionRequirement::Any))
        .map(|relation| relation.package().to_string())
        .collect::<BTreeSet<_>>();

    add_dependencies(
        &project.root,
        &mut project.description,
        &added_relations,
        dependency_field,
    )?;
    let mut resolution = resolve_project(&project, ResolutionPolicy::ReuseIfValid)
        .await
        .map_err(map_resolution_error)?;
    let final_added_relations =
        pin_unconstrained_relations(&added_relations, &unconstrained_packages, &resolution)
            .await
            .map_err(LockError::BasePackages)?;

    if final_added_relations != added_relations {
        add_dependencies(
            &project.root,
            &mut project.description,
            &final_added_relations,
            dependency_field,
        )?;
        resolution.lockfile.requirements =
            project_dependencies(&project.root, &project.description)?;
    }

    write_project_metadata(&project, &resolution)?;
    sync_resolved_project(
        &project,
        resolution,
        SyncProjectOptions {
            project_package: if no_install_project {
                ProjectPackageMode::Omit
            } else {
                ProjectPackageMode::Install
            },
            system: SystemSyncMode::Check,
        },
    )
    .await?;
    status(format_args!(
        "Added {}",
        added_relations
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    Ok(())
}

fn map_resolution_error(error: LockError) -> Error {
    match error {
        LockError::PackageMetadata { source } => Error::PackageMetadata { source },
        source => Error::Lock(source),
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("invalid package constraints")]
#[diagnostic(code(rpx::add::invalid_constraint))]
pub(crate) struct AddPackagesParseError {
    #[related]
    issues: Vec<AddPackageParseIssue>,
}

#[derive(Debug, Error, Diagnostic)]
#[error("invalid package constraint `{package}`: {reason}")]
struct AddPackageParseIssue {
    package: String,
    reason: AddPackageParseReason,
}

#[derive(Debug, Error)]
enum AddPackageParseReason {
    #[error("must not be empty")]
    Empty,

    #[error("must not contain whitespace")]
    Whitespace,

    #[error("package name is missing")]
    MissingName,

    #[error(
        "version constraint is missing; quote constraints containing shell operators, for example `rpx add 'digest@>=0.6.37'`"
    )]
    MissingConstraint,

    #[error("version constraint operator is missing or invalid")]
    InvalidOperator,

    #[error("version is missing")]
    MissingVersion,

    #[error("invalid version `{version}`: {details}")]
    InvalidVersion { version: String, details: String },

    #[error("invalid package name: {details}")]
    InvalidPackageName { details: String },
}

fn parse_add_packages(packages: &[String]) -> Result<BTreeSet<Relation>, AddPackagesParseError> {
    let (relations, issues): (Vec<_>, Vec<_>) = packages
        .iter()
        .map(|package| {
            let issue = |reason| AddPackageParseIssue {
                package: package.clone(),
                reason,
            };
            if package.is_empty() {
                return Err(issue(AddPackageParseReason::Empty));
            }
            if package.chars().any(char::is_whitespace) {
                return Err(issue(AddPackageParseReason::Whitespace));
            }

            let Some((name, constraint)) = package.split_once('@') else {
                return Relation::any(package).map_err(|source| {
                    issue(AddPackageParseReason::InvalidPackageName {
                        details: source.to_string(),
                    })
                });
            };
            if name.is_empty() {
                return Err(issue(AddPackageParseReason::MissingName));
            }
            if constraint.is_empty() {
                return Err(issue(AddPackageParseReason::MissingConstraint));
            }

            let (operator, version) = [">=", "<=", "==", "!=", ">", "<"]
                .into_iter()
                .find_map(|operator| {
                    constraint
                        .strip_prefix(operator)
                        .map(|version| (operator, version))
                })
                .ok_or_else(|| issue(AddPackageParseReason::InvalidOperator))?;
            if version.is_empty() {
                return Err(issue(AddPackageParseReason::MissingVersion));
            }

            let version = version.parse::<Version>().map_err(|source| {
                issue(AddPackageParseReason::InvalidVersion {
                    version: version.to_string(),
                    details: source.to_string(),
                })
            })?;
            let requirement = match operator {
                ">=" => VersionRequirement::GreaterThanEqual(version),
                "<=" => VersionRequirement::LessThanEqual(version),
                "==" => VersionRequirement::Equal(version),
                "!=" => VersionRequirement::NotEqual(version),
                ">" => VersionRequirement::GreaterThan(version),
                "<" => VersionRequirement::LessThan(version),
                _ => unreachable!("constraint operator was selected from a fixed set"),
            };

            Relation::new(name, requirement).map_err(|source| {
                issue(AddPackageParseReason::InvalidPackageName {
                    details: source.to_string(),
                })
            })
        })
        .partition(Result::is_ok);

    if issues.is_empty() {
        Ok(relations.into_iter().filter_map(Result::ok).collect())
    } else {
        Err(AddPackagesParseError {
            issues: issues.into_iter().filter_map(Result::err).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_add_packages_accepts_supported_forms() {
        let inputs = [
            ("dplyr", "dplyr"),
            ("dplyr@>=1.0.0", "dplyr (>= 1.0.0)"),
            ("dplyr@<=1.0.0", "dplyr (<= 1.0.0)"),
            ("dplyr@==1.0.0", "dplyr (== 1.0.0)"),
            ("dplyr@!=1.0.0", "dplyr (!= 1.0.0)"),
            ("dplyr@>1.0.0", "dplyr (> 1.0.0)"),
            ("dplyr@<1.0.0", "dplyr (< 1.0.0)"),
        ];
        let packages = inputs
            .iter()
            .map(|(input, _)| (*input).to_string())
            .collect::<Vec<_>>();

        let parsed = parse_add_packages(&packages).expect("supported forms should parse");
        let expected = inputs
            .iter()
            .map(|(_, expected)| {
                expected
                    .parse::<Relation>()
                    .expect("expected relation should parse")
            })
            .collect::<BTreeSet<_>>();

        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_add_packages_reports_all_invalid_forms() {
        let packages = [
            "",
            "dplyr >= 1.0.0",
            "@>=1.0.0",
            "dplyr@",
            "dplyr@=1.0.0",
            "dplyr@>=",
            "dplyr@>= 1.0.0",
            "dplyr@>=1",
            "valid",
        ]
        .map(str::to_string);

        let error = parse_add_packages(&packages).expect_err("invalid forms should fail");

        assert_eq!(error.issues.len(), packages.len() - 1);
        assert!(matches!(
            &error.issues[0].reason,
            AddPackageParseReason::Empty
        ));
        assert!(matches!(
            &error.issues[1].reason,
            AddPackageParseReason::Whitespace
        ));
        assert!(matches!(
            &error.issues[2].reason,
            AddPackageParseReason::MissingName
        ));
        assert!(matches!(
            &error.issues[3].reason,
            AddPackageParseReason::MissingConstraint
        ));
        assert!(matches!(
            &error.issues[4].reason,
            AddPackageParseReason::InvalidOperator
        ));
        assert!(matches!(
            &error.issues[5].reason,
            AddPackageParseReason::MissingVersion
        ));
        assert!(matches!(
            &error.issues[6].reason,
            AddPackageParseReason::Whitespace
        ));
        assert!(matches!(
            &error.issues[7].reason,
            AddPackageParseReason::InvalidVersion { version, .. } if version == "1"
        ));
    }
}
