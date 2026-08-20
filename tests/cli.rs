mod common;

use common::*;

#[test]
fn runs_rpx_help_inside_custom_r_image() {
    let container = start_container();
    let (exit_code, stdout, stderr) = run_command(&container, &["rpx", "--help"]);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Usage:"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn runs_rpx_run_with_isolated_library() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-run";
    create_package_project(&container, project_path);
    let command = format!(
        "mkdir -p {project_path} && cd {project_path} && rpx run Rscript -e \"cat(.libPaths()[1])\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("rpx/libraries/"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn run_from_subdirectory_uses_project_library_and_preserves_working_directory() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-run-subdirectory";
    let working_path = "/tmp/rpx-project-run-subdirectory/scripts/nested";
    create_package_project(&container, project_path);

    let command = format!(
        "mkdir -p {working_path} && cd {working_path} && rpx run Rscript -e \"cat(getwd(), '\\n', .libPaths()[1], sep = '')\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some(working_path), "stdout was: {stdout}");
    let library = lines.next().expect("library path should be printed");
    assert!(
        library.contains("/libraries/") && library.ends_with("/library"),
        "stdout was: {stdout}"
    );
}

#[test]
fn runs_rpx_init_in_empty_directory() {
    let container = start_container();
    let project_path = "/tmp/new-rpx-project";
    let command = format!("mkdir -p {project_path} && cd {project_path} && rpx init");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Initialized project at"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );

    let description =
        run_shell_command(&container, &format!("cd {project_path} && cat DESCRIPTION"));
    assert_eq!(
        description.0, 0,
        "stdout was: {}\nstderr was: {}",
        description.1, description.2
    );
    assert!(
        description.1.contains("Package: new.rpx.project"),
        "DESCRIPTION was: {}",
        description.1
    );
    assert!(
        description.1.contains("Title: New Rpx Project"),
        "DESCRIPTION was: {}",
        description.1
    );

    let lockfile = run_shell_command(&container, &format!("cat {project_path}/rpx.lock"));
    assert_eq!(
        lockfile.0, 0,
        "stdout was: {}\nstderr was: {}",
        lockfile.1, lockfile.2
    );

    let (exit_code, stdout, stderr) =
        run_shell_command(&container, &format!("cd {project_path} && rpx status"));
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Project is in sync"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn default_init_passes_r_cmd_check_from_source() {
    let container = start_container();
    let project_path = "/tmp/rpx-init-check";
    let init_command = format!("mkdir -p {project_path} && cd {project_path} && rpx init");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &init_command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let (exit_code, stdout, stderr) = run_shell_command(
        &container,
        &format!("cd /tmp && R CMD check --no-manual {project_path}"),
    );

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
}

#[test]
fn init_creates_package_that_sync_can_install() {
    let container = start_container();
    let project_path = "/tmp/rpx-init-target/projects/example";
    let command = format!(
        "cd /tmp && rpx init {project_path} --name custom.pkg --title 'Custom Package' --description 'A custom package.' --license gpl-3 && cd {project_path} && rpx sync"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let description = run_shell_command(&container, &format!("cat {project_path}/DESCRIPTION"));
    assert_eq!(
        description.0, 0,
        "stdout was: {}\nstderr was: {}",
        description.1, description.2
    );
    for field in [
        "Package: custom.pkg",
        "Title: Custom Package",
        "Description: A custom package.",
        "License: GPL-3",
        "Authors@R: person(given = \"Package Author\", email = \"author@example.com\", role = c(\"aut\", \"cre\"))",
        "Author: Package Author [aut, cre]",
        "Maintainer: Package Author <author@example.com>",
    ] {
        assert!(
            description.1.contains(field),
            "DESCRIPTION was: {}",
            description.1
        );
    }
    let retry = run_shell_command(
        &container,
        &format!("cd /tmp && rpx init {project_path} --title Replaced"),
    );
    assert_ne!(
        retry.0, 0,
        "stdout was: {}\nstderr was: {}",
        retry.1, retry.2
    );
    assert!(retry.2.contains("not empty"), "stderr was: {}", retry.2);

    let unchanged = run_shell_command(&container, &format!("cat {project_path}/DESCRIPTION"));
    assert_eq!(
        unchanged.0, 0,
        "stdout was: {}\nstderr was: {}",
        unchanged.1, unchanged.2
    );
    assert!(
        unchanged.1.contains("Title: Custom Package"),
        "DESCRIPTION was: {}",
        unchanged.1
    );

    let buildignore = run_shell_command(&container, &format!("cat {project_path}/.Rbuildignore"));
    assert_eq!(
        buildignore.0, 0,
        "stdout was: {}\nstderr was: {}",
        buildignore.1, buildignore.2
    );
    for pattern in ["^rpx\\.lock$", "^docs$", "^\\.github$", "^[.]?air[.]toml$"] {
        assert!(
            buildignore.1.lines().any(|line| line == pattern),
            ".Rbuildignore was: {}",
            buildignore.1
        );
    }

    let hidden_target = "/tmp/rpx-init-target/nonempty-hidden";
    let hidden = run_shell_command(
        &container,
        &format!("mkdir -p {hidden_target}/.git && rpx init {hidden_target}"),
    );
    assert_ne!(
        hidden.0, 0,
        "stdout was: {}\nstderr was: {}",
        hidden.1, hidden.2
    );
    assert!(hidden.2.contains("not empty"), "stderr was: {}", hidden.2);
}

#[test]
fn init_creates_project_that_can_add_dependencies() {
    let container = start_container();
    let project_path = "/tmp/rpx-init-add";
    let command =
        format!("mkdir -p {project_path} && cd {project_path} && rpx init && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let lockfile = run_shell_command(&container, &format!("cd {project_path} && cat rpx.lock"));
    assert_eq!(
        lockfile.0, 0,
        "stdout was: {}\nstderr was: {}",
        lockfile.1, lockfile.2
    );
    assert!(
        lockfile.1.contains("\"digest\""),
        "lockfile was: {}",
        lockfile.1
    );
}

#[test]
fn clean_removes_project_library_and_cache_directories() {
    let container = start_container();
    let project_path = "/tmp/rpx-clean";
    let setup_command =
        format!("mkdir -p {project_path} && cd {project_path} && rpx init && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &setup_command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let library_path_command =
        format!("cd {project_path} && rpx run Rscript -e \"cat(.libPaths()[1])\"");
    let (exit_code, library_path, stderr) = run_shell_command(&container, &library_path_command);
    assert_eq!(
        exit_code, 0,
        "stdout was: {library_path}\nstderr was: {stderr}"
    );

    let library_path = library_path.trim();
    let check_before_command = format!("test -d '{library_path}' && test -d /root/.cache/rpx");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &check_before_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let working_path = "/tmp/rpx-clean/nested";
    let clean_command = format!("mkdir -p {working_path} && cd {working_path} && rpx clean");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &clean_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Removed project library and cache directories"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );

    let check_after_command = format!("test ! -d '{library_path}' && test ! -d /root/.cache/rpx");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &check_after_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
}
