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
    PackageParse(#[from] AddPackageParseError),

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
    let added_relations = packages
        .iter()
        .map(|package| parse_add_package(package))
        .collect::<Result<BTreeSet<_>, _>>()?;
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
#[error("invalid package constraint {package}: {details}")]
#[diagnostic(
    code(rpx::add::invalid_constraint),
    help("Use PACKAGE@OPERATORVERSION, for example digest@>=0.6.37.")
)]
pub(crate) struct AddPackageParseError {
    package: String,
    details: String,
}

fn parse_add_package(package: &str) -> Result<Relation, AddPackageParseError> {
    if package.is_empty() || package.chars().any(char::is_whitespace) {
        return Err(AddPackageParseError {
            package: package.to_string(),
            details: "package specifications cannot contain whitespace".to_string(),
        });
    }

    let Some((name, constraint)) = package.split_once('@') else {
        return Relation::any(package).map_err(|source| AddPackageParseError {
            package: package.to_string(),
            details: source.to_string(),
        });
    };
    if name.is_empty() {
        return Err(AddPackageParseError {
            package: package.to_string(),
            details: "package name is missing".to_string(),
        });
    }

    let (operator, version) = [">=", "<=", "==", "!=", ">", "<"]
        .into_iter()
        .find_map(|operator| {
            constraint
                .strip_prefix(operator)
                .map(|version| (operator, version))
        })
        .ok_or_else(|| AddPackageParseError {
            package: package.to_string(),
            details: "version constraint operator is missing or invalid".to_string(),
        })?;
    if version.is_empty() {
        return Err(AddPackageParseError {
            package: package.to_string(),
            details: "version is missing".to_string(),
        });
    }

    let version = version
        .parse::<Version>()
        .map_err(|source| AddPackageParseError {
            package: package.to_string(),
            details: source.to_string(),
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

    Relation::new(name, requirement).map_err(|source| AddPackageParseError {
        package: package.to_string(),
        details: source.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_add_package_accepts_supported_forms() {
        for (input, expected) in [
            ("dplyr", "dplyr"),
            ("dplyr@>=1.0.0", "dplyr (>= 1.0.0)"),
            ("dplyr@<=1.0.0", "dplyr (<= 1.0.0)"),
            ("dplyr@==1.0.0", "dplyr (== 1.0.0)"),
            ("dplyr@!=1.0.0", "dplyr (!= 1.0.0)"),
            ("dplyr@>1.0.0", "dplyr (> 1.0.0)"),
            ("dplyr@<1.0.0", "dplyr (< 1.0.0)"),
        ] {
            let parsed = parse_add_package(input).expect("supported form should parse");
            assert_eq!(parsed.package(), "dplyr");
            assert_eq!(parsed.to_string(), expected);
        }
    }

    #[test]
    fn parse_add_package_rejects_invalid_forms() {
        for input in [
            "",
            "dplyr >= 1.0.0",
            "@>=1.0.0",
            "dplyr@=1.0.0",
            "dplyr@>=",
            "dplyr@>= 1.0.0",
        ] {
            assert!(parse_add_package(input).is_err(), "{input:?} should fail");
        }
    }
}
