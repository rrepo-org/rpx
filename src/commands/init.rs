use crate::{
    LockError, SyncError,
    cli::{InitArgs, InitLicense},
    description::{
        DependencyField, DescriptionParseError, DescriptionWriteError, NamespaceWriteError,
        add_dependencies, project_dependencies, write_description, write_namespace_if_missing,
    },
    git,
    lockfile::write_lockfile,
    output::status,
    pin_dependency_to_resolved_major, resolve_lockfile_for_description, sync_project,
};
use miette::Diagnostic;
use r_description::{RDescription, Relation};
use std::{
    collections::BTreeSet,
    env, fmt, fs, io,
    io::IsTerminal,
    path::{Path, PathBuf},
};
use thiserror::Error;

const RBUILDIGNORE: &str = include_str!("../../assets/Rbuildignore");
const GITIGNORE: &str = include_str!("../../assets/R.gitignore");
const DEFAULT_DESCRIPTION: &str = "Describe what this package does.";
const DEFAULT_AUTHOR_NAME: &str = "Package Author";
const DEFAULT_AUTHOR_EMAIL: &str = "author@example.com";

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error("failed to determine the current working directory: {0}")]
    #[diagnostic(
        code(rpx::init::working_directory_unavailable),
        help("Change to an existing, accessible directory and rerun `rpx init`.")
    )]
    WorkingDirectoryUnavailable(#[source] io::Error),

    #[error("init target is not a directory: {}", path.display())]
    #[diagnostic(
        code(rpx::init::target_not_directory),
        help("Choose a missing path or an empty directory.")
    )]
    TargetNotDirectory { path: PathBuf },

    #[error("failed to read init target {}: {source}", path.display())]
    #[diagnostic(
        code(rpx::init::target_read_failed),
        help("Check read permissions for the target and its parent directories, then rerun.")
    )]
    ReadTarget {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("init target is not empty: {}", path.display())]
    #[diagnostic(
        code(rpx::init::target_not_empty),
        help("Choose a missing or empty directory, or move the existing contents first.")
    )]
    TargetNotEmpty { path: PathBuf },

    #[error(transparent)]
    #[diagnostic(transparent)]
    InvalidPackageName(#[from] PackageNameValidationError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InvalidAuthorName(#[from] AuthorNameValidationError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    InvalidText(#[from] TextValidationError),

    #[error("invalid author email `{author_email}`: {reason}")]
    #[diagnostic(code(rpx::init::invalid_author_email))]
    InvalidAuthorEmail {
        author_email: String,
        reason: &'static str,
    },

    #[error("failed to derive a package name from {}", path.display())]
    #[diagnostic(code(rpx::init::package_name_missing))]
    MissingDirectoryName { path: PathBuf },

    #[error("project directory name is not valid UTF-8: {}", path.display())]
    #[diagnostic(code(rpx::init::package_name_invalid_utf8))]
    InvalidDirectoryName { path: PathBuf },

    #[error("failed to create init target {}: {source}", path.display())]
    #[diagnostic(code(rpx::init::target_creation_failed))]
    CreateTarget {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to initialize DESCRIPTION")]
    #[diagnostic(code(rpx::description::initializing_description))]
    InitialDescription(#[from] r_description::FieldMutationError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    WriteDescription(#[from] DescriptionWriteError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    WriteNamespace(#[from] NamespaceWriteError),

    #[error("failed to write .Rbuildignore at {}: {source}", path.display())]
    #[diagnostic(code(rpx::init::rbuildignore_write_failed))]
    WriteRbuildignore {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to write .gitignore at {}: {source}", path.display())]
    #[diagnostic(code(rpx::init::gitignore_write_failed))]
    WriteGitignore {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to initialize Git repository at {}: {source}", path.display())]
    #[diagnostic(code(rpx::init::git_initialization_failed))]
    InitializeGit {
        path: PathBuf,
        #[source]
        source: git::GitError,
    },

    #[error("failed to inspect Git repository for {}: {source}", path.display())]
    #[diagnostic(
        code(rpx::init::git_inspection_failed),
        help("Check access to the target and its parent directories, then rerun `rpx init`.")
    )]
    InspectGit {
        path: PathBuf,
        #[source]
        source: git::GitError,
    },

    #[error("failed to write license file at {}: {source}", path.display())]
    #[diagnostic(code(rpx::init::license_write_failed))]
    WriteLicense {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("interactive init failed: {0}")]
    #[diagnostic(
        code(rpx::init::interactive_prompt_failed),
        help("Rerun in an interactive terminal, or provide init options non-interactively.")
    )]
    InteractivePrompt(#[source] io::Error),

    #[error("failed to generate an available project directory name")]
    #[diagnostic(code(rpx::init::project_name_generation_failed))]
    ProjectNameGeneration,

    #[error(transparent)]
    #[diagnostic(transparent)]
    InitialLock(#[from] LockError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Sync(#[from] SyncError),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DevelopmentPackage {
    Testthat,
    Roxygen2,
    Devtools,
}

#[derive(Clone, Copy)]
struct InitialDescriptionOptions<'a> {
    package_name: &'a str,
    title: &'a str,
    description: &'a str,
    authors_at_r: &'a str,
    author: &'a str,
    maintainer: &'a str,
    license: &'a str,
}

#[derive(Debug, Error, Diagnostic)]
#[error("invalid package name `{package_name}`")]
#[diagnostic(
    code(rpx::init::invalid_package_name),
    help("Edit the package name, or pass `--name` when it is inferred from a directory.")
)]
pub(crate) struct PackageNameValidationError {
    package_name: String,

