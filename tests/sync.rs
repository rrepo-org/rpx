mod common;

use common::*;
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

fn install_sync_failure_wrappers(
    container: &testcontainers::core::Container<testcontainers::GenericImage>,
    project_path: &str,
    wrapper_path: &str,
    state_path: &str,
) -> String {
    let command = format!(
        r#"mkdir -p {wrapper_path}/real {state_path}
ln -s "$(command -v R)" {wrapper_path}/real/R
ln -s "$(command -v Rscript)" {wrapper_path}/real/Rscript
cat > {wrapper_path}/R <<'EOF'
#!/bin/sh
case "$*" in
    *CMD*build*"$RPX_TEST_PROJECT"*)
        if [ "${{RPX_TEST_ROOT_DELAY:-}}" = 1 ] && [ "${{RPX_TEST_FAIL_DIGEST:-}}" = 1 ]; then
            rm -f "$RPX_TEST_STATE/root-cleaned" "$RPX_TEST_STATE/digest-failed"
            touch "$RPX_TEST_STATE/root-active"
            attempts=0
            while [ ! -f "$RPX_TEST_STATE/digest-failed" ] && [ "$attempts" -lt 1200 ]; do
                sleep 0.05
                attempts=$((attempts + 1))
            done
            sleep 1
            {wrapper_path}/real/R "$@"
            status=$?
            rm -f "$RPX_TEST_STATE/root-active"
            touch "$RPX_TEST_STATE/root-cleaned"
            exit "$status"
        fi
        ;;
esac
exec {wrapper_path}/real/R "$@"
EOF
cat > {wrapper_path}/Rscript <<'EOF'
#!/bin/sh
case "$*" in
    *install.packages*digest_*)
        if [ "${{RPX_TEST_FAIL_DIGEST:-}}" = 1 ]; then
            attempts=0
            while [ ! -f "$RPX_TEST_STATE/root-active" ] && [ "$attempts" -lt 100 ]; do
                sleep 0.05
                attempts=$((attempts + 1))
            done
            if [ ! -f "$RPX_TEST_STATE/root-active" ]; then
                echo "digest install did not overlap the root build" >&2
                exit 98
            fi
            touch "$RPX_TEST_STATE/digest-failed"
            echo "injected digest install failure" >&2
            exit 97
        fi
        ;;
esac
exec {wrapper_path}/real/Rscript "$@"
EOF
chmod +x {wrapper_path}/R {wrapper_path}/Rscript"#
    );
    let (exit_code, stdout, stderr) = run_shell_command(container, &command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    format!(
        "PATH={wrapper_path}:$PATH RPX_TEST_PROJECT={project_path} RPX_TEST_STATE={state_path} RPX_TEST_ROOT_DELAY=1"
    )
}

#[test]
fn runs_rpx_lock_from_current_library() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-lock";
    create_package_project(&container, project_path);
    lock_package_project(&container, project_path);
    let install_command = format!(
        "mkdir -p {project_path} && cd {project_path} && rpx run Rscript -e \"install.packages('digest')\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &install_command);

    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let lock_command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &lock_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let lockfile =
        serde_json::from_str::<Value>(&read_project_file(&container, project_path, "rpx.lock"))
            .expect("lockfile should parse");
    assert_eq!(lockfile["version"], 5);
    assert_eq!(lockfile["revision"], 0);
    assert_eq!(lockfile["packages"], json!({}));
}

#[test]
fn runs_rpx_sync_from_lockfile_without_mutating_it() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-sync";
    let working_path = "/tmp/rpx-project-sync/nested";
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
    let status_command = format!("cd {project_path} && rpx status");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &status_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stderr.contains("Required packages not installed") && stderr.contains("digest"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );

    let sync_command = format!("mkdir -p {working_path} && cd {working_path} && rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    assert_package_state(&container, project_path, "digest", "TRUE");
    assert_package_state(&container, project_path, "testpkg", "TRUE");

    let after = read_project_file(&container, project_path, "rpx.lock");
    assert_eq!(
        after, before,
        "lockfile changed during sync\nbefore:\n{before}\nafter:\n{after}"
    );
}

