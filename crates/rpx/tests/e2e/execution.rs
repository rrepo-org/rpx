use super::support::{Fixture, diagnostic};
use std::fs;

#[test]
fn discovers_project_from_nested_directory_without_changing_cwd() {
    let f = Fixture::new();
    f.locked_project();
    let nested = f.project.join("scripts/path with spaces");
    fs::create_dir_all(&nested).unwrap();
    let output = f.success(
        &nested,
        &["run", "Rscript", "--vanilla", "-e", "cat(getwd())"],
    );
    let actual = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap());
    assert_eq!(
        actual.canonicalize().unwrap(),
        nested.canonicalize().unwrap()
    );
    f.r_assert(
        &nested,
        "stopifnot(normalizePath(.libPaths()[1L]) == normalizePath(Sys.getenv('R_LIBS_USER')))",
    );
    f.close();
}

#[test]
fn arguments_are_literal_including_spaces_empty_strings_and_metacharacters() {
    let f = Fixture::new();
    f.locked_project();
    fs::write(f.project.join("arguments.R"), r#"
        stopifnot(identical(commandArgs(trailingOnly=TRUE), c("two words", "*", "$HOME", "", "-n", "a\"b", "semi;colon")))
    "#).unwrap();
    f.success(
        &f.project,
        &[
            "run",
            "Rscript",
            "--vanilla",
            "arguments.R",
            "two words",
            "*",
            "$HOME",
            "",
            "-n",
            "a\"b",
            "semi;colon",
        ],
    );
    f.close();
}

#[test]
fn forwards_standard_streams_and_child_exit_code() {
    let f = Fixture::new();
    f.locked_project();
    let output = f.rpx(
        &f.project,
        &[
            "run",
            "Rscript",
            "--vanilla",
            "-e",
            "cat('child-out'); cat('child-error',file=stderr()); quit(status=42)",
        ],
    );
    assert_eq!(output.status.code(), Some(42), "{}", diagnostic(&output));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "child-out");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "child-error");
    f.close();
}

#[test]
fn missing_executable_has_an_actionable_diagnostic() {
    let f = Fixture::new();
    f.locked_project();
    f.failure(
        &f.project,
        &["run", "rpx-fixture-command-that-does-not-exist"],
        "rpx::run::command_not_found",
    );
    f.close();
}

#[test]
fn project_r_profile_does_not_interfere_with_environment_validation() {
    let f = Fixture::new();
    f.locked_project();
    fs::write(
        f.project.join(".Rprofile"),
        "stop('project profile must not run during validation')\n",
    )
    .unwrap();
    f.success(
        &f.project,
        &["run", "Rscript", "--vanilla", "-e", "stopifnot(TRUE)"],
    );
    f.close();
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStringExt, fs::symlink, process::ExitStatusExt},
        process::Stdio,
    };

    #[test]
    fn preserves_non_utf8_arguments() {
        let f = Fixture::new();
        f.locked_project();
        let output = f
            .rpx_command(&f.project)
            .args(["run", "sh", "-c", "printf '%s' \"$1\"", "marker"])
            .arg(OsString::from_vec(vec![0xff, 0xfe]))
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", diagnostic(&output));
        assert_eq!(output.stdout, [0xff, 0xfe]);
        f.close();
    }

    #[test]
    fn preserves_signal_termination() {
        let f = Fixture::new();
        f.locked_project();
        let output = f.rpx(&f.project, &["run", "sh", "-c", "kill -TERM $$"]);
        assert_eq!(output.status.signal(), Some(15), "{}", diagnostic(&output));
        f.close();
    }

    #[test]
    fn replaces_process_instead_of_leaving_a_wrapper() {
        let f = Fixture::new();
        f.locked_project();
        let child = f
            .rpx_command(&f.project)
            .args(["run", "sh", "-c", "echo $$"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", diagnostic(&output));
        assert_eq!(
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap(),
            pid
        );
        f.close();
    }

    #[test]
    fn recursive_shebangs_are_stopped() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        f.locked_project();
        // Use a short interpreter path rather than embedding the long checkout/build path.
        let executable = f.rpx_command(&f.project).get_program().to_owned();
        let interpreter = f.root.join("rpx");
        symlink(executable, &interpreter).unwrap();
        let script = f.project.join("recursive");
        fs::write(&script, format!("#!{} run\n", interpreter.display())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        f.failure(
            &f.project,
            &["run", "./recursive"],
            "rpx::run::recursion_limit",
        );
        f.close();
    }
}
