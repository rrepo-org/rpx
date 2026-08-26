use clap::Parser;
use tracing_indicatif::{
    filter::{IndicatifFilter, hide_indicatif_span_fields},
    style::ProgressStyle,
};
use tracing_subscriber::{
    EnvFilter,
    fmt::format::DefaultFields,
    layer::{Layer, SubscriberExt},
    util::SubscriberInitExt,
};

mod cache;
mod cli;
mod commands;
mod description;
mod git;
mod http;
mod lockfile;
mod output;
mod project;
mod r;
mod repository;
mod resolver;
mod sync;
mod ui;

use cli::{Cli, Commands};
use commands::{
    add, clean, init, lock, remove, repo, run as run_command, status as status_command,
    sync as sync_command,
};
use ui::progress_spinner_style;

/// Runs the CLI application.
///
/// # Errors
///
/// Returns an error when command execution or diagnostic rendering fails.
pub async fn run() -> miette::Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init(args) => init::run(args).await?,
        Commands::Add(args) => add::run(args).await?,
        Commands::Remove(args) => remove::run(args).await?,
        Commands::Run(args) => run_command::run(args).await?,
        Commands::Lock {} => lock::run().await?,
        Commands::Status => status_command::run().await?,
        Commands::Sync(args) => sync_command::run(args).await?,
        Commands::Clean => clean::run()?,
        Commands::Repo { command } => repo::run(command).await?,
    }

    Ok(())
}

fn init_tracing() {
    let indicatif_layer = tracing_indicatif::IndicatifLayer::new()
        .with_span_field_formatter(hide_indicatif_span_fields(DefaultFields::new()))
        .with_progress_style(progress_spinner_style())
        .with_max_progress_bars(
            10,
            Some(
                ProgressStyle::with_template("...and {pending_progress_bars} more packages")
                    .expect("progress footer style should be valid"),
            ),
        );
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,reqwest_tracing=info,rpx=info"));
    let fmt_layer =
        tracing_subscriber::fmt::layer().with_writer(indicatif_layer.get_stderr_writer());

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(indicatif_layer.with_filter(IndicatifFilter::new(false)))
        .try_init();
}

