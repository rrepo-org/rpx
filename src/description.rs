use miette::Diagnostic;
use r_description::{FieldMutationError, RDescription, VersionParseError};
use std::{
    fs,
    path::{Path, PathBuf},
};
use thiserror::Error;

use crate::project::{ProjectPathError, new_project_description_path};

#[derive(Debug, Error, Diagnostic)]
pub enum DescriptionError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectPath(#[from] ProjectPathError),

    #[error("DESCRIPTION already exists at {}", path.display())]
    #[diagnostic(
        code(rpx::description::already_exists),
        help(
            "Run rpx commands from this project, or remove DESCRIPTION before initializing a new project."
        )
    )]
    AlreadyExists { path: PathBuf },

    #[error("failed to derive package name for DESCRIPTION: {details}")]
    #[diagnostic(code(rpx::description::package_name_failed))]
    PackageNameFailed { details: String },

    #[error("failed to build DESCRIPTION: {source}")]
    #[diagnostic(code(rpx::description::invalid_field))]
    FieldMutation {
        #[source]
        source: FieldMutationError,
    },

    #[error("failed to build DESCRIPTION: invalid version: {source}")]
    #[diagnostic(code(rpx::description::invalid_version))]
    Version {
        #[source]
        source: VersionParseError,
    },

    #[error("failed to write DESCRIPTION at {}: {source}", path.display())]
    #[diagnostic(code(rpx::description::write_failed))]
    WriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn init_description() -> Result<String, DescriptionError> {
    let path = new_project_description_path()?;
    if path.exists() {
        return Err(DescriptionError::AlreadyExists { path });
    }

    let package_name = package_name_from_description_path(&path)?;
    let description = initial_description(&package_name)?;

    fs::write(&path, description.to_string()).map_err(|source| DescriptionError::WriteFailed {
        path: path.clone(),
        source,
    })?;
    let namespace_path = path.with_file_name("NAMESPACE");
    if !namespace_path.exists() {
        fs::write(&namespace_path, "").map_err(|source| DescriptionError::WriteFailed {
            path: namespace_path,
            source,
        })?;
    }

    Ok(path.display().to_string())
}

fn initial_description(package_name: &str) -> Result<RDescription, DescriptionError> {
    let mut description = RDescription::parse("");
    description
        .set_package(package_name)
        .map_err(|source| DescriptionError::FieldMutation { source })?;
    let version = "0.1.0"
        .parse()
        .map_err(|source| DescriptionError::Version { source })?;
    description.set_version(&version);
    description
        .set_title(&title_from_package_name(package_name))
        .map_err(|source| DescriptionError::FieldMutation { source })?;
    description
        .set_description("Add a package description.")
        .map_err(|source| DescriptionError::FieldMutation { source })?;
    description
        .set_license("MIT")
        .map_err(|source| DescriptionError::FieldMutation { source })?;
    description
        .set_authors_at_r(
            r#"person("First", "Last", email = "you@example.com", role = c("aut", "cre"))"#,
        )
        .map_err(|source| DescriptionError::FieldMutation { source })?;
    description
        .set_maintainer("Your Name <you@example.com>")
        .map_err(|source| DescriptionError::FieldMutation { source })?;
    Ok(description)
}

fn package_name_from_description_path(path: &Path) -> Result<String, DescriptionError> {
    let directory_name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .ok_or_else(|| DescriptionError::PackageNameFailed {
            details: "failed to derive package name from DESCRIPTION path".to_string(),
        })?;

    sanitize_package_name(directory_name)
}

fn sanitize_package_name(directory_name: &str) -> Result<String, DescriptionError> {
    let mut package_name = String::new();

    for character in directory_name.chars() {
        match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' => package_name.push(character),
            '-' | '_' | ' ' | '.' => {
                if !package_name.ends_with('.') {
                    package_name.push('.');
                }
            }
            _ => {}
        }
    }

    let package_name = package_name.trim_matches('.').to_string();
    let Some(first) = package_name.chars().next() else {
        return Err(DescriptionError::PackageNameFailed {
            details: "current directory does not produce a valid package name".to_string(),
        });
    };

    if !first.is_ascii_alphabetic() {
        return Err(DescriptionError::PackageNameFailed {
            details: "package name must start with a letter".to_string(),
        });
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

#[cfg(test)]
mod tests {
    use super::{sanitize_package_name, title_from_package_name};
    use r_description::RDescription;

    #[test]
    fn sanitizes_directory_name_to_package_name() {
        assert_eq!(
            sanitize_package_name("my-package_name").unwrap(),
            "my.package.name"
        );
    }

    #[test]
    fn rejects_package_name_without_leading_letter() {
        assert_eq!(
            sanitize_package_name("123pkg").unwrap_err().to_string(),
            "failed to derive package name for DESCRIPTION: package name must start with a letter"
        );
    }

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