#[test]
fn sync_installs_project_without_mutating_its_sources() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-sync-clean-sources";
    let temp_path = "/tmp/rpx-project-sync-clean-sources-tmp";
    create_package_project(&container, project_path);
    let add_build_sources = format!(
        "mkdir -p {temp_path} && cd {project_path} && mkdir src && cat > configure <<'EOF'\n#!/bin/sh\ntouch configured-during-install\nEOF\nchmod +x configure && cat > src/native.c <<'EOF'\nvoid native(void) {{}}\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_build_sources);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let lock_command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &lock_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let sync_command = format!("cd {project_path} && TMPDIR={temp_path} rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert_package_state(&container, project_path, "testpkg", "TRUE");

    let assert_clean = format!(
        "cd {project_path} && test ! -e configured-during-install && test ! -e src/native.o && test ! -e src/testpkg.so && set -- {temp_path}/rpx-build-* && test ! -e \"$1\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &assert_clean);
    assert_eq!(
        exit_code, 0,
        "project sources or temporary build files were left behind\nstdout was: {stdout}\nstderr was: {stderr}"
    );

    let fail_configure = format!(
        "cd {project_path} && cat > configure <<'EOF'\n#!/bin/sh\ntouch configured-during-install\nexit 1\nEOF\nchmod +x configure"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &fail_configure);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &assert_clean);
    assert_eq!(
        exit_code, 0,
        "failed installation left project sources or temporary build files behind\nstdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn failed_sync_drains_active_source_builds() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-sync-drain-installs";
    let wrapper_path = "/tmp/rpx-sync-drain-wrappers";
    let state_path = "/tmp/rpx-sync-drain-state";
    let temp_path = "/tmp/rpx-sync-drain-temp";
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
Suggests: digest",
    );

    let lock_command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &lock_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let wrapped_environment =
        install_sync_failure_wrappers(&container, project_path, wrapper_path, state_path);
    let sync_command = format!(
        "mkdir -p {temp_path} && cd {project_path} && {wrapped_environment} TMPDIR={temp_path} RPX_TEST_FAIL_DIGEST=1 rpx sync"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stderr.contains("code Some(97)") && stderr.contains("injected digest"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );

    let completed_root_command = format!(
        "test -f {state_path}/root-cleaned && test ! -e {state_path}/root-active && set -- {temp_path}/rpx-build-* && test ! -e \"$1\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &completed_root_command);
    assert_eq!(
        exit_code, 0,
        "sync returned before the root build cleaned up\nstdout was: {stdout}\nstderr was: {stderr}"
    );

    let retry_command =
        format!("cd {project_path} && {wrapped_environment} TMPDIR={temp_path} rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &retry_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert_package_state(&container, project_path, "digest", "TRUE");
    assert_package_state(&container, project_path, "testpkg", "TRUE");
}

#[test]
fn sync_without_project_installs_dependencies_and_removes_project() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-sync-without-project";
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

    let lock_command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &lock_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let before = read_project_file(&container, project_path, "rpx.lock");

    let sync_command = format!("cd {project_path} && rpx sync --no-install-project");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert_package_state(&container, project_path, "digest", "TRUE");
    assert_package_state(&container, project_path, "testpkg", "FALSE");

    let sync_command = format!("cd {project_path} && rpx sync");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert_package_state(&container, project_path, "testpkg", "TRUE");

    let sync_command = format!("cd {project_path} && rpx sync --no-install-project");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &sync_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert_package_state(&container, project_path, "digest", "TRUE");
    assert_package_state(&container, project_path, "testpkg", "FALSE");

    let after = read_project_file(&container, project_path, "rpx.lock");
    assert_eq!(after, before, "lockfile changed during sync");
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
    let add_command = format!("cd {project_path} && rpx add 'digest@==0.6.37'");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let remove_package_dir = format!(
        "cd {project_path} && rm -rf \"$(rpx run Rscript -e \"cat(file.path(.libPaths()[1], 'digest'))\")\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &remove_package_dir);
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
        stderr.contains("rpx::project::lockfile_outdated"),
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
        stderr.contains("rpx::project::lockfile_from_newer_rpx"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn runs_rpx_sync_with_reordered_lockfile_requirements() {
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
        r#"cd {project_path} && perl -0pi -e 's/"requirements": \[\s+"digest",\s+"jsonlite"\s+\]/"requirements": [
    "jsonlite",
    "digest"
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
