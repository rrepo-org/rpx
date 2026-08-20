use crate::{
    LockError,
    cli::{InitArgs, InitLicense},
    description::{
        DescriptionWriteError, InitialDescriptionError, InitialDescriptionOptions,
        NamespaceWriteError, initial_description, write_description, write_namespace_if_missing,
    },
    git,
    lockfile::write_lockfile,
    output::status,
    resolve_lockfile_for_description,
};
use miette::Diagnostic;
use std::{
    env, fs, io,
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
    #[error("failed to determine the current working directory: {source}")]
    #[diagnostic(code(rpx::init::working_directory_unavailable))]
    WorkingDirectoryUnavailable {
        #[source]
        source: io::Error,
    },

    #[error("init target is not a directory: {}", path.display())]
    #[diagnostic(code(rpx::init::target_not_directory))]
    TargetNotDirectory { path: PathBuf },

    #[error("failed to read init target {}: {source}", path.display())]
    #[diagnostic(code(rpx::init::target_read_failed))]
    ReadTarget {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("init target is not empty: {}", path.display())]
    #[diagnostic(
        code(rpx::init::target_not_empty),
        help("Choose an empty target directory.")
    )]
    TargetNotEmpty { path: PathBuf },

    #[error("invalid package name `{package_name}`: {reason}")]
    #[diagnostic(code(rpx::init::invalid_package_name))]
    InvalidPackageName {
        package_name: String,
        reason: &'static str,
    },

    #[error("invalid author name `{author_name}`: {reason}")]
    #[diagnostic(code(rpx::init::invalid_author_name))]
    InvalidAuthorName {
        author_name: String,
        reason: &'static str,
    },

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

    #[error("directory name `{directory_name}` does not produce a valid package name")]
    #[diagnostic(
        code(rpx::init::package_name_empty),
        help("Use at least one ASCII letter in the directory name.")
    )]
    EmptyPackageName { directory_name: String },

    #[error("derived package name `{package_name}` must start with a letter")]
    #[diagnostic(
        code(rpx::init::package_name_invalid_start),
        help("Choose a directory whose name starts with an ASCII letter.")
    )]
    DerivedPackageNameMustStartWithLetter { package_name: String },

    #[error("derived package name `{package_name}` must contain at least two characters")]
    #[diagnostic(
        code(rpx::init::package_name_too_short),
        help("Choose a directory name that produces at least two package-name characters.")
    )]
    DerivedPackageNameTooShort { package_name: String },

    #[error("failed to create init target {}: {source}", path.display())]
    #[diagnostic(code(rpx::init::target_creation_failed))]
    CreateTarget {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    InitialDescription(#[from] InitialDescriptionError),

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

    #[error("failed to write license file at {}: {source}", path.display())]
    #[diagnostic(code(rpx::init::license_write_failed))]
    WriteLicense {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("interactive init failed: {source}")]
    #[diagnostic(code(rpx::init::interactive_prompt_failed))]
    InteractivePrompt {
        #[source]
        source: io::Error,
    },

    #[error("failed to generate an available project directory name")]
    #[diagnostic(code(rpx::init::project_name_generation_failed))]
    ProjectNameGeneration,

    #[error(transparent)]
    #[diagnostic(transparent)]
    InitialLock(#[from] LockError),
}

pub(crate) async fn run(args: InitArgs) -> Result<(), Error> {
    let InitArgs {
        path,
        name,
        title,
        description,
        author_name,
        author_email,
        license,
    } = args;
    let current_dir =
        env::current_dir().map_err(|source| Error::WorkingDirectoryUnavailable { source })?;
    let git_identity = git::configured_identity(&current_dir);
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let metadata_form_active = interactive
        && (path.is_none()
            || name.is_none()
            || title.is_none()
            || description.is_none()
            || author_name.is_none()
            || author_email.is_none()
            || license.is_none());
    if metadata_form_active {
        cliclack::intro("Create an R package")
            .map_err(|source| Error::InteractivePrompt { source })?;
    }

    let (target, prompted_directory) = match path {
        Some(path) if path.is_absolute() => (path, None),
        Some(path) => (current_dir.join(path), None),
        None if interactive => {
            let (target, input) = prompt_for_target(&current_dir)?;
            (target, Some(input))
        }
        None => (current_dir, None),
    };

    if target.try_exists().map_err(|source| Error::ReadTarget {
        path: target.clone(),
        source,
    })? {
        if !target.is_dir() {
            return Err(Error::TargetNotDirectory { path: target });
        }
        let mut entries = fs::read_dir(&target).map_err(|source| Error::ReadTarget {
            path: target.clone(),
            source,
        })?;
        if entries
            .next()
            .transpose()
            .map_err(|source| Error::ReadTarget {
                path: target.clone(),
                source,
            })?
            .is_some()
        {
            return Err(Error::TargetNotEmpty { path: target });
        }
    }

    let prompt_for_git = interactive && !git::is_inside_worktree(&target);
    if prompt_for_git && !metadata_form_active {
        cliclack::intro("Create an R package")
            .map_err(|source| Error::InteractivePrompt { source })?;
    }
    let form_active = metadata_form_active || prompt_for_git;

    let package_name = match name {
        Some(package_name) => {
            validate_package_name(&package_name)?;
            package_name
        }
        None if interactive => prompt_for_package_name(&target)?,
        None => derive_package_name(&target)?,
    };
    let title = match title {
        Some(title) => title,
        None if interactive => prompt_for_title(&package_name)?,
        None => title_from_package_name(&package_name),
    };
    let description = match description {
        Some(description) => description,
        None if interactive => prompt_for_description()?,
        None => DEFAULT_DESCRIPTION.to_string(),
    };
    let default_author_name = git_identity
        .name
        .filter(|name| author_name_validation_reason(name).is_none())
        .unwrap_or_else(|| DEFAULT_AUTHOR_NAME.to_string());
    let default_author_email = git_identity
        .email
        .filter(|email| author_email_validation_reason(email).is_none())
        .unwrap_or_else(|| DEFAULT_AUTHOR_EMAIL.to_string());
    let author_name = match author_name {
        Some(author_name) => validate_author_name(author_name)?,
        None if interactive => prompt_for_author_name(&default_author_name)?,
        None => default_author_name,
    };
    let author_email = match author_email {
        Some(author_email) => validate_author_email(author_email)?,
        None if interactive => prompt_for_author_email(&default_author_email)?,
        None => default_author_email,
    };
    let authors_at_r = authors_at_r(&author_name, &author_email);
    let license = match license {
        Some(license) => license,
        None if interactive => prompt_for_license()?,
        None => InitLicense::Mit,
    };
    let initialize_git = prompt_for_git && prompt_for_git_repository()?;
    let description = initial_description(InitialDescriptionOptions {
        package_name: &package_name,
        title: &title,
        description: &description,
        authors_at_r: &authors_at_r,
        license: license.description_value(),
    })?;

    fs::create_dir_all(&target).map_err(|source| Error::CreateTarget {
        path: target.clone(),
        source,
    })?;
    let lockfile = resolve_lockfile_for_description(&target, &description, None).await?;

    write_description(&target, &description)?;
    write_namespace_if_missing(&target)?;
    write_rbuildignore(&target)?;
    write_license_files(&target, license, &author_name)?;
    write_lockfile(&target, &lockfile).map_err(LockError::from)?;
    if initialize_git {
        git::initialize_repository(&target).map_err(|source| Error::InitializeGit {
            path: target.clone(),
            source,
        })?;
        write_gitignore(&target)?;
    }

    if form_active {
        let message = if initialize_git {
            format!(
                "Initialized project and Git repository at {}",
                target.display()
            )
        } else {
            format!("Initialized project at {}", target.display())
        };
        cliclack::outro(message).map_err(|source| Error::InteractivePrompt { source })?;
    } else {
        status(format_args!("Initialized project at {}", target.display()));
    }
    if let Some(directory) = prompted_directory {
        status(format_args!("Next: cd `{directory}`"));
        status("Then: run `rpx sync` or `rpx add <package>`");
    } else {
        status("Next: run `rpx sync` or `rpx add <package>`");
    }
    Ok(())
}

fn prompt_for_target(current_dir: &Path) -> Result<(PathBuf, String), Error> {
    let suggestion = suggested_project_directory(current_dir)?;
    let input: String = cliclack::input("Project directory")
        .default_input(&suggestion)
        .interact()
        .map_err(|source| Error::InteractivePrompt { source })?;
    let path = PathBuf::from(&input);
    let target = if path.is_absolute() {
        path
    } else {
        current_dir.join(path.strip_prefix(".").unwrap_or(&path))
    };
    Ok((target, input))
}

fn prompt_for_package_name(target: &Path) -> Result<String, Error> {
    let default = derive_package_name(target).ok();
    let mut prompt = cliclack::input("Package name")
        .validate(|input: &String| package_name_validation_reason(input).map_or(Ok(()), Err));
    if let Some(default) = &default {
        prompt = prompt.default_input(default);
    }
    let package_name: String = prompt
        .interact()
        .map_err(|source| Error::InteractivePrompt { source })?;
    validate_package_name(&package_name)?;
    Ok(package_name)
}

fn prompt_for_title(package_name: &str) -> Result<String, Error> {
    cliclack::input("Package title")
        .default_input(&title_from_package_name(package_name))
        .interact()
        .map_err(|source| Error::InteractivePrompt { source })
}

fn prompt_for_description() -> Result<String, Error> {
    cliclack::input("Package description")
        .default_input(DEFAULT_DESCRIPTION)
        .interact()
        .map_err(|source| Error::InteractivePrompt { source })
}

fn prompt_for_author_name(default: &str) -> Result<String, Error> {
    let author_name: String = cliclack::input("Author name")
        .default_input(default)
        .validate(|input: &String| author_name_validation_reason(input).map_or(Ok(()), Err))
        .interact()
        .map_err(|source| Error::InteractivePrompt { source })?;
    validate_author_name(author_name)
}

fn prompt_for_author_email(default: &str) -> Result<String, Error> {
    let author_email: String = cliclack::input("Author email")
        .default_input(default)
        .validate(|input: &String| author_email_validation_reason(input).map_or(Ok(()), Err))
        .interact()
        .map_err(|source| Error::InteractivePrompt { source })?;
    validate_author_email(author_email)
}

fn prompt_for_license() -> Result<InitLicense, Error> {
    cliclack::select("License")
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
        .map_err(|source| Error::InteractivePrompt { source })
}

fn prompt_for_git_repository() -> Result<bool, Error> {
    cliclack::confirm("Initialize a Git repository?")
        .initial_value(true)
        .interact()
        .map_err(|source| Error::InteractivePrompt { source })
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
                &format!("YEAR: {year}\n\nCOPYRIGHT HOLDER: {copyright_holder}\n"),
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
        return Err(Error::EmptyPackageName {
            directory_name: directory_name.to_string(),
        });
    };
    if !first.is_ascii_alphabetic() {
        return Err(Error::DerivedPackageNameMustStartWithLetter { package_name });
    }
    if package_name.len() < 2 {
        return Err(Error::DerivedPackageNameTooShort { package_name });
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

fn author_name_validation_reason(author_name: &str) -> Option<&'static str> {
    if author_name.trim().is_empty() {
        Some("must not be empty")
    } else if author_name.chars().any(char::is_control) {
        Some("must be a single line without control characters")
    } else {
        None
    }
}

fn validate_author_name(author_name: String) -> Result<String, Error> {
    let author_name = author_name.trim().to_string();
    match author_name_validation_reason(&author_name) {
        Some(reason) => Err(Error::InvalidAuthorName {
            author_name,
            reason,
        }),
        None => Ok(author_name),
    }
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

fn package_name_validation_reason(package_name: &str) -> Option<&'static str> {
    if package_name.len() < 2 {
        Some("must contain at least two characters")
    } else if !package_name.starts_with(|character: char| character.is_ascii_alphabetic()) {
        Some("must start with an ASCII letter")
    } else if package_name.ends_with('.') {
        Some("must not end with a dot")
    } else if !package_name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '.')
    {
        Some("may contain only ASCII letters, digits, and dots")
    } else {
        None
    }
}

fn validate_package_name(package_name: &str) -> Result<(), Error> {
    match package_name_validation_reason(package_name) {
        Some(reason) => Err(Error::InvalidPackageName {
            package_name: package_name.to_string(),
            reason,
        }),
        None => Ok(()),
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
            Err(Error::DerivedPackageNameMustStartWithLetter { .. })
        ));
        assert!(matches!(
            derive_package_name(Path::new("/tmp/---")),
            Err(Error::EmptyPackageName { .. })
        ));
        assert!(matches!(
            derive_package_name(Path::new("/tmp/x")),
            Err(Error::DerivedPackageNameTooShort { .. })
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
                    Err(Error::InvalidPackageName { .. })
                ),
                "package name should be invalid: {package_name}"
            );
        }
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
