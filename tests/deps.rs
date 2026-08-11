mod common;

use common::*;
use r_description::lossless::RDescription;
use serde_json::{Value, json};

fn write_description(
    container: &testcontainers::core::Container<testcontainers::GenericImage>,
    project_path: &str,
    contents: &str,
) {
    let command = format!(
        "mkdir -p {project_path} && touch {project_path}/NAMESPACE && cat > {project_path}/DESCRIPTION <<'EOF'\n{contents}\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(container, &command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
}

fn read_project_file(
    container: &testcontainers::core::Container<testcontainers::GenericImage>,
    project_path: &str,
    file_name: &str,
) -> String {
    let command = format!("cd {project_path} && cat {file_name}");
    let (exit_code, stdout, stderr) = run_shell_command(container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    stdout
}

fn assert_package_state(
    container: &testcontainers::core::Container<testcontainers::GenericImage>,
    project_path: &str,
    package: &str,
    expected: &str,
) {
    let check = format!(
        "cat(tryCatch({{ library('{package}', character.only = TRUE, lib.loc = .libPaths()[1]); TRUE }}, error = function(error) {{ message(conditionMessage(error)); FALSE }}))"
    );
    let command =
        format!("mkdir -p {project_path} && cd {project_path} && rpx run Rscript -e \"{check}\"");
    let (exit_code, stdout, stderr) = run_shell_command(container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains(expected),
        "expected package state {expected}\nstdout was: {stdout}\nstderr was: {stderr}"
    );
}

fn parsed_description(contents: &str) -> RDescription {
    contents.parse().expect("DESCRIPTION should parse")
}

fn relation_names(relations: Option<r_description::lossless::Relations>) -> Vec<String> {
    relations
        .into_iter()
        .flat_map(|relations| relations.iter())
        .map(|relation| relation.name())
        .collect()
}

#[test]
fn reports_pubgrub_no_solution_explanation() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-no-solution";
    write_description(
        &container,
        project_path,
        "Package: rlang\nVersion: 1.0.1\nTitle: Local rlang\nDescription: Resolver conflict fixture.\nLicense: MIT\nAuthor: Test Author\nMaintainer: Test Author <test@example.com>",
    );

    let command = format!("cd {project_path} && rpx add 'testthat@>=3.1.8'");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stderr.contains("rpx::lock::no_solution"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn runs_rpx_add_inside_custom_r_image() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-add";
    let working_path = "/tmp/rpx-project-add/subdir";
    create_package_project(&container, project_path);

    let command = format!("mkdir -p {working_path} && cd {working_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Added digest"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert_package_state(&container, working_path, "digest", "TRUE");
    assert_package_state(&container, working_path, "testpkg", "TRUE");

    let lockfile =
        serde_json::from_str::<Value>(&read_project_file(&container, project_path, "rpx.lock"))
            .expect("lockfile should parse");
    assert!(lockfile["packages"].get("digest").is_some());
    assert!(lockfile["packages"].get("testpkg").is_none());
    assert_eq!(
        lockfile["repos"][0]["url"],
        "https://upstream.rrepo.dev/cran"
    );
    assert!(
        lockfile["requirements"]
            .as_array()
            .is_some_and(|requirements| {
                requirements
                    .iter()
                    .filter_map(Value::as_str)
                    .all(|requirement| requirement.starts_with("digest "))
            })
    );

    let description = read_project_file(&container, project_path, "DESCRIPTION");
    assert!(
        description.contains("digest (>=") && description.contains("digest (<"),
        "DESCRIPTION was: {description}"
    );
}

#[test]
fn adds_existing_dependency_as_development_dependency() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-add-dev";
    write_description(
        &container,
        project_path,
        "Package: testpkg
Version: 0.1.0
Title: Test Package
Description: Test package for rpx integration tests.
License: MIT
Author: Test Author
Maintainer: Test Author <test@example.com>
Imports: digest (>= 0.6.37), digest (< 1.0.0)",
    );

    let command = format!("cd {project_path} && rpx add --dev digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Added digest"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert_package_state(&container, project_path, "digest", "TRUE");

    let description =
        parsed_description(&read_project_file(&container, project_path, "DESCRIPTION"));
    assert!(
        !relation_names(description.imports()).contains(&"digest".to_string()),
        "DESCRIPTION was: {description}"
    );
    assert_eq!(
        description
            .suggests()
            .unwrap()
            .iter()
            .filter(|relation| relation.name() == "digest")
            .map(|relation| relation.to_string())
            .collect::<Vec<_>>(),
        vec!["digest (< 1.0.0)", "digest (>= 0.6.37)"]
    );
}

#[test]
fn constrained_add_replaces_default_dependency_bounds() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-add-constraint";
    create_package_project(&container, project_path);

    let add_command = format!("cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let constrain_command = format!("cd {project_path} && rpx add 'digest@>=0.6.37'");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &constrain_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Added digest (>= 0.6.37)"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );

    let description =
        parsed_description(&read_project_file(&container, project_path, "DESCRIPTION"));
    let digest_relations = description
        .imports()
        .into_iter()
        .flat_map(|relations| relations.iter())
        .filter(|relation| relation.name() == "digest")
        .map(|relation| relation.to_string())
        .collect::<Vec<_>>();
    assert_eq!(digest_relations, vec!["digest (>= 0.6.37)"]);
    assert!(
        !relation_names(description.depends()).contains(&"digest".to_string())
            && !relation_names(description.linking_to()).contains(&"digest".to_string())
            && !relation_names(description.suggests()).contains(&"digest".to_string())
            && !relation_names(description.enhances()).contains(&"digest".to_string()),
        "DESCRIPTION was: {}",
        read_project_file(&container, project_path, "DESCRIPTION")
    );
}

#[test]
fn duplicate_add_reuses_lock_and_restores_missing_package() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-add-reuse";
    create_package_project(&container, project_path);

    let add_command = format!("cd {project_path} && rpx add digest");
    let reuse_command = format!("cd {project_path} && rpx add --default-repo digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let lockfile = read_project_file(&container, project_path, "rpx.lock");

    let remove_package_dir = format!(
        "cd {project_path} && rm -rf \"$(rpx run Rscript -e \"cat(file.path(.libPaths()[1], 'digest'))\")\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &remove_package_dir);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let (exit_code, stdout, stderr) = run_shell_command(&container, &reuse_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert_package_state(&container, project_path, "digest", "TRUE");
    assert_eq!(
        read_project_file(&container, project_path, "rpx.lock"),
        lockfile
    );
}

#[test]
fn reused_add_synchronizes_with_the_updated_description() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-add-reuse-description";
    create_package_project(&container, project_path);
    let add_suggests = format!(
        "cd {project_path} && cat >> DESCRIPTION <<'EOF'\nSuggests: digest (>= 0.6.37)\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_suggests);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let lock_command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &lock_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let lockfile = read_project_file(&container, project_path, "rpx.lock");

    let add_command = format!("cd {project_path} && rpx add 'digest@>=0.6.37'");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert_package_state(&container, project_path, "digest", "TRUE");
    assert_eq!(
        read_project_file(&container, project_path, "rpx.lock"),
        lockfile
    );
}

#[test]
fn records_base_package_as_runtime_requirement() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-add-base-package";
    create_package_project(&container, project_path);

    let command = format!("mkdir -p {project_path} && cd {project_path} && rpx add grid");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Added grid"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert_package_state(&container, project_path, "grid", "TRUE");

    let lockfile =
        serde_json::from_str::<Value>(&read_project_file(&container, project_path, "rpx.lock"))
            .expect("lockfile should parse");
    assert!(lockfile["r"].is_string());
    assert_eq!(lockfile["requirements"], json!(["grid"]));
    assert_eq!(lockfile["packages"], json!({}));
}

#[test]
fn runs_rpx_remove_inside_custom_r_image() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-remove";
    create_package_project(&container, project_path);

    let add_command = format!("mkdir -p {project_path} && cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert_package_state(&container, project_path, "digest", "TRUE");

    let remove_command = format!("cd {project_path} && rpx remove digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &remove_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Removed digest"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert_package_state(&container, project_path, "digest", "FALSE");

    let lockfile = read_project_file(&container, project_path, "rpx.lock");
    assert!(!lockfile.contains("\"digest\""), "lockfile was: {lockfile}");

    let description = read_project_file(&container, project_path, "DESCRIPTION");
    assert!(
        !description.contains("digest"),
        "DESCRIPTION was: {description}"
    );
}

#[test]
fn reports_when_removed_package_is_already_missing_from_library() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-remove-missing";
    create_package_project(&container, project_path);

    let add_command = format!("mkdir -p {project_path} && cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let remove_package_dir = format!(
        "cd {project_path} && rm -rf \"$(rpx run Rscript -e \"cat(file.path(.libPaths()[1], 'digest'))\")\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &remove_package_dir);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let remove_command = format!("cd {project_path} && rpx remove digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &remove_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("digest is already missing from the project library"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert_package_state(&container, project_path, "digest", "FALSE");
}

#[test]
fn undeclared_remove_reuses_lock_and_removes_installed_package() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-remove-reuse";
    create_package_project(&container, project_path);

    let lock_command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &lock_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let lockfile = read_project_file(&container, project_path, "rpx.lock");

    let install_command =
        format!("cd {project_path} && rpx run Rscript -e \"install.packages('jsonlite')\"");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &install_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let remove_command = format!("cd {project_path} && rpx remove jsonlite");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &remove_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Removed jsonlite"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert_package_state(&container, project_path, "jsonlite", "FALSE");
    assert_eq!(
        read_project_file(&container, project_path, "rpx.lock"),
        lockfile
    );
}

#[test]
fn adds_and_removes_multiple_packages() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-multi-deps";
    create_package_project(&container, project_path);

    let add_command = format!("cd {project_path} && rpx add digest cli");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Added digest, cli"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert_package_state(&container, project_path, "digest", "TRUE");
    assert_package_state(&container, project_path, "cli", "TRUE");

    let remove_command = format!("cd {project_path} && rpx remove digest cli");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &remove_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Removed cli, digest"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert_package_state(&container, project_path, "digest", "FALSE");
    assert_package_state(&container, project_path, "cli", "FALSE");
}

#[test]
fn runs_rpx_lock_without_installing_packages() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-lock";
    write_description(
        &container,
        project_path,
        "Package: testpkg
Version: 0.1.0
Title: Test Package
Description: Test package for rpx integration tests.
License: MIT
Author: Test Author
Maintainer: Test Author <test@example.com>
Imports: digest",
    );

    let command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert_package_state(&container, project_path, "digest", "FALSE");

    let lockfile =
        serde_json::from_str::<Value>(&read_project_file(&container, project_path, "rpx.lock"))
            .expect("lockfile should parse");
    assert!(
        lockfile["repos"].as_array().is_some_and(|repositories| {
            repositories
                .iter()
                .any(|repository| repository["url"] == "https://upstream.rrepo.dev/cran")
        }),
        "lockfile was: {lockfile}"
    );
    assert_eq!(
        lockfile["packages"]["digest"]["repository"],
        "https://upstream.rrepo.dev/cran"
    );
}

#[test]
fn inherits_and_overrides_default_repository_policy() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-default-repository-policy";
    create_package_project(&container, project_path);

    let command = format!("cd {project_path} && rpx lock --no-default-repo");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let lockfile = read_project_file(&container, project_path, "rpx.lock");
    assert!(
        !lockfile.contains("https://upstream.rrepo.dev/cran"),
        "lockfile was: {lockfile}"
    );

    let command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let lockfile = read_project_file(&container, project_path, "rpx.lock");
    assert!(
        !lockfile.contains("https://upstream.rrepo.dev/cran"),
        "lockfile was: {lockfile}"
    );

    let command = format!("cd {project_path} && rpx lock --default-repo");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let lockfile = read_project_file(&container, project_path, "rpx.lock");
    assert!(
        lockfile.contains("https://upstream.rrepo.dev/cran"),
        "lockfile was: {lockfile}"
    );
}