    #[related]
    issues: Vec<PackageNameIssue>,
}

impl PackageNameValidationError {
    fn inline_message(&self) -> String {
        self.issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[derive(Debug, Error, Diagnostic)]
enum PackageNameIssue {
    #[error("must contain at least two characters")]
    #[diagnostic(help("Enter at least two characters."))]
    TooShort,

    #[error("must start with an ASCII letter")]
    #[diagnostic(help("Start the name with a letter from A to Z."))]
    MustStartWithLetter,

    #[error("must not end with a dot")]
    #[diagnostic(help("Remove the final dot."))]
    EndsWithDot,

    #[error("may contain only ASCII letters, digits, and dots")]
    #[diagnostic(help("Remove spaces, punctuation other than dots, and non-ASCII characters."))]
    InvalidCharacters,
}

#[derive(Debug, Error, Diagnostic)]
#[error("invalid author name `{author_name}`")]
#[diagnostic(
    code(rpx::init::invalid_author_name),
    help("Enter a non-empty name on a single line.")
)]
pub(crate) struct AuthorNameValidationError {
    author_name: String,

    #[related]
    issues: Vec<AuthorNameIssue>,
}

impl AuthorNameValidationError {
    fn inline_message(&self) -> String {
        self.issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[derive(Debug, Error, Diagnostic)]
enum AuthorNameIssue {
    #[error("must not be empty")]
    #[diagnostic(help("Enter the author's name."))]
    Empty,

    #[error("must be a single line without control characters")]
    #[diagnostic(help("Remove line breaks and other control characters."))]
    ControlCharacters,
}

#[derive(Clone, Copy, Debug)]
enum TextField {
    Title,
    Description,
}

impl fmt::Display for TextField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Title => "package title",
            Self::Description => "package description",
        })
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("invalid {field}")]
#[diagnostic(
    code(rpx::init::invalid_text),
    help("Enter non-empty text on a single line.")
)]
pub(crate) struct TextValidationError {
    field: TextField,

    #[related]
    issues: Vec<TextIssue>,
}

