mod common;

use common::*;

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
    let check =
        format!("cat('{package}' %in% rownames(installed.packages(lib.loc = .libPaths()[1])))");
    let command =
        format!("mkdir -p {project_path} && cd {project_path} && rpx run Rscript -e \"{check}\"");
    let (exit_code, stdout, stderr) = run_shell_command(container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains(expected),
        "expected package state {expected}\nstdout was: {stdout}\nstderr was: {stderr}"
    );
}

fn assert_package_version(
    container: &testcontainers::core::Container<testcontainers::GenericImage>,
    project_path: &str,
    package: &str,
    expected: &str,
) {
    let check =
        format!("cat(installed.packages(lib.loc = .libPaths()[1])['{package}', 'Version'])");
    let command =
        format!("mkdir -p {project_path} && cd {project_path} && rpx run Rscript -e \"{check}\"");
    let (exit_code, stdout, stderr) = run_shell_command(container, &command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains(expected),
        "expected package version {expected}\nstdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn runs_rpx_lock_from_current_library() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-lock";
    create_package_project(&container, project_path);
    let install_command = format!(
        "mkdir -p {project_path} && cd {project_path} && rpx run Rscript -e \"install.packages('digest')\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &install_command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let lock_command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &lock_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let lockfile = read_project_file(&container, project_path, "rpx.lock");
    assert!(
        lockfile.contains("\"version\": 4"),
        "lockfile was: {lockfile}"
    );
    assert!(
        lockfile.contains("\"revision\": 0"),
        "lockfile was: {lockfile}"
    );
    assert!(
        lockfile.contains("\"packages\": {}"),
        "lockfile was: {lockfile}"
    );
}

#[test]
fn runs_rpx_sync_from_lockfile_without_mutating_it() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-sync";
    create_package_project(&container, project_path);
    let add_command = format!("cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let remove_package_dir = format!(
        "cd {project_path} && rm -rf \"$(rpx run Rscript -e \"cat(file.path(.libPaths()[1], 'digest'))\")\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &remove_package_dir);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let before = read_project_file(&container, project_path, "rpx.lock");
    assert_package_state(&container, project_path, "digest", "FALSE");

    let sync_command = format!("cd {project_path} && rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    assert_package_state(&container, project_path, "digest", "TRUE");

    let after = read_project_file(&container, project_path, "rpx.lock");
    assert_eq!(
        after, before,
        "lockfile changed during sync\nbefore:\n{before}\nafter:\n{after}"
    );
}

#[test]
fn runs_rpx_sync_removes_extra_packages() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-sync-prune";
    create_package_project(&container, project_path);

    let setup_command = format!("cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &setup_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let extra_command =
        format!("cd {project_path} && rpx run Rscript -e \"install.packages('jsonlite')\"");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &extra_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert_package_state(&container, project_path, "jsonlite", "TRUE");

    let before = read_project_file(&container, project_path, "rpx.lock");

    let sync_command = format!("cd {project_path} && rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    assert_package_state(&container, project_path, "digest", "TRUE");
    assert_package_state(&container, project_path, "jsonlite", "FALSE");

    let after = read_project_file(&container, project_path, "rpx.lock");
    assert_eq!(
        after, before,
        "lockfile changed during strict sync\nbefore:\n{before}\nafter:\n{after}"
    );
}

#[test]
fn runs_rpx_sync_restores_locked_versions() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-sync-version";
    create_package_project(&container, project_path);
    let add_dependency =
        format!("cd {project_path} && cat >> DESCRIPTION <<'EOF'\nImports: digest\nEOF");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_dependency);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let seed_lockfile = format!(
        "mkdir -p {project_path} && cd {project_path} && r_version=$(Rscript -e 'cat(as.character(getRversion()))') && cat > rpx.lock <<EOF\n{{\n  \"version\": 4,\n  \"revision\": 0,\n  \"repositories\": [\n    {{\n      \"url\": \"https://upstream.rrepo.dev/cran\",\n      \"kind\": \"rrepo\"\n    }}\n  ],\n  \"r\": {{\n    \"version\": \"$r_version\"\n  }},\n  \"roots\": [\n    {{\n      \"package\": \"digest\",\n      \"constraint\": \"*\"\n    }}\n  ],\n  \"packages\": {{\n    \"digest\": {{\n      \"package\": \"digest\",\n      \"version\": \"0.6.37\",\n      \"source\": \"repository\",\n      \"source_url\": \"https://upstream.rrepo.dev/cran/packages/digest/versions/0.6.37/source\"\n    }}\n  }}\n}}\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &seed_lockfile);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let before = read_project_file(&container, project_path, "rpx.lock");

    let sync_command = format!("cd {project_path} && rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    assert_package_version(&container, project_path, "digest", "0.6.37");

    let after = read_project_file(&container, project_path, "rpx.lock");
    assert_eq!(
        after, before,
        "lockfile changed during strict sync\nbefore:\n{before}\nafter:\n{after}"
    );
}

#[test]
fn refuses_to_sync_old_lockfile() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-sync-old-lockfile";
    create_package_project(&container, project_path);
    let seed_lockfile = format!(
        "mkdir -p {project_path} && cd {project_path} && cat > rpx.lock <<'EOF'\n{{\n  \"version\": 3,\n  \"revision\": 1,\n  \"registry\": \"https://api.rrepo.org\",\n  \"roots\": [],\n  \"packages\": {{}}\n}}\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &seed_lockfile);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let sync_command = format!("cd {project_path} && rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stderr.contains("unsupported rpx.lock schema version 3"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stderr.contains("rpx::project::lockfile_unsupported_version"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn refuses_to_sync_newer_lockfile() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-sync-newer-lockfile";
    create_package_project(&container, project_path);
    let seed_lockfile = format!(
        "mkdir -p {project_path} && cd {project_path} && cat > rpx.lock <<'EOF'\n{{\n  \"version\": 999,\n  \"registry\": \"https://api.rrepo.org\",\n  \"roots\": [],\n  \"packages\": {{}}\n}}\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &seed_lockfile);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let sync_command = format!("cd {project_path} && rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stderr.contains("unsupported rpx.lock schema version 999"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stderr.contains("rpx::project::lockfile_unsupported_version"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn runs_rpx_sync_with_reordered_lockfile_roots() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-sync-ordered-roots";
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
Imports: digest, jsonlite",
    );

    let lock_command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &lock_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let reorder_command = format!(
        r#"cd {project_path} && perl -0pi -e 's/"roots": \[\s+\{{\s+"package": "digest",\s+"constraint": "\*"\s+\}},\s+\{{\s+"package": "jsonlite",\s+"constraint": "\*"\s+\}}\s+\]/"roots": [
    {{
      "package": "jsonlite",
      "constraint": "*"
    }},
    {{
      "package": "digest",
      "constraint": "*"
    }}
  ]/' rpx.lock"#
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &reorder_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let sync_command = format!("cd {project_path} && rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
}

#[test]
fn refuses_to_sync_repository_changes() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-repo-drift";
    create_package_project(&container, project_path);

    let setup_command = format!("cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &setup_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let mutate_command = format!(
        "cd {project_path} && cat >> DESCRIPTION <<'EOF'\nAdditional_repositories: https://packagemanager.posit.co/cran/latest\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &mutate_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let sync_command = format!("cd {project_path} && rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stderr.contains("rpx::project::repositories_changed"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}
