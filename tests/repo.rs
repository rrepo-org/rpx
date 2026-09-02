mod common;

use common::{create_package_project, run_shell_command, start_container};
use testcontainers::{GenericImage, core::Container};

fn append_description(container: &Container<GenericImage>, project_path: &str, contents: &str) {
    let command = format!("cat >> {project_path}/DESCRIPTION <<'EOF'\n{contents}\nEOF");
    let (exit_code, stdout, stderr) = run_shell_command(container, &command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
}

fn read_project_file(
    container: &Container<GenericImage>,
    project_path: &str,
    file: &str,
) -> String {
    let command = format!("cat {project_path}/{file}");
    let (exit_code, stdout, stderr) = run_shell_command(container, &command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    stdout
}

#[test]
fn lists_description_repositories_without_a_lockfile() {
    let container = start_container();
    let project_path = "/tmp/rpx-repo-list";
    let working_path = "/tmp/rpx-repo-list/nested/directory";
    create_package_project(&container, project_path);
    append_description(
        &container,
        project_path,
        "Config/rpx/base-repository: https://base.example/cran\nRemotes: github::owner/repository@main\nAdditional_repositories: https://additional.example/cran",
    );

    let command = format!("mkdir -p {working_path} && cd {working_path} && rpx repo list");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let base = stdout.find("https://base.example/cran").unwrap();
    let remote = stdout.find("github::owner/repository@main").unwrap();
    let additional = stdout.find("https://additional.example/cran").unwrap();
    assert!(base < remote && remote < additional, "stdout was: {stdout}");
    assert!(!stdout.contains("rpx.lock"), "stdout was: {stdout}");
}

#[test]
fn list_filter_reports_configured_base_source() {
    let container = start_container();
    let project_path = "/tmp/rpx-repo-list-filter";
    create_package_project(&container, project_path);
    append_description(
        &container,
        project_path,
        "Config/rpx/base-repository: https://fallback.example/cran\nRemotes: github::owner/repository\nAdditional_repositories: https://additional.example/cran",
    );

    let command = format!("cd {project_path} && rpx repo list --type base");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(stdout.contains("configured"), "stdout was: {stdout}");
    assert!(
        stdout.contains("https://fallback.example/cran"),
        "stdout was: {stdout}"
    );
    assert!(!stdout.contains("github::"), "stdout was: {stdout}");
    assert!(
        !stdout.contains("additional.example"),
        "stdout was: {stdout}"
    );
}

#[test]
fn resets_configured_base_and_relocks_with_builtin_repository() {
    let container = start_container();
    let project_path = "/tmp/rpx-repo-base-reset";
    create_package_project(&container, project_path);
    append_description(
        &container,
        project_path,
        "Config/rpx/base-repository: https://unused.example/cran\nTitle: Normalized Title",
    );

    let command = format!("cd {project_path} && rpx repo base reset");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let description = read_project_file(&container, project_path, "DESCRIPTION");
    let lockfile = read_project_file(&container, project_path, "rpx.lock");
    assert!(!description.contains("Config/rpx/base-repository"));
    assert_eq!(description.matches("Title:").count(), 1);
    assert!(description.contains("Title: Normalized Title"));
    assert!(description.find("Title:").unwrap() < description.find("Version:").unwrap());
    assert!(lockfile.contains("https://upstream.rrepo.dev/cran"));
}

#[test]
fn sets_normalized_base_repository() {
    let container = start_container();
    let project_path = "/tmp/rpx-repo-base-set";
    create_package_project(&container, project_path);

    let command =
        format!("cd {project_path} && rpx repo base set https://upstream.rrepo.dev/cran/");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let description = read_project_file(&container, project_path, "DESCRIPTION");
    assert!(description.contains("Config/rpx/base-repository: https://upstream.rrepo.dev/cran"));
    assert!(!description.contains("https://upstream.rrepo.dev/cran/"));

    let command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("already up to date"),
        "stdout was: {stdout}"
    );
}

#[test]
fn additional_shortcut_detects_normalized_duplicate_without_relocking() {
    let container = start_container();
    let project_path = "/tmp/rpx-repo-additional-shortcut";
    create_package_project(&container, project_path);
    append_description(
        &container,
        project_path,
        "Additional_repositories: https://upstream.rrepo.dev/cran/",
    );
    let before = read_project_file(&container, project_path, "DESCRIPTION");

    let command = format!("cd {project_path} && rpx repo add https://upstream.rrepo.dev/cran");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("already configured"),
        "stdout was: {stdout}"
    );
    let lock_check = format!("test ! -e {project_path}/rpx.lock");
    let (exit_code, _, stderr) = run_shell_command(&container, &lock_check);
    assert_eq!(exit_code, 0, "stderr was: {stderr}");
    assert_eq!(
        read_project_file(&container, project_path, "DESCRIPTION"),
        before
    );
}

#[test]
fn removes_additional_repository_and_preserves_other_description_fields() {
    let container = start_container();
    let project_path = "/tmp/rpx-repo-additional-remove";
    create_package_project(&container, project_path);
    append_description(
        &container,
        project_path,
        "Additional_repositories: https://unused.example/cran\nSuggests: testthat",
    );

    let command =
        format!("cd {project_path} && rpx repo additional remove https://unused.example/cran/");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let description = read_project_file(&container, project_path, "DESCRIPTION");
    assert!(!description.contains("Additional_repositories"));
    assert!(description.contains("Suggests:\n    testthat"));
}

#[test]
fn removes_remote_by_normalized_spec_and_relocks() {
    let container = start_container();
    let project_path = "/tmp/rpx-repo-remote-remove";
    create_package_project(&container, project_path);
    append_description(
        &container,
        project_path,
        "Remotes: github::owner/repository@main",
    );

    let command = format!("cd {project_path} && rpx repo remote remove owner/repository@main");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let description = read_project_file(&container, project_path, "DESCRIPTION");
    assert!(!description.contains("Remotes:"));
    read_project_file(&container, project_path, "rpx.lock");
}

#[test]
fn invalid_remote_addition_leaves_project_files_unchanged() {
    let container = start_container();
    let project_path = "/tmp/rpx-repo-invalid-remote";
    create_package_project(&container, project_path);
    let before = read_project_file(&container, project_path, "DESCRIPTION");

    let command = format!(
        "cd {project_path} && rpx repo remote add archive=url::https://example.test/package.tar.gz"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &command);

    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stderr.contains("unsupported remote"),
        "stderr was: {stderr}"
    );
    assert_eq!(
        read_project_file(&container, project_path, "DESCRIPTION"),
        before
    );
    let lock_check = format!("test ! -e {project_path}/rpx.lock");
    let (exit_code, _, stderr) = run_shell_command(&container, &lock_check);
    assert_eq!(exit_code, 0, "stderr was: {stderr}");
}
