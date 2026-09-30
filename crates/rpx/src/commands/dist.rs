use crate::{
    cli::{DistBuildArgs, DistCommands, DistPublishArgs},
    description::{DescriptionParseError, ProjectType, project_type, root_package},
    output::status,
    project::{
        LibraryMismatches, LoadProjectResolutionError, ProjectLoadError, library_mismatches,
        load_project, load_project_resolution, project_library_path,
    },
    r::{BasePackagesError, InstalledPackagesError, base_packages, installed_packages},
};
use miette::Diagnostic;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLoad(#[from] ProjectLoadError),

    #[error("distribution requires an installable R package")]
    #[diagnostic(code(rpx::dist::not_a_package))]
    NotAPackage,

    #[error(transparent)]
    #[diagnostic(transparent)]
    Description(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    LoadResolution(#[from] LoadProjectResolutionError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    BasePackages(#[from] BasePackagesError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InstalledPackages(#[from] InstalledPackagesError),

    #[error("project library has not been synced for distribution")]
    #[diagnostic(
        code(rpx::dist::library_out_of_sync),
        help("Run `rpx sync` before `rpx dist build`.")
    )]
    LibraryMissing,

    #[error("project library is not synced for distribution\n\n{mismatches}")]
    #[diagnostic(
        code(rpx::dist::library_out_of_sync),
        help("Run `rpx sync` before `rpx dist build`.")
    )]
    LibraryOutOfSync { mismatches: LibraryMismatches },

    #[error("failed to build source package: {source}")]
    #[diagnostic(code(rpx::dist::build_failed))]
    Build {
        #[source]
        source: rpx_dist::BuildError,
    },

    #[error("no publish destination configured")]
    #[diagnostic(
        code(rpx::dist::repository_missing),
        help("Set `Repository: acme/internal` in DESCRIPTION, RREPO_REPOSITORY, or --repository.")
    )]
    RepositoryMissing,

    #[error("RREPO_REPOSITORY must be valid UTF-8")]
    #[diagnostic(code(rpx::dist::repository_invalid_env))]
    InvalidRepositoryEnv,

    #[error("invalid rrepo repository `{value}`: {source}")]
    #[diagnostic(code(rpx::dist::repository_invalid))]
    InvalidRepository {
        value: String,
        #[source]
        source: rpx_dist::RepositorySlugError,
    },

    #[error("RREPO_API_KEY must be set to a packages:write API key")]
    #[diagnostic(code(rpx::dist::api_key_missing))]
    ApiKeyMissing,

    #[error("failed to publish source package: {source}")]
    #[diagnostic(code(rpx::dist::publish_failed))]
    Publish {
        #[source]
        source: rpx_dist::PublishError,
    },
}

pub(crate) async fn run(command: DistCommands) -> Result<(), Error> {
    match command {
        DistCommands::Build(args) => build(args).await,
        DistCommands::Publish(args) => publish(args).await,
    }
}

fn publish_repository(
    explicit: Option<String>,
    environment: Option<String>,
    description: &r_description::Description,
) -> Result<rpx_dist::RepositorySlug, Error> {
    let value = explicit
        .or(environment)
        .or_else(|| {
            description
                .repository()
                .map(|value| value.as_str().to_string())
        })
        .ok_or(Error::RepositoryMissing)?;
    value
        .parse()
        .map_err(|source| Error::InvalidRepository { value, source })
}

async fn publish(args: DistPublishArgs) -> Result<(), Error> {
    let project = load_project()?;
    if project_type(&project.description) != ProjectType::Package {
        return Err(Error::NotAPackage);
    }
    let (package, version) = root_package(&project.root, &project.description)?;
    let environment = if args.repository.is_some() {
        None
    } else {
        match std::env::var("RREPO_REPOSITORY") {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotUnicode(_)) => return Err(Error::InvalidRepositoryEnv),
            Err(std::env::VarError::NotPresent) => None,
        }
    };
    let repository = publish_repository(args.repository, environment, &project.description)?;
    let api_key = std::env::var("RREPO_API_KEY").map_err(|_| Error::ApiKeyMissing)?;
    if api_key.trim().is_empty() {
        return Err(Error::ApiKeyMissing);
    }
    let artifact = args.artifact.unwrap_or_else(|| {
        project
            .root
            .join("dist")
            .join(format!("{package}_{version}.tar.gz"))
    });
    rpx_dist::publish(rpx_dist::PublishRequest {
        artifact,
        package: package.clone(),
        version: version.clone(),
        repository: repository.clone(),
        api_key,
    })
    .await
    .map_err(|source| Error::Publish { source })?;
    status(format_args!(
        "Published {package} {version} to {repository}"
    ));
    Ok(())
}

async fn build(args: DistBuildArgs) -> Result<(), Error> {
    let project = load_project()?;
    if project_type(&project.description) != ProjectType::Package {
        return Err(Error::NotAPackage);
    }
    let (package, version) = root_package(&project.root, &project.description)?;
    let resolution = load_project_resolution(&project).await?;
    let base_packages = base_packages().await?;
    let expected = resolution
        .lockfile
        .packages
        .iter()
        .filter(|(name, _)| !base_packages.contains(*name))
        .map(|(name, package)| (name.clone(), package.version.clone()))
        .collect::<BTreeMap<_, _>>();
    let project_library = project_library_path(&project.root);
    if !project_library.is_dir() {
        return Err(Error::LibraryMissing);
    }
    let installed = installed_packages(&project_library).await?;
    let mismatches = library_mismatches(&expected, &installed, Some(&package));
    if !mismatches.is_exact() {
        return Err(Error::LibraryOutOfSync { mismatches });
    }
    let output_dir = args.output_dir.unwrap_or_else(|| project.root.join("dist"));
    let artifact = rpx_dist::build(rpx_dist::BuildRequest {
        package_root: project.root,
        package,
        version,
        output_dir,
        project_library,
    })
    .await
    .map_err(|source| Error::Build { source })?;
    status(format_args!(
        "Built {} {} at {}",
        artifact.package,
        artifact.version,
        artifact.path.display()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use r_description::Description;

    #[test]
    fn publish_destination_uses_explicit_then_environment_then_description() {
        let description =
            Description::parse("Package: fixture\nVersion: 1.0.0\nRepository: acme/production\n");
        assert_eq!(
            publish_repository(None, None, &description)
                .unwrap()
                .to_string(),
            "acme/production"
        );
        assert_eq!(
            publish_repository(None, Some("acme/staging".into()), &description)
                .unwrap()
                .to_string(),
            "acme/staging"
        );
        assert_eq!(
            publish_repository(
                Some("acme/custom".into()),
                Some("CRAN".into()),
                &description
            )
            .unwrap()
            .to_string(),
            "acme/custom"
        );
        assert!(matches!(
            publish_repository(None, None, &Description::parse("Package: fixture\n")),
            Err(Error::RepositoryMissing)
        ));
        assert!(matches!(
            publish_repository(None, None, &Description::parse("Repository: CRAN\n")),
            Err(Error::InvalidRepository { .. })
        ));
    }
}