#[test]
fn does_not_add_import_when_package_is_already_in_depends() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-add-depends";
    write_description(
        &container,
        project_path,
        "Package: testpkg
Version: 0.1.0
Title: Test Package
Description: Test package for rpx integration tests.
License: MIT
Author: Test Author
Maintainer: Test Author <test@example.com>
Depends: R (>= 4.3), digest",
    );

    let command = format!("cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let description = read_project_file(&container, project_path, "DESCRIPTION");
    let parsed = parsed_description(&description);
    let depends = relation_names(parsed.depends());
    let imports = relation_names(parsed.imports());
    assert!(
        depends.contains(&"digest".to_string()) && depends.contains(&"R".to_string()),
        "DESCRIPTION was: {description}"
    );
    assert!(
        !imports.contains(&"digest".to_string()),
        "DESCRIPTION was: {description}"
    );
}

#[test]
fn removes_dependency_from_depends_while_preserving_r_requirement() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-remove-depends";
    write_description(
        &container,
        project_path,
        "Package: testpkg
Version: 0.1.0
Title: Test Package
Description: Test package for rpx integration tests.
License: MIT
Author: Test Author
Maintainer: Test Author <test@example.com>
Depends: R (>= 4.3), digest",
    );

    let install_command =
        format!("cd {project_path} && rpx run Rscript -e \"install.packages('digest')\"");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &install_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let command = format!("cd {project_path} && rpx remove digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let description = read_project_file(&container, project_path, "DESCRIPTION");
    let parsed = parsed_description(&description);
    let depends = relation_names(parsed.depends());
    assert!(
        depends == vec!["R".to_string()],
        "DESCRIPTION was: {description}"
    );
    assert!(
        !description.contains("digest"),
        "DESCRIPTION was: {description}"
    );
}