#[cfg(test)]
mod tests {
    use crate::project::RequiredPackages;
    use crate::{
        git::GitError,
        project::{LockfileBuildError, ResolveProjectError, lockfile_from_resolution},
        r::BasePackagesError,
        repository::{LocalRepository, PackageRepository, RepositoryError, built_in_repository},
        resolver::{PackageVersion, ProviderError, RDependencyProvider, ResolutionError},
    };
    use miette::Diagnostic;
    use pubgrub::{DerivationTree, External, PubGrubError, Ranges};
    use r_description::{RDescription, Relation, Version};
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
        sync::Arc,
    };

    fn version(value: &str) -> Version {
        value.parse().expect("version fixture should parse")
    }

    fn relation(value: &str) -> Relation {
        value.parse().expect("relation fixture should parse")
    }

    fn required_packages(packages: &[(&str, &str)]) -> RequiredPackages {
        packages
            .iter()
            .map(|(name, fields)| {
                let description =
                    RDescription::parse(&format!("Package: {name}\nVersion: 1.0.0\n{fields}"));
                (
                    (*name).to_string(),
                    (
                        PackageVersion::new(version("1.0.0"), built_in_repository()),
                        Arc::new(description),
                    ),
                )
            })
            .collect()
    }

    fn git_access_error(remote: &str) -> RepositoryError {
        RepositoryError::Git {
            repository: remote.to_string(),
            source: Arc::new(GitError::Access {
                remote: remote.to_string(),
                source: git2::Error::from_str("access denied"),
            }),
        }
    }

    fn ordinary_repository_error() -> RepositoryError {
        RepositoryError::InvalidData {
            resource: "fixture".to_string(),
            details: "invalid".to_string(),
        }
    }

    #[test]
    fn resolve_project_error_maps_resolution_categories() {
        let source: PubGrubError<RDependencyProvider> = PubGrubError::NoSolution(
            DerivationTree::External(External::NoVersions("missing".into(), Ranges::empty())),
        );
        let error = ResolveProjectError::from(ResolutionError::PubGrub(source));
        assert!(matches!(
            &error,
            ResolveProjectError::NoSolution { explanation } if explanation.contains("missing")
        ));
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("rpx::lock::no_solution")
        );

        for provider in [
            ProviderError::Repository(git_access_error("https://example.test/repo.git")),
            ProviderError::DependencyMetadata {
                repository: "fixture".into(),
                source: git_access_error("ssh://git@example.test/repo.git"),
            },
        ] {
            assert!(matches!(
                ResolveProjectError::from(ResolutionError::Provider(provider)),
                ResolveProjectError::GitRepositoryUnavailable {
                    repository,
                    source,
                }
                    if repository.contains("example.test/repo.git")
                        && matches!(source.as_ref(), ResolutionError::Provider(_))
            ));
        }
        let base = BasePackagesError::InvalidUtf8 {
            source: String::from_utf8(vec![0xff]).expect_err("invalid UTF-8 fixture"),
        };
        assert!(matches!(
            ResolveProjectError::from(ResolutionError::BasePackages(base)),
            ResolveProjectError::BasePackages(_)
        ));
        assert!(matches!(
            ResolveProjectError::from(ResolutionError::Provider(ProviderError::Repository(
                ordinary_repository_error()
            ))),
            ResolveProjectError::Resolution { .. }
        ));
    }

    #[test]
    fn cran_index_diagnostic_survives_pubgrub_resolution_errors() {
        let parse_error = crate::http::CranPackagesIndex::parse(
            "https://example.test/src/contrib/PACKAGES",
            "Package: fixture\nVersion: invalid\n".to_string(),
        )
        .expect_err("invalid CRAN version should fail");
        let source = PubGrubError::ErrorChoosingVersion {
            package: "fixture".to_string(),
            source: ProviderError::Repository(RepositoryError::CranPackages(Box::new(parse_error))),
        };

        let error = ResolveProjectError::from(ResolutionError::from(source));

        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("rpx::repository::cran_packages_parse_failed")
        );
        assert!(error.source_code().is_some());
        let help = error
            .help()
            .expect("CRAN parse diagnostic should retain repository guidance")
            .to_string();
        assert!(help.contains("repository returned invalid metadata"));
        assert!(!help.contains("DESCRIPTION"));
        assert!(matches!(
            error,
            ResolveProjectError::Repository(RepositoryError::CranPackages(_))
        ));
    }

    #[test]
    fn lockfile_build_error_maps_repository_categories() {
        assert!(matches!(
            LockfileBuildError::from(git_access_error("https://example.test/private.git")),
            LockfileBuildError::GitRepositoryUnavailable { repository, .. }
                if repository == "https://example.test/private.git"
        ));
        assert!(matches!(
            LockfileBuildError::from(ordinary_repository_error()),
            LockfileBuildError::Repository { .. }
        ));
    }

    #[tokio::test]
    async fn lockfile_from_resolution_assembles_current_metadata() {
        let requirements = BTreeSet::from([relation("selected (>= 1.0.0)")]);
        let resolved = required_packages(&[(
            "selected",
            "Depends: R (>= 4.4), hardDepends\nImports: hardImports\nLinkingTo: hardLinking\nSuggests: optional\n",
        )]);
        let lockfile = lockfile_from_resolution(
            requirements.clone(),
            &resolved,
            &[built_in_repository()],
            &semver::Version::new(4, 5, 1),
        )
        .await
        .expect("resolution should lock");
        let package = &lockfile.packages["selected"];
        assert_eq!(lockfile.requirements, requirements);
        assert_eq!(lockfile.r, semver::Version::new(4, 5, 1));
        assert_eq!(lockfile.repos.len(), 1);
        assert_eq!(lockfile.repos[0].url(), &package.repository);
        assert_eq!(package.version, version("1.0.0"));
        assert_eq!(
            package.dependencies,
            BTreeSet::from([
                relation("R (>= 4.4)"),
                relation("hardDepends"),
                relation("hardImports"),
                relation("hardLinking"),
            ])
        );
    }

    #[tokio::test]
    async fn lockfile_from_resolution_rejects_unprovided_repository() {
        let local: Arc<dyn PackageRepository> =
            Arc::new(LocalRepository::new(PathBuf::from("vendor/selected")));
        let resolved = BTreeMap::from([(
            "selected".into(),
            (
                PackageVersion::new(version("1.0.0"), local),
                Arc::new(RDescription::parse("Package: selected\nVersion: 1.0.0\n")),
            ),
        )]);
        assert!(matches!(
            lockfile_from_resolution(
                BTreeSet::new(), &resolved, &[built_in_repository()],
                &semver::Version::new(4, 5, 0),
            ).await,
            Err(LockfileBuildError::UnsupportedRepository { repository })
                if repository == "vendor/selected"
        ));
    }

    #[tokio::test]
    async fn lockfile_from_resolution_rejects_invalid_metadata() {
        for field in ["Depends", "Imports", "LinkingTo"] {
            let metadata = format!("{field}: broken (>= invalid)\n");
            let resolved = required_packages(&[("selected", &metadata)]);
            assert!(matches!(
                lockfile_from_resolution(
                    BTreeSet::new(),
                    &resolved,
                    &[built_in_repository()],
                    &semver::Version::new(4, 5, 0),
                )
                .await,
                Err(LockfileBuildError::InvalidPackageRequirements { .. })
            ));
        }
    }
}
