use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::description::DependencyField;

#[derive(Parser, Debug)]
#[command(name = "rpx")]
#[command(
    version,
    about = "Manage R project dependencies with DESCRIPTION and rpx.lock",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(
        about = "Initialize an R project",
        long_about = "Initialize an R package project in a new or empty target directory."
    )]
    Init(InitArgs),

    #[command(
        about = "Install one or more packages",
        long_about = "Install one or more packages for this project. Each package is recorded in DESCRIPTION, then rpx regenerates rpx.lock and syncs the project library."
    )]
    Add {
        #[command(flatten)]
        dependency_type: AddDependencyTypeArgs,

        #[arg(
            help = "Packages to add, optionally with a constraint such as digest@>=0.6.37",
            value_name = "PACKAGE[@CONSTRAINTVERSION]",
            required = true
        )]
        packages: Vec<String>,
    },

    #[command(
        about = "Remove one or more packages",
        long_about = "Remove one or more packages from this project. The packages are removed from DESCRIPTION, the project library is synced, and rpx regenerates rpx.lock."
    )]
    Remove {
        #[arg(
            help = "Package names to remove from the project's dependencies",
            value_name = "PACKAGE",
            required = true
        )]
        packages: Vec<String>,
    },

    #[command(
        about = "Run a command in the project environment",
        long_about = "Run a command with this project's isolated R package library activated."
    )]
    Run {
        #[arg(
            help = "Command and arguments to run inside the project environment",
            value_name = "COMMAND",
            trailing_var_arg = true,
            allow_hyphen_values = true,
            required = true
        )]
        command: Vec<String>,
    },

    #[command(
        about = "Resolve project dependencies",
        long_about = "Resolve project dependencies from DESCRIPTION and write the resolved package set to rpx.lock without installing packages."
    )]
    Lock {},

    #[command(
        about = "Check project dependency state",
        long_about = "Check whether DESCRIPTION, rpx.lock, and the project library are in sync."
    )]
    Status,

    #[command(
        about = "Install the locked package set",
        long_about = "Install the exact package set recorded in rpx.lock into the project library."
    )]
    Sync {
        #[arg(
            long,
            conflicts_with = "install_only_system",
            help = "Install missing system dependencies before syncing R packages"
        )]
        install_system: bool,

        #[arg(
            long,
            conflicts_with = "install_system",
            help = "Install only missing system dependencies and stop"
        )]
        install_only_system: bool,
    },

    #[command(
        about = "Remove project library and caches",
        long_about = "Remove this project's isolated library and wipe rpx cache directories so the next sync or add starts from a clean local state."
    )]
    Clean,

    #[command(about = "Manage package repositories")]
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
}

#[derive(Args, Debug)]
pub struct InitArgs {
    #[arg(
        help = "Directory to initialize",
        value_name = "PATH",
        value_hint = clap::ValueHint::DirPath
    )]
    pub path: Option<PathBuf>,

    #[arg(long, help = "Package name; defaults to the target directory name")]
    pub name: Option<String>,

    #[arg(long, help = "Package title")]
    pub title: Option<String>,

    #[arg(long, help = "Package description")]
    pub description: Option<String>,

    #[arg(long, help = "Package author name")]
    pub author_name: Option<String>,

    #[arg(long, help = "Package author email")]
    pub author_email: Option<String>,

    #[arg(long, value_enum, help = "Package license")]
    pub license: Option<InitLicense>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InitLicense {
    #[value(name = "mit")]
    Mit,
    #[value(name = "apache-2.0")]
    Apache2,
    #[value(name = "gpl-2")]
    Gpl2,
    #[value(name = "gpl-3")]
    Gpl3,
    #[value(name = "agpl-3")]
    Agpl3,
    #[value(name = "lgpl-2.1")]
    Lgpl21,
    #[value(name = "lgpl-3")]
    Lgpl3,
    #[value(name = "cc0")]
    Cc0,
    #[value(name = "cc-by-4.0")]
    CcBy4,
    #[value(name = "proprietary")]
    Proprietary,
}

#[derive(Args, Debug)]
#[group(multiple = false)]
#[allow(clippy::struct_excessive_bools)]
pub struct AddDependencyTypeArgs {
    #[arg(long, help = "Add packages to Depends")]
    depends: bool,

    #[arg(long, help = "Add packages to Imports (default)")]
    imports: bool,

    #[arg(long, help = "Add packages to LinkingTo")]
    linking_to: bool,

    #[arg(long, visible_alias = "dev", help = "Add packages to Suggests")]
    suggests: bool,
}