impl TextValidationError {
    fn inline_message(&self) -> String {
        self.issues
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[derive(Debug, Error, Diagnostic)]
enum TextIssue {
    #[error("must not be empty")]
    #[diagnostic(help("Enter a value."))]
    Empty,

    #[error("must be a single line without control characters")]
    #[diagnostic(help("Remove line breaks and other control characters."))]
    ControlCharacters,
}

impl DevelopmentPackage {
    fn name(self) -> &'static str {
        match self {
            Self::Testthat => "testthat",
            Self::Roxygen2 => "roxygen2",
            Self::Devtools => "devtools",
        }
    }
}

pub(crate) async fn run(args: InitArgs) -> Result<(), Error> {
    let current_dir = env::current_dir().map_err(Error::WorkingDirectoryUnavailable)?;
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();

    if interactive {
        cliclack::intro("Create an R package").map_err(Error::InteractivePrompt)?;
    }

    let target = match args.path {
        Some(path) => {
            let target = resolve_target(&current_dir, &path);
            validate_target(&target)?;
            target
        }
        None if interactive => prompt_for_target(&current_dir)?,
        None => {
            validate_target(&current_dir)?;
            current_dir.clone()
        }
    };

    let package_name = match args.name {
        Some(package_name) => {
            validate_package_name(&package_name)?;
            package_name
        }
        None if interactive => prompt_for_package_name(&target)?,
        None => derive_package_name(&target)?,
    };
    let title = match args.title {
        Some(title) => validated_text(title, TextField::Title)?,
        None if interactive => prompt_for_title(&package_name)?,
        None => title_from_package_name(&package_name),
    };
    let description = match args.description {
        Some(description) => validated_text(description, TextField::Description)?,
        None if interactive => prompt_for_description()?,
        None => DEFAULT_DESCRIPTION.to_string(),
    };
    let (author_name, author_email) = if interactive {
        prompt_for_authors(&target, args.author_name, args.author_email)?
    } else {
        (
            args.author_name
                .map(validated_author_name)
                .transpose()?
                .unwrap_or_else(|| DEFAULT_AUTHOR_NAME.to_string()),
            args.author_email
                .map(validate_author_email)
                .transpose()?
                .unwrap_or_else(|| DEFAULT_AUTHOR_EMAIL.to_string()),
        )
    };
    let authors_at_r = authors_at_r(&author_name, &author_email);
    let author = format!("{author_name} [aut, cre]");
    let maintainer = format!("{author_name} <{author_email}>");
    let license = match args.license {
        Some(license) => license,
        None if interactive => prompt_for_license()?,
        None => InitLicense::Mit,
    };
    let development_packages = if interactive {
        prompt_for_development_packages()?
    } else {
        Vec::new()
    };
    let initialize_git = if interactive {
        prompt_for_git_repository(&target)?
    } else {
        false
    };
    let mut description = initial_description(InitialDescriptionOptions {
        package_name: &package_name,
        title: &title,
        description: &description,
        authors_at_r: &authors_at_r,
        author: &author,
        maintainer: &maintainer,
        license: license.description_value(),
    })?;
    let development_relations = development_relations(&development_packages);
    add_dependencies(
        &target,
        &mut description,
        &development_relations,
        DependencyField::Suggests,
    )?;

    fs::create_dir_all(&target).map_err(|source| Error::CreateTarget {
        path: target.clone(),
        source,
    })?;
    let mut lockfile = resolve_lockfile_for_description(&target, &description, None).await?;
    let mut pinned_development_relations = development_relations.clone();
    for package in development_packages {
        let version = &lockfile
            .packages
            .get(package.name())
            .expect("resolved lockfile should contain selected development packages")
            .version;
        pin_dependency_to_resolved_major(
            &mut pinned_development_relations,
            package.name(),
            version,
        );
    }
    if pinned_development_relations != development_relations {
        add_dependencies(
            &target,
            &mut description,
            &pinned_development_relations,
            DependencyField::Suggests,
        )?;
        lockfile.requirements = project_dependencies(&target, &description)?;
    }

    write_description(&target, &description)?;
    write_namespace_if_missing(&target)?;
    write_rbuildignore(&target)?;
    write_license_files(&target, license, &author_name)?;
    write_lockfile(&target, &lockfile).map_err(LockError::from)?;
    let r_version = lockfile.r.clone();
    sync_project(&target, description, &lockfile, &r_version, false, false).await?;
    if initialize_git {
        git::initialize_repository(&target).map_err(|source| Error::InitializeGit {
            path: target.clone(),
            source,
        })?;
        write_gitignore(&target)?;
    }

    if interactive {
        let message = if initialize_git {
            format!(
                "Initialized project and Git repository at {}",
                target.display()
            )
        } else {
            format!("Initialized project at {}", target.display())
        };
        cliclack::outro(message).map_err(Error::InteractivePrompt)?;
    } else {
        status(format_args!("Initialized project at {}", target.display()));
    }
    if target != current_dir {
        status(format_args!("Next: cd `{}`", target.display()));
        status("Then: run `rpx add <package>`");
    } else {
        status("Next: run `rpx add <package>`");
    }
    Ok(())
}

fn prompt_for_target(current_dir: &Path) -> Result<PathBuf, Error> {
    let suggestion = suggested_project_directory(current_dir)?;
    let validation_dir = current_dir.to_path_buf();
    let input: String = cliclack::input("Project directory")
        .default_input(&suggestion)
        .validate(move |input: &String| {
            let target = resolve_target(&validation_dir, Path::new(input));
            validate_target(&target).map_err(|error| error.to_string())
        })
        .interact()
        .map_err(Error::InteractivePrompt)?;

    Ok(resolve_target(current_dir, Path::new(&input)))
}

fn resolve_target(current_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    }
}

