use std::path::PathBuf;

use testcontainers::{
    GenericBuildableImage, ImageExt,
    core::Container,
    core::ExecCommand,
    runners::{SyncBuilder, SyncRunner},
};

pub fn rpx_test_image() -> testcontainers::GenericImage {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    GenericBuildableImage::new("rpx-test", "latest")
        .with_dockerfile(root.join("tests/docker/rpx.Dockerfile"))
        .with_file(root.join("Cargo.toml"), "./Cargo.toml")
        .with_file(root.join("Cargo.lock"), "./Cargo.lock")
        .with_file(root.join("assets"), "./assets")
        .with_file(root.join("src"), "./src")
        .build_image()
        .expect("image should build")
}

pub fn start_container() -> Container<testcontainers::GenericImage> {
    rpx_test_image()
        .with_cmd(vec!["sleep", "infinity"])
        .start()
        .expect("container should start")
}

pub fn run_shell_command(
    container: &Container<testcontainers::GenericImage>,
    command: &str,
) -> (i64, String, String) {
    run_command(container, &["sh", "-lc", command])
}

pub fn run_command(
    container: &Container<testcontainers::GenericImage>,
    command: &[&str],
) -> (i64, String, String) {
    let mut result = container
        .exec(ExecCommand::new(command.iter().copied()))
        .expect("command should run");

    let stdout = String::from_utf8(result.stdout_to_vec().expect("should read stdout"))
        .expect("stdout should be utf-8");
    let stderr = String::from_utf8(result.stderr_to_vec().expect("should read stderr"))
        .expect("stderr should be utf-8");

    let exit_code = result
        .exit_code()
        .expect("should read exit code")
        .expect("command should have exited");

    (exit_code, stdout, stderr)
}

pub fn create_package_project(
    container: &testcontainers::core::Container<testcontainers::GenericImage>,
    project_path: &str,
) {
    let command = format!(
        "mkdir -p {project_path} && touch {project_path}/NAMESPACE && cat > {project_path}/DESCRIPTION <<'EOF'\nPackage: testpkg\nVersion: 0.1.0\nTitle: Test Package\nDescription: Test package for rpx integration tests.\nLicense: MIT\nAuthor: Test Author\nMaintainer: Test Author <test@example.com>\nEOF"
    );
    let (exit_code, stdout, stderr) = run_shell_command(container, &command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
}

#[allow(dead_code)]
pub fn lock_package_project(
    container: &testcontainers::core::Container<testcontainers::GenericImage>,
    project_path: &str,
) {
    let command = format!("cd {project_path} && rpx lock");
    let (exit_code, stdout, stderr) = run_shell_command(container, &command);
    assert_eq!(exit_code, 0, "stdout was: {stdout}\nstderr was: {stderr}");
}