impl From<AddDependencyTypeArgs> for DependencyField {
    fn from(args: AddDependencyTypeArgs) -> Self {
        [
            (args.depends, Self::Depends),
            (args.imports, Self::Imports),
            (args.linking_to, Self::LinkingTo),
            (args.suggests, Self::Suggests),
        ]
        .into_iter()
        .find_map(|(selected, field)| selected.then_some(field))
        .unwrap_or_default()
    }
}

#[derive(Subcommand, Debug)]
pub enum RepoCommands {
    #[command(about = "Add an additional repository (shortcut)")]
    Add(RepoAdditionalAddArgs),

    #[command(about = "Remove an additional repository (shortcut)")]
    Remove(RepoAdditionalRemoveArgs),

    #[command(about = "List configured repositories")]
    List(RepoListArgs),

    #[command(about = "Manage the base repository")]
    Base {
        #[command(subcommand)]
        command: RepoBaseCommands,
    },

    #[command(about = "Manage additional repositories")]
    Additional {
        #[command(subcommand)]
        command: RepoAdditionalCommands,
    },

    #[command(about = "Manage remote package repositories")]
    Remote {
        #[command(subcommand)]
        command: RepoRemoteCommands,
    },
}

#[derive(Subcommand, Debug)]
pub enum RepoBaseCommands {
    #[command(about = "Set the configured base repository")]
    Set(RepoBaseSetArgs),

    #[command(about = "Reset the configured base repository")]
    Reset(RepoBaseResetArgs),
}

#[derive(Subcommand, Debug)]
pub enum RepoAdditionalCommands {
    #[command(about = "Add an additional repository")]
    Add(RepoAdditionalAddArgs),

    #[command(about = "Remove an additional repository")]
    Remove(RepoAdditionalRemoveArgs),
}

#[derive(Subcommand, Debug)]
pub enum RepoRemoteCommands {
    #[command(about = "Add a remote package repository")]
    Add(RepoRemoteArgs),

    #[command(about = "Remove a remote package repository")]
    Remove(RepoRemoteArgs),
}

#[derive(Args, Debug)]
pub struct RepoAdditionalAddArgs {
    #[arg(help = "Repository base URL", value_name = "URL", required = true)]
    pub url: String,
}

#[derive(Args, Debug)]
pub struct RepoAdditionalRemoveArgs {
    #[arg(help = "Repository base URL", value_name = "URL", required = true)]
    pub url: String,

    #[arg(
        long,
        help = "Also remove any stored API key for this repository's origin"
    )]
    pub remove_credential: bool,
}

#[derive(Args, Debug)]
pub struct RepoListArgs {
    #[arg(long = "type", value_enum, help = "Show only one repository class")]
    pub repository_type: Option<RepositoryType>,
}

#[derive(Args, Debug)]
pub struct RepoBaseSetArgs {
    #[arg(help = "Repository base URL", value_name = "URL", required = true)]
    pub url: String,
}

#[derive(Args, Debug)]
pub struct RepoBaseResetArgs {
    #[arg(
        long,
        help = "Also remove any stored API key for the configured repository's origin"
    )]
    pub remove_credential: bool,
}

