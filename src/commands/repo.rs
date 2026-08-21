use crate::{
    cli::{
        RepoAdditionalAddArgs, RepoAdditionalCommands, RepoAdditionalRemoveArgs, RepoBaseCommands,
        RepoBaseResetArgs, RepoBaseSetArgs, RepoCommands, RepoListArgs, RepoRemoteArgs,
        RepoRemoteCommands, RepositoryType,
    },
    description::{
        BASE_REPOSITORY_FIELD, DescriptionParseError, DescriptionReadError,
        RepositoryMutationError, add_additional_repository, add_remote_repository,
        additional_repositories, base_repository, read_description, remotes,
        remove_additional_repository, remove_remote_repository, reset_base_repository,
        set_base_repository,
    },
    http,
    output::status,
    project::{
        Project, ProjectDiscoveryError, ProjectWriteError, ResolutionPolicy, ResolveProjectError,
        find_project_root, resolve_project, write_project_files,
    },
    repository::{RepositoryError, built_in_repository_url, parse_repository_url},
};
use miette::Diagnostic;
use r_description::{FieldMutationError, PositionedRemoteParseError, RDescription, Remote, Url};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectDiscovery(#[from] ProjectDiscoveryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionRead(#[from] DescriptionReadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error("failed to update {BASE_REPOSITORY_FIELD}: {source}")]
    #[diagnostic(code(rpx::repo::description_update_failed))]
    BaseMutation {
        #[source]
        source: FieldMutationError,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    RepositoryMutation(#[from] RepositoryMutationError),

    #[error("invalid remote repository {remote}: {source}")]
    #[diagnostic(code(rpx::repo::invalid_remote))]
    RemoteParse {
        remote: String,
        #[source]
        source: PositionedRemoteParseError,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectWrite(#[from] ProjectWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Resolve(Box<ResolveProjectError>),

    #[error("invalid {action} repository URL {url}: {source}")]
    #[diagnostic(code(rpx::repo::invalid_url))]
    RepositoryUrl {
        action: &'static str,
        url: String,
        #[source]
        source: Box<RepositoryError>,
    },

    #[error("failed to remove repository credential: {details}")]
    #[diagnostic(code(rpx::repo::credential_remove_failed))]
    CredentialRemove { details: String },
}

impl From<ResolveProjectError> for Error {
    fn from(error: ResolveProjectError) -> Self {
        Self::Resolve(Box::new(error))
    }
}

pub(crate) async fn run(command: RepoCommands) -> Result<(), Error> {
    match command {
        RepoCommands::Add(args) => add_additional(args).await,
        RepoCommands::Remove(args) => remove_additional(args).await,
        RepoCommands::List(args) => list(args),
        RepoCommands::Base {
            command: RepoBaseCommands::Set(args),
        } => set_base(args).await,
        RepoCommands::Base {
            command: RepoBaseCommands::Reset(args),
        } => reset_base(args).await,
        RepoCommands::Additional {
            command: RepoAdditionalCommands::Add(args),
        } => add_additional(args).await,
        RepoCommands::Additional {
            command: RepoAdditionalCommands::Remove(args),
        } => remove_additional(args).await,
        RepoCommands::Remote {
            command: RepoRemoteCommands::Add(args),
        } => add_remote(args).await,
        RepoCommands::Remote {
            command: RepoRemoteCommands::Remove(args),
        } => remove_remote(args).await,
    }
}

async fn set_base(args: RepoBaseSetArgs) -> Result<(), Error> {
    let project_path = find_project_root()?;
    let mut description = read_description(&project_path)?;
    let repository = repository_url("base", &args.url)?;
    let previous = base_repository(&project_path, &description)?;
    if previous.as_ref() == Some(&repository) {
        status(format_args!(
            "Base repository already configured: {repository}"
        ));
        return Ok(());
    }

    set_base_repository(&mut description, &repository)
        .map_err(|source| Error::BaseMutation { source })?;
    relock_and_write(&project_path, &description).await?;
    match previous {
        Some(previous) => status(format_args!(
            "Replaced base repository {previous} with {repository}"
        )),
        None => status(format_args!("Set base repository {repository}")),
    }
    Ok(())
}

async fn reset_base(args: RepoBaseResetArgs) -> Result<(), Error> {
    let project_path = find_project_root()?;
    let mut description = read_description(&project_path)?;
    let previous = base_repository(&project_path, &description)?;
    let Some(previous) = previous else {
        status("Base repository is already reset");
        return Ok(());
    };

    reset_base_repository(&mut description);
    relock_and_write(&project_path, &description).await?;
    if args.remove_credential {
        remove_credential_for(&previous)?;
    }
    let effective = built_in_repository_url();
    status(format_args!(
        "Reset base repository from {previous} to {effective}"
    ));
    Ok(())
}

async fn add_additional(args: RepoAdditionalAddArgs) -> Result<(), Error> {
    let project_path = find_project_root()?;
    let mut description = read_description(&project_path)?;
    let repository = repository_url("additional", &args.url)?;
    if !add_additional_repository(&project_path, &mut description, repository.clone())? {
        status(format_args!("Repository already configured: {repository}"));
        return Ok(());
    }

    relock_and_write(&project_path, &description).await?;
    status(format_args!("Added additional repository {repository}"));
    Ok(())
}

async fn remove_additional(args: RepoAdditionalRemoveArgs) -> Result<(), Error> {
    let project_path = find_project_root()?;
    let mut description = read_description(&project_path)?;
    let repository = repository_url("additional", &args.url)?;
    if !remove_additional_repository(&project_path, &mut description, &repository)? {
        if args.remove_credential {
            remove_credential_for(&repository)?;
            status(format_args!("Removed stored credential for {repository}"));
        }
        status(format_args!("Repository not configured: {repository}"));
        return Ok(());
    }

    relock_and_write(&project_path, &description).await?;
    if args.remove_credential {
        remove_credential_for(&repository)?;
    }
    status(format_args!("Removed additional repository {repository}"));
    Ok(())
}

async fn add_remote(args: RepoRemoteArgs) -> Result<(), Error> {
    let project_path = find_project_root()?;
    let mut description = read_description(&project_path)?;
    let remote = parse_remote(&args.remote)?;
    let normalized = remote.to_string();
    if !add_remote_repository(&project_path, &mut description, remote)? {
        status(format_args!("Remote already configured: {normalized}"));
        return Ok(());
    }

    relock_and_write(&project_path, &description).await?;
    status(format_args!("Added remote repository {normalized}"));
    Ok(())
}

async fn remove_remote(args: RepoRemoteArgs) -> Result<(), Error> {
    let project_path = find_project_root()?;
    let mut description = read_description(&project_path)?;
    let remote = parse_remote(&args.remote)?;
    let normalized = remote.to_string();
    if !remove_remote_repository(&project_path, &mut description, &remote)? {
        status(format_args!("Remote not configured: {normalized}"));
        return Ok(());
    }

    relock_and_write(&project_path, &description).await?;
    status(format_args!("Removed remote repository {normalized}"));
    Ok(())
}

fn list(args: RepoListArgs) -> Result<(), Error> {
    let project_path = find_project_root()?;
    let description = read_description(&project_path)?;
    let configured_base = base_repository(&project_path, &description)?;
    let configured_remotes = remotes(&project_path, &description)?;
    let configured_additional = additional_repositories(&project_path, &description)?;

    status(format_args!("{:<12}{:<14}REPOSITORY", "TYPE", "SOURCE"));
    if args
        .repository_type
        .is_none_or(|filter| filter == RepositoryType::Base)
    {
        match configured_base {
            Some(repository) => status(format_args!(
                "{:<12}{:<14}{repository}",
                "base", "configured"
            )),
            None => status(format_args!(
                "{:<12}{:<14}{}",
                "base",
                "built-in",
                built_in_repository_url()
            )),
        }
    }
    if args
        .repository_type
        .is_none_or(|filter| filter == RepositoryType::Remote)
    {
        configured_remotes.into_iter().for_each(|remote| {
            status(format_args!(
                "{:<12}{:<14}{remote}",
                "remote", "DESCRIPTION"
            ));
        });
    }
    if args
        .repository_type
        .is_none_or(|filter| filter == RepositoryType::Additional)
    {
        configured_additional.into_iter().for_each(|repository| {
            status(format_args!(
                "{:<12}{:<14}{repository}",
                "additional", "DESCRIPTION"
            ));
        });
    }
    Ok(())
}

async fn relock_and_write(path: &Path, description: &RDescription) -> Result<(), Error> {
    let project = Project {
        root: path.to_path_buf(),
        description: description.clone(),
    };
    let resolution = resolve_project(&project, ResolutionPolicy::AlwaysResolve).await?;
    write_project_files(
        &project.root,
        Some(&project.description),
        &resolution.lockfile,
    )?;
    Ok(())
}

fn repository_url(action: &'static str, value: &str) -> Result<Url, Error> {
    parse_repository_url(value).map_err(|source| Error::RepositoryUrl {
        action,
        url: value.trim().to_string(),
        source: Box::new(source),
    })
}

fn parse_remote(value: &str) -> Result<Remote, Error> {
    value.parse().map_err(|source| Error::RemoteParse {
        remote: value.to_string(),
        source,
    })
}

fn remove_credential_for(repository: &Url) -> Result<(), Error> {
    http::remove_stored_credential(repository).map_err(|error| Error::CredentialRemove {
        details: error.to_string(),
    })
}