fn validate_target(target: &Path) -> Result<(), Error> {
    match fs::read_dir(target) {
        Ok(mut entries) => {
            if entries
                .next()
                .transpose()
                .map_err(|source| Error::ReadTarget {
                    path: target.to_path_buf(),
                    source,
                })?
                .is_some()
            {
                return Err(Error::TargetNotEmpty {
                    path: target.to_path_buf(),
                });
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) if source.kind() == io::ErrorKind::NotADirectory => {
            return Err(Error::TargetNotDirectory {
                path: target.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(Error::ReadTarget {
                path: target.to_path_buf(),
                source,
            });
        }
    }

    Ok(())
}

fn prompt_for_package_name(target: &Path) -> Result<String, Error> {
    let default = derive_package_name(target).ok();
    let mut prompt = cliclack::input("Package name").validate(|input: &String| {
        validate_package_name(input).map_err(|error| error.inline_message())
    });
    if let Some(default) = &default {
        prompt = prompt.default_input(default);
    }
    let package_name: String = prompt.interact().map_err(Error::InteractivePrompt)?;
    Ok(package_name)
}

fn prompt_for_title(package_name: &str) -> Result<String, Error> {
    let title: String = cliclack::input("Package title")
        .default_input(&title_from_package_name(package_name))
        .validate(|input: &String| {
            validate_text(input, TextField::Title).map_err(|error| error.inline_message())
        })
        .interact()
        .map_err(Error::InteractivePrompt)?;
    Ok(title.trim().to_string())
}

fn prompt_for_description() -> Result<String, Error> {
    let description: String = cliclack::input("Package description")
        .default_input(DEFAULT_DESCRIPTION)
        .validate(|input: &String| {
            validate_text(input, TextField::Description).map_err(|error| error.inline_message())
        })
        .interact()
        .map_err(Error::InteractivePrompt)?;
    Ok(description.trim().to_string())
}

fn prompt_for_authors(
    target: &Path,
    author_name: Option<String>,
    author_email: Option<String>,
) -> Result<(String, String), Error> {
    let identity = if author_name.is_none() || author_email.is_none() {
        git::configured_identity(target).unwrap_or_default()
    } else {
        git::Identity::default()
    };
    let default_author_name = identity
        .name
        .filter(|name| validate_author_name(name).is_ok())
        .unwrap_or_else(|| DEFAULT_AUTHOR_NAME.to_string());
    let default_author_email = identity
        .email
        .filter(|email| author_email_validation_reason(email).is_none())
        .unwrap_or_else(|| DEFAULT_AUTHOR_EMAIL.to_string());

    let author_name = match author_name {
        Some(author_name) => validated_author_name(author_name)?,
        None => prompt_for_author_name(&default_author_name)?,
    };
    let author_email = match author_email {
        Some(author_email) => validate_author_email(author_email)?,
        None => prompt_for_author_email(&default_author_email)?,
    };

    Ok((author_name, author_email))
}

fn prompt_for_author_name(default: &str) -> Result<String, Error> {
    let author_name: String = cliclack::input("Author name")
        .default_input(default)
        .validate(|input: &String| {
            validate_author_name(input).map_err(|error| error.inline_message())
        })
        .interact()
        .map_err(Error::InteractivePrompt)?;
    Ok(author_name.trim().to_string())
}

fn prompt_for_author_email(default: &str) -> Result<String, Error> {
    let author_email: String = cliclack::input("Author email")
        .default_input(default)
        .validate(|input: &String| author_email_validation_reason(input).map_or(Ok(()), Err))
        .interact()
        .map_err(Error::InteractivePrompt)?;
    validate_author_email(author_email)
}

fn prompt_for_license() -> Result<InitLicense, Error> {
    Ok(cliclack::select("License")
        .item(InitLicense::Mit, "MIT", "Simple and permissive")
        .item(
            InitLicense::Apache2,
            "Apache 2.0",
            "Permissive with patent protection",
        )
        .item(InitLicense::Gpl2, "GPL 2", "Strong copyleft")
        .item(InitLicense::Gpl3, "GPL 3", "Strong copyleft")
        .item(InitLicense::Agpl3, "AGPL 3", "Network copyleft")
        .item(InitLicense::Lgpl21, "LGPL 2.1", "Library copyleft")
        .item(InitLicense::Lgpl3, "LGPL 3", "Library copyleft")
        .item(InitLicense::Cc0, "CC0", "Public-domain dedication")
        .item(
            InitLicense::CcBy4,
            "CC BY 4.0",
            "Attribution license for data",
        )
        .item(
            InitLicense::Proprietary,
            "Proprietary",
            "All rights reserved",
        )
        .initial_value(InitLicense::Mit)
        .max_rows(8)
        .interact()
        .map_err(Error::InteractivePrompt)?)
}

fn prompt_for_development_packages() -> Result<Vec<DevelopmentPackage>, Error> {
    Ok(cliclack::multiselect("Development packages")
        .item(DevelopmentPackage::Testthat, "testthat", "Unit testing")
        .item(
            DevelopmentPackage::Roxygen2,
            "roxygen2",
            "Documentation generation",
        )
        .item(
            DevelopmentPackage::Devtools,
            "devtools",
            "Package development toolkit",
        )
        .required(false)
        .interact()
        .map_err(Error::InteractivePrompt)?)
}

fn development_relations(packages: &[DevelopmentPackage]) -> BTreeSet<Relation> {
    packages
        .iter()
        .map(|package| {
            Relation::any(package.name()).expect("built-in package names should be valid")
        })
        .collect()
}

fn initial_description(
    options: InitialDescriptionOptions<'_>,
) -> Result<RDescription, r_description::FieldMutationError> {
    let mut description = RDescription::parse("");
    description.set_package(options.package_name)?;
    let version = "0.1.0".parse().expect("0.1.0 should parse");
    description.set_version(&version);
    description.set_title(options.title)?;
    description.set_description(options.description)?;
    description.set_license(options.license)?;
    description.set_authors_at_r(options.authors_at_r)?;
    description.set_author(options.author)?;
    description.set_maintainer(options.maintainer)?;
    Ok(description)
}

fn prompt_for_git_repository(target: &Path) -> Result<bool, Error> {
    if git::is_inside_worktree(target).map_err(|source| Error::InspectGit {
        path: target.to_path_buf(),
        source,
    })? {
        return Ok(false);
    }

    Ok(cliclack::confirm("Initialize a Git repository?")
        .initial_value(true)
        .interact()
        .map_err(Error::InteractivePrompt)?)
}

fn suggested_project_directory(current_dir: &Path) -> Result<String, Error> {
    for _ in 0..64 {
        let Some(name) = petname::petname(2, "-") else {
            continue;
        };
        let mut words = name.split('-');
        let valid = matches!((words.next(), words.next(), words.next()), (Some(first), Some(second), None)
            if !first.is_empty()
                && !second.is_empty()
                && first.chars().all(|character| character.is_ascii_lowercase())
                && second.chars().all(|character| character.is_ascii_lowercase()));
        if !valid {
            continue;
        }

        let candidate = current_dir.join(&name);
        if !candidate.try_exists().map_err(|source| Error::ReadTarget {
            path: candidate,
            source,
        })? {
            return Ok(format!("./{name}"));
        }
    }

    Err(Error::ProjectNameGeneration)
}

fn write_rbuildignore(target: &Path) -> Result<(), Error> {
    let path = target.join(".Rbuildignore");
    fs::write(&path, RBUILDIGNORE).map_err(|source| Error::WriteRbuildignore { path, source })
}

fn write_gitignore(target: &Path) -> Result<(), Error> {
    let path = target.join(".gitignore");
    fs::write(&path, GITIGNORE).map_err(|source| Error::WriteGitignore { path, source })
}

impl InitLicense {
    fn description_value(self) -> &'static str {
        match self {
            Self::Mit => "MIT + file LICENSE",
            Self::Apache2 => "Apache License (== 2.0)",
            Self::Gpl2 => "GPL-2",
            Self::Gpl3 => "GPL-3",
            Self::Agpl3 => "AGPL-3",
            Self::Lgpl21 => "LGPL-2.1",
            Self::Lgpl3 => "LGPL-3",
            Self::Cc0 => "CC0",
            Self::CcBy4 => "CC BY 4.0",
            Self::Proprietary => "file LICENSE",
        }
    }

    fn full_text(self) -> Option<&'static str> {
        match self {
            Self::Mit => Some(include_str!("../../assets/licenses/MIT.md")),
            Self::Apache2 => Some(include_str!("../../assets/licenses/Apache-2.0.md")),
            Self::Gpl2 => Some(include_str!("../../assets/licenses/GPL-2.md")),
            Self::Gpl3 => Some(include_str!("../../assets/licenses/GPL-3.md")),
            Self::Agpl3 => Some(include_str!("../../assets/licenses/AGPL-3.md")),
            Self::Lgpl21 => Some(include_str!("../../assets/licenses/LGPL-2.1.md")),
            Self::Lgpl3 => Some(include_str!("../../assets/licenses/LGPL-3.md")),
            Self::Cc0 => Some(include_str!("../../assets/licenses/CC0.md")),
            Self::CcBy4 => Some(include_str!("../../assets/licenses/CC-BY-4.0.md")),
            Self::Proprietary => None,
        }
    }
}

fn write_license_files(
    target: &Path,
    license: InitLicense,
    copyright_holder: &str,
) -> Result<(), Error> {
    let year = time::OffsetDateTime::now_utc().year();
    match license {
        InitLicense::Mit => {
            write_license_file(
                target.join("LICENSE"),
                &format!("YEAR: {year}\nCOPYRIGHT HOLDER: {copyright_holder}\n"),
            )?;
            let full_text = license
                .full_text()
                .expect("MIT should have a full license template")
                .replace("{{{year}}}", &year.to_string())
                .replace("{{{copyright_holder}}}", copyright_holder);
            write_license_file(target.join("LICENSE.md"), &full_text)
        }
        InitLicense::Proprietary => write_license_file(
            target.join("LICENSE"),
            &format!("Copyright {year} {copyright_holder}. All rights reserved.\n"),
        ),
        _ => write_license_file(
            target.join("LICENSE.md"),
            license
                .full_text()
                .expect("open-source licenses should have a full license text"),
        ),
    }
}

fn write_license_file(path: PathBuf, contents: &str) -> Result<(), Error> {
    fs::write(&path, contents).map_err(|source| Error::WriteLicense { path, source })
}

fn derive_package_name(path: &Path) -> Result<String, Error> {
    let directory_name = path
        .file_name()
        .ok_or_else(|| Error::MissingDirectoryName {
            path: path.to_path_buf(),
        })?
        .to_str()
        .ok_or_else(|| Error::InvalidDirectoryName {
            path: path.to_path_buf(),
        })?;

    let package_name = directory_name
        .chars()
        .filter_map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' => Some(character),
            '-' | '_' | ' ' | '.' => Some('.'),
            _ => None,
        })
        .fold(String::new(), |mut package_name, character| {
            if character != '.' || !package_name.ends_with('.') {
                package_name.push(character);
            }
            package_name
        });
    let package_name = package_name.trim_matches('.').to_string();
    validate_package_name(&package_name)?;
    Ok(package_name)
}

