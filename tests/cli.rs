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

    let clean_command = format!("cd {project_path} && rpx clean");
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