#[derive(Args, Debug)]
pub struct RepoRemoteArgs {
    #[arg(
        help = "Remote repository specification",
        value_name = "REMOTE",
        required = true
    )]
    pub remote: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RepositoryType {
    Base,
    Additional,
    Remote,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(arguments: &[&str]) -> Commands {
        Cli::try_parse_from(arguments)
            .expect("command should parse")
            .command
    }

    #[test]
    fn parses_init_defaults() {
        assert!(matches!(
            parse(&["rpx", "init"]),
            Commands::Init(InitArgs {
                path: None,
                name: None,
                title: None,
                description: None,
                author_name: None,
                author_email: None,
                license: None,
            })
        ));
    }

    #[test]
    fn parses_init_target_and_metadata() {
        assert!(matches!(
            parse(&[
                "rpx",
                "init",
                "projects/example",
                "--name",
                "example.pkg",
                "--title",
                "Example Package",
                "--description",
                "An example package.",
                "--author-name",
                "Example Author",
                "--author-email",
                "author@example.com",
                "--license",
                "apache-2.0",
            ]),
            Commands::Init(InitArgs {
                path: Some(path),
                name: Some(name),
                title: Some(title),
                description: Some(description),
                author_name: Some(author_name),
                author_email: Some(author_email),
                license: Some(license),
            }) if path == PathBuf::from("projects/example")
                && name == "example.pkg"
                && title == "Example Package"
                && description == "An example package."
                && author_name == "Example Author"
                && author_email == "author@example.com"
                && license == InitLicense::Apache2
        ));
    }

    #[test]
    fn parses_add_dependency_fields() {
        for (flag, expected) in [
            (None, DependencyField::Imports),
            (Some("--depends"), DependencyField::Depends),
            (Some("--imports"), DependencyField::Imports),
            (Some("--linking-to"), DependencyField::LinkingTo),
            (Some("--suggests"), DependencyField::Suggests),
            (Some("--dev"), DependencyField::Suggests),
        ] {
            let mut arguments = vec!["rpx", "add"];
            arguments.extend(flag);
            arguments.push("digest@>=0.6.37");

            let Commands::Add {
                packages,
                dependency_type,
            } = parse(&arguments)
            else {
                panic!("add command should parse");
            };

            assert_eq!(packages, ["digest@>=0.6.37"]);
            assert_eq!(DependencyField::from(dependency_type), expected);
        }
    }

    #[test]
    fn rejects_multiple_add_dependency_fields() {
        let flags = ["--depends", "--imports", "--linking-to", "--suggests"];

        assert!(flags.iter().enumerate().all(|(index, first)| {
            flags
                .iter()
                .skip(index + 1)
                .all(|second| Cli::try_parse_from(["rpx", "add", first, second, "digest"]).is_err())
        }));
        assert!(Cli::try_parse_from(["rpx", "add", "--enhances", "digest"]).is_err());
    }

    #[test]
    fn add_help_lists_supported_dependency_fields() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("add")
            .expect("add command should exist")
            .render_long_help()
            .to_string();

        assert!(
            [
                "--depends",
                "--imports",
                "--linking-to",
                "--suggests",
                "--dev",
            ]
            .iter()
            .all(|flag| help.contains(flag))
        );
        assert!(!help.contains("--enhances"));
    }

    #[test]
    fn parses_base_repository_commands() {
        assert!(matches!(
            parse(&["rpx", "repo", "base", "set", "https://example.test/cran"]),
            Commands::Repo {
                command: RepoCommands::Base {
                    command: RepoBaseCommands::Set(RepoBaseSetArgs { url })
                }
            } if url == "https://example.test/cran"
        ));
        assert!(matches!(
            parse(&["rpx", "repo", "base", "reset", "--remove-credential"]),
            Commands::Repo {
                command: RepoCommands::Base {
                    command: RepoBaseCommands::Reset(RepoBaseResetArgs {
                        remove_credential: true
                    })
                }
            }
        ));
    }

    #[test]
    fn parses_explicit_and_shortcut_additional_commands() {
        for arguments in [
            vec!["rpx", "repo", "add", "https://example.test/cran"],
            vec![
                "rpx",
                "repo",
                "additional",
                "add",
                "https://example.test/cran",
            ],
        ] {
            assert!(matches!(
                parse(&arguments),
                Commands::Repo {
                    command: RepoCommands::Add(RepoAdditionalAddArgs { url })
                        | RepoCommands::Additional {
                            command: RepoAdditionalCommands::Add(RepoAdditionalAddArgs { url })
                        }
                } if url == "https://example.test/cran"
            ));
        }
    }

    #[test]
    fn parses_explicit_and_shortcut_additional_removals() {
        for arguments in [
            vec![
                "rpx",
                "repo",
                "remove",
                "https://example.test/cran",
                "--remove-credential",
            ],
            vec![
                "rpx",
                "repo",
                "additional",
                "remove",
                "https://example.test/cran",
                "--remove-credential",
            ],
        ] {
            assert!(matches!(
                parse(&arguments),
                Commands::Repo {
                    command: RepoCommands::Remove(RepoAdditionalRemoveArgs {
                        url,
                        remove_credential: true
                    }) | RepoCommands::Additional {
                        command: RepoAdditionalCommands::Remove(RepoAdditionalRemoveArgs {
                            url,
                            remove_credential: true
                        })
                    }
                } if url == "https://example.test/cran"
            ));
        }
    }

    #[test]
    fn parses_remote_repository_commands_without_git_specific_types() {
        assert!(matches!(
            parse(&["rpx", "repo", "remote", "add", "github::owner/repository@main"]),
            Commands::Repo {
                command: RepoCommands::Remote {
                    command: RepoRemoteCommands::Add(RepoRemoteArgs { remote })
                }
            } if remote == "github::owner/repository@main"
        ));
    }

    #[test]
    fn parses_repository_list_filter() {
        assert!(matches!(
            parse(&["rpx", "repo", "list", "--type", "remote"]),
            Commands::Repo {
                command: RepoCommands::List(RepoListArgs {
                    repository_type: Some(RepositoryType::Remote)
                })
            }
        ));
    }
}
