mod common;

use common::*;

#[test]
fn runs_rpx_status_for_clean_project() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-status-clean";
    create_package_project(&container, project_path);
    let setup_command = format!("mkdir -p {project_path} && cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &setup_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let status_command = format!("cd {project_path} && rpx status");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &status_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Project is in sync"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn runs_rpx_status_for_lockfile_drift() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-status-drift";
    create_package_project(&container, project_path);
    let add_dependency =
        format!("cd {project_path} && cat >> DESCRIPTION <<'EOF'\nImports: digest\nEOF");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &add_dependency);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let seed_lockfile = format!(
        "mkdir -p {project_path} && cd {project_path} && cat > rpx.lock <<'EOF'\n{{\n  \"version\": 4,\n  \"revision\": 0,\n  \"repositories\": [\n    {{\n      \"url\": \"https://api.rrepo.org\",\n      \"kind\": \"rrepo\"\n    }}\n  ],\n  \"roots\": [],\n  \"packages\": {{}}\n}}\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &seed_lockfile);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let status_command = format!("cd {project_path} && rpx status");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &status_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Project is out of sync"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("Run: rpx lock && rpx sync"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("Packages in DESCRIPTION but not locked:"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("- digest"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn reports_old_lockfile_needs_update() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-status-old-lockfile";
    create_package_project(&container, project_path);
    let seed_lockfile = format!(
        "mkdir -p {project_path} && cd {project_path} && cat > rpx.lock <<'EOF'\n{{\n  \"version\": 3,\n  \"revision\": 1,\n  \"registry\": \"https://api.rrepo.org\",\n  \"roots\": [],\n  \"packages\": {{}}\n}}\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &seed_lockfile);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let status_command = format!("cd {project_path} && rpx status");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &status_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stderr.contains("lockfile is out of date"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stderr.contains("rpx::lockfile::older"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stderr.contains("Run `rpx lock`"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn reports_newer_lockfile_is_incompatible() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-status-newer-lockfile";
    create_package_project(&container, project_path);
    let seed_lockfile = format!(
        "mkdir -p {project_path} && cd {project_path} && cat > rpx.lock <<'EOF'\n{{\n  \"version\": 999,\n  \"registry\": \"https://api.rrepo.org\",\n  \"roots\": [],\n  \"packages\": {{}}\n}}\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &seed_lockfile);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let status_command = format!("cd {project_path} && rpx status");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &status_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stderr.contains("lockfile is incompatible"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stderr.contains("rpx::lockfile::newer"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stderr.contains("Upgrade rpx or regenerate the lockfile with this version."),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn runs_rpx_status_ignores_additional_repositories() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-status-repo-drift";
    create_package_project(&container, project_path);

    let setup_command = format!("cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &setup_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let mutate_command = format!(
        "cd {project_path} && cat >> DESCRIPTION <<'EOF'\nAdditional_repositories: https://packagemanager.posit.co/cran/latest\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &mutate_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let status_command = format!("cd {project_path} && rpx status");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &status_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Project is in sync"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn runs_rpx_status_for_missing_library_package() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-status-missing-library";
    create_package_project(&container, project_path);

    let setup_command = format!("cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &setup_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let remove_package_dir = format!(
        "cd {project_path} && rm -rf \"$(rpx run Rscript -e \"cat(file.path(.libPaths()[1], 'digest'))\")\" \"$(rpx run Rscript -e \"cat(file.path(.libPaths()[1], 'testpkg'))\")\""
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &remove_package_dir);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let status_command = format!("cd {project_path} && rpx status");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &status_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Project library is out of sync"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("Run: rpx sync"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("Required packages not installed:"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("- digest"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("- testpkg"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn runs_rpx_status_for_extra_library_package() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-status-extra-library";
    create_package_project(&container, project_path);

    let setup_command = format!("cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &setup_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let extra_command =
        format!("cd {project_path} && rpx run Rscript -e \"install.packages('jsonlite')\"");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &extra_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let status_command = format!("cd {project_path} && rpx status");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &status_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Project library is out of sync"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("Unexpected packages installed:"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("- jsonlite"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn runs_rpx_status_for_version_mismatch() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-status-version-mismatch";
    create_package_project(&container, project_path);

    let setup_command = format!("cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &setup_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let mutate_lockfile = format!(
        "cd {project_path} && perl -0pi -e 's/(\"digest\": \\{{\\s+\"package\": \"digest\",\\s+\"version\": )\"[0-9.]+\"/${{1}}\"0.0.1\"/' rpx.lock"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &mutate_lockfile);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    let mutate_description =
        format!("cd {project_path} && perl -0pi -e 's/Version: 0.1.0/Version: 0.2.0/' DESCRIPTION");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &mutate_description);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let status_command = format!("cd {project_path} && rpx status");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &status_command);
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("Project library is out of sync"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("Installed versions that differ from expected versions:"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("- digest ("),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("0.0.1 locked"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("- testpkg (0.1.0 installed, 0.2.0 expected)"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}

#[test]
fn reports_r_runtime_version_mismatch_without_failing_status() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-status-r-version-mismatch";
    create_package_project(&container, project_path);

    let setup_command = format!("cd {project_path} && rpx add digest");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &setup_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let mutate_lockfile = format!(
        "cd {project_path} && perl -0pi -e 's/(\"r\": \\{{\\s+\"version\": )\"[0-9.]+\"/${{1}}\"0.0.1\"/' rpx.lock"
    );
    let (exit_code, stdout, stderr) = run_shell_command(&container, &mutate_lockfile);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");

    let status_command = format!("cd {project_path} && rpx status");
    let (exit_code, stdout, stderr) = run_shell_command(&container, &status_command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(
        stdout.contains("R runtime differs from lockfile:"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("R 0.0.1 locked"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
    assert!(
        stdout.contains("Project is in sync"),
        "stdout was: {stdout}\nstderr was: {stderr}"
    );
}