fn title_from_package_name(package_name: &str) -> String {
    package_name
        .split('.')
        .filter_map(|part| {
            let mut characters = part.chars();
            characters
                .next()
                .map(|first| format!("{}{}", first.to_ascii_uppercase(), characters.as_str()))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_author_name(author_name: &str) -> Result<(), AuthorNameValidationError> {
    let issues = [
        author_name
            .trim()
            .is_empty()
            .then_some(AuthorNameIssue::Empty),
        author_name
            .chars()
            .any(char::is_control)
            .then_some(AuthorNameIssue::ControlCharacters),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    if issues.is_empty() {
        Ok(())
    } else {
        Err(AuthorNameValidationError {
            author_name: author_name.to_string(),
            issues,
        })
    }
}

fn validated_author_name(author_name: String) -> Result<String, Error> {
    validate_author_name(&author_name)?;
    Ok(author_name.trim().to_string())
}

fn validate_text(value: &str, field: TextField) -> Result<(), TextValidationError> {
    let issues = [
        value.trim().is_empty().then_some(TextIssue::Empty),
        value
            .chars()
            .any(char::is_control)
            .then_some(TextIssue::ControlCharacters),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    if issues.is_empty() {
        Ok(())
    } else {
        Err(TextValidationError { field, issues })
    }
}

fn validated_text(value: String, field: TextField) -> Result<String, Error> {
    validate_text(&value, field)?;
    Ok(value.trim().to_string())
}

fn author_email_validation_reason(author_email: &str) -> Option<&'static str> {
    let author_email = author_email.trim();
    let Some((local, domain)) = author_email.split_once('@') else {
        return Some("must contain an @ separating local and domain parts");
    };
    if local.is_empty() || domain.is_empty() {
        Some("must contain non-empty local and domain parts")
    } else if domain.contains('@') {
        Some("must contain exactly one @")
    } else if !author_email.is_ascii()
        || author_email
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        Some("must be an ASCII address without whitespace or control characters")
    } else {
        None
    }
}

fn validate_author_email(author_email: String) -> Result<String, Error> {
    let author_email = author_email.trim().to_string();
    match author_email_validation_reason(&author_email) {
        Some(reason) => Err(Error::InvalidAuthorEmail {
            author_email,
            reason,
        }),
        None => Ok(author_email),
    }
}

fn authors_at_r(author_name: &str, author_email: &str) -> String {
    format!(
        "person(given = {}, email = {}, role = c(\"aut\", \"cre\"))",
        r_string(author_name),
        r_string(author_email)
    )
}

fn r_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn validate_package_name(package_name: &str) -> Result<(), PackageNameValidationError> {
    let issues = [
        (package_name.chars().count() < 2).then_some(PackageNameIssue::TooShort),
        (!package_name.starts_with(|character: char| character.is_ascii_alphabetic()))
            .then_some(PackageNameIssue::MustStartWithLetter),
        package_name
            .ends_with('.')
            .then_some(PackageNameIssue::EndsWithDot),
        (!package_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '.'))
        .then_some(PackageNameIssue::InvalidCharacters),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    if issues.is_empty() {
        Ok(())
    } else {
        Err(PackageNameValidationError {
            package_name: package_name.to_string(),
            issues,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_title_from_package_name() {
        assert_eq!(
            title_from_package_name("my.package.name"),
            "My Package Name"
        );
    }

    #[test]
    fn derives_and_validates_package_names_from_directory_names() {
        assert_eq!(
            derive_package_name(Path::new("/tmp/my-project_name")).unwrap(),
            "my.project.name"
        );
        assert!(matches!(
            derive_package_name(Path::new("/tmp/123-project")),
            Err(Error::InvalidPackageName(_))
        ));
        assert!(matches!(
            derive_package_name(Path::new("/tmp/---")),
            Err(Error::InvalidPackageName(_))
        ));
        assert!(matches!(
            derive_package_name(Path::new("/tmp/x")),
            Err(Error::InvalidPackageName(_))
        ));
    }

    #[test]
    fn validates_explicit_package_names() {
        for package_name in ["ab", "example", "example.pkg", "a1"] {
            validate_package_name(package_name).expect("package name should be valid");
        }

        for package_name in ["", "x", "1example", "example-", "example.", "éxample"] {
            assert!(
                matches!(
                    validate_package_name(package_name),
                    Err(PackageNameValidationError { .. })
                ),
                "package name should be invalid: {package_name}"
            );
        }
    }

    #[test]
    fn validates_author_names_with_related_issues() {
        validate_author_name("Package Author").expect("author name should be valid");

        let error = validate_author_name("\n").expect_err("author name should be invalid");
        assert_eq!(error.issues.len(), 2);
        assert_eq!(
            error.inline_message(),
            "must not be empty; must be a single line without control characters"
        );
    }

    #[test]
    fn validates_required_description_text_with_related_issues() {
        validate_text("A useful package.", TextField::Description)
            .expect("description should be valid");

        let error = validate_text("\n", TextField::Title).expect_err("title should be invalid");
        assert_eq!(error.issues.len(), 2);
        assert_eq!(
            error.inline_message(),
            "must not be empty; must be a single line without control characters"
        );
    }

    #[test]
    fn maps_selected_development_packages_to_unconstrained_relations() {
        let relations = development_relations(&[
            DevelopmentPackage::Testthat,
            DevelopmentPackage::Roxygen2,
            DevelopmentPackage::Devtools,
        ]);

        assert_eq!(
            relations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["devtools", "roxygen2", "testthat"]
        );
    }

    #[test]
    fn creates_initial_description() {
        let description = initial_description(InitialDescriptionOptions {
            package_name: "my.package",
            title: "My Package",
            description: "Describe what this package does.",
            authors_at_r: r#"person(given = "Package Author", email = "author@example.com", role = c("aut", "cre"))"#,
            author: "Package Author [aut, cre]",
            maintainer: "Package Author <author@example.com>",
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
        assert!(rendered.contains("Author: Package Author [aut, cre]"));
        assert!(rendered.contains("Maintainer: Package Author <author@example.com>"));
    }

    #[test]
    fn production_rbuildignore_has_exact_contents() {
        assert_eq!(
            RBUILDIGNORE,
            "^rpx\\.lock$\n\
^.*\\.Rproj$\n\
^\\.Rproj\\.user$\n\
^README\\.Rmd$\n\
^LICENSE\\.md$\n\
^cran-comments\\.md$\n\
^CRAN-SUBMISSION$\n\
^CRAN-RELEASE$\n\
^revdep$\n\
^data-raw$\n\
^pkgdown$\n\
^_pkgdown\\.yml$\n\
^docs$\n\
^\\.github$\n\
^\\.vscode$\n\
^[.]?air[.]toml$\n"
        );
    }
}
