mod common;

use common::*;
use r_description::RDescription;
use serde_json::Value;

fn write_r_source(
    container: &testcontainers::core::Container<testcontainers::GenericImage>,
    project_path: &str,
    source: &str,
) {
    let command = format!(
        "mkdir -p {project_path}/R && cat > {project_path}/R/code.R <<'EOF'\n{source}\nEOF"
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

#[test]
fn reports_source_mismatches_in_non_interactive_mode() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-audit-report";
    create_package_project(&container, project_path);
    write_r_source(&container, project_path, "digest::digest('value')");

    let (exit_code, stdout, stderr) =
        run_shell_command(&container, &format!("cd {project_path} && rpx audit"));

    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(stdout.contains("Missing dependencies:"));
    assert!(stdout.contains("digest (qualified call at R/code.R:1:1)"));
    assert!(stderr.contains("rpx::audit::mismatches"));
}

#[test]
fn adds_and_prunes_without_installing_packages() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-audit-apply";
    create_package_project(&container, project_path);
    let (exit_code, stdout, stderr) = run_shell_command(
        &container,
        &format!("cat >> {project_path}/DESCRIPTION <<'EOF'\nImports: jsonlite\nEOF"),
    );
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    write_r_source(&container, project_path, "digest::digest('value')");

    let (exit_code, stdout, stderr) = run_shell_command(
        &container,
        &format!("cd {project_path} && rpx audit --add --prune"),
    );
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(stdout.contains("Added digest"));
    assert!(stdout.contains("Removed jsonlite"));

    let description =
        RDescription::parse(&read_project_file(&container, project_path, "DESCRIPTION"));
    let imports = description
        .imports()
        .expect("Imports should parse")
        .map(|relation| relation.package().to_string())
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 2);
    assert!(imports.iter().all(|package| package == "digest"));

    let lockfile =
        serde_json::from_str::<Value>(&read_project_file(&container, project_path, "rpx.lock"))
            .expect("lockfile should parse");
    assert!(lockfile["packages"].get("digest").is_some());
    assert!(lockfile["packages"].get("jsonlite").is_none());

    let (run_exit, _, run_stderr) = run_shell_command(
        &container,
        &format!("cd {project_path} && rpx run Rscript -e 'library(digest)'"),
    );
    assert_eq!(run_exit, 1, "stderr was: {run_stderr}");
}

#[test]
fn parser_diagnostics_disable_pruning() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-audit-invalid";
    create_package_project(&container, project_path);
    let (exit_code, stdout, stderr) = run_shell_command(
        &container,
        &format!("cat >> {project_path}/DESCRIPTION <<'EOF'\nImports: digest\nEOF"),
    );
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
    write_r_source(&container, project_path, "library(");

    let (exit_code, stdout, stderr) = run_shell_command(
        &container,
        &format!("cd {project_path} && rpx audit --prune"),
    );
    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(stdout.contains("Pruning skipped"));
    assert!(stderr.contains("rpx::audit::scan_incomplete"));
    assert!(stderr.contains("R-PARSE-"));
    assert!(read_project_file(&container, project_path, "DESCRIPTION").contains("digest"));
}

#[test]
fn rejects_packages_without_namespace() {
    let container = start_container();
    let project_path = "/tmp/rpx-project-audit-no-namespace";
    create_package_project(&container, project_path);
    let (exit_code, stdout, stderr) = run_shell_command(
        &container,
        &format!("rm {project_path}/NAMESPACE && cd {project_path} && rpx audit"),
    );

    assert_eq!(exit_code, 1, "stdout was: {stdout}\nstderr was: {stderr}");
    assert!(stderr.contains("rpx::audit::namespace_missing"));
}
