use clap::{Parser, Subcommand};

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
        long_about = "Initialize an R project in the current directory by creating a DESCRIPTION file."
    )]
    Init,

    #[command(
        about = "Install one or more packages",
        long_about = "Install one or more packages for this project. Each package is recorded in DESCRIPTION, then rpx regenerates rpx.lock and syncs the project library."
    )]
    Add {
        #[arg(long, help = "Record packages as development dependencies in Suggests")]
        dev: bool,

        #[arg(
            long,
            conflicts_with = "no_default_repo",
            help = "Use the built-in default repository during resolution"
        )]
        default_repo: bool,

        #[arg(
            long,
            conflicts_with = "default_repo",
            help = "Do not use the built-in default repository during resolution"
        )]
        no_default_repo: bool,

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
            long,
            conflicts_with = "no_default_repo",
            help = "Use the built-in default repository during resolution"
        )]
        default_repo: bool,

        #[arg(
            long,
            conflicts_with = "default_repo",
            help = "Do not use the built-in default repository during resolution"
        )]
        no_default_repo: bool,

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
    Lock {
        #[arg(
            long,
            conflicts_with = "no_default_repo",
            help = "Use the built-in default repository during resolution"
        )]
        default_repo: bool,

        #[arg(
            long,
            conflicts_with = "default_repo",
            help = "Do not use the built-in default repository during resolution"
        )]
        no_default_repo: bool,
    },

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

    #[command(about = "Manage additional repositories")]
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
}

#[derive(Subcommand, Debug)]
pub enum RepoCommands {
    #[command(about = "Add an additional repository")]
    Add {
        #[arg(help = "Repository base URL", value_name = "URL", required = true)]
        url: String,
    },

    #[command(about = "Remove an additional repository")]
    Remove {
        #[arg(help = "Repository base URL", value_name = "URL", required = true)]
        url: String,

        #[arg(long, help = "Also remove any stored API key for this repository")]
        remove_credential: bool,
    },

    #[command(about = "List configured repositories")]
    List,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Commands};

    #[test]
    fn parses_dev_add_flag() {
        let cli =
            Cli::try_parse_from(["rpx", "add", "--dev", "testthat"]).expect("dev add should parse");

        let Commands::Add { dev, packages, .. } = cli.command else {
            panic!("expected add command");
        };
        assert!(dev);
        assert_eq!(packages, ["testthat"]);
    }
}
