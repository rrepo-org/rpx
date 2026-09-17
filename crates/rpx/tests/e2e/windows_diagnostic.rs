use std::{fs, process::Command};

use super::support::Fixture;

// Temporary diagnostic, run explicitly by the Windows diagnostic CI job only.
#[test]
#[ignore = "Windows Rscript crash investigation"]
fn compare_direct_and_wrapped_rscript() {
    let f = Fixture::new();
    f.success(&f.project, &["init"]);
    let library = f.library();
    let full = r#"
        normalized <- function(path) normalizePath(path, winslash = "/", mustWork = TRUE)
        within <- function(path, root) startsWith(normalized(path), paste0(normalized(root), "/"))
        metadata <- read.dcf("DESCRIPTION")
        stopifnot(metadata[1, "Package"] == "sample.package")
        stopifnot(metadata[1, "Title"] == "Sample Package")
        stopifnot(metadata[1, "License"] == "MIT + file LICENSE")
        library_path <- .libPaths()[1L]
        stopifnot(within(library_path, Sys.getenv("RPX_DATA_DIR")))
        stopifnot(within(tempdir(), Sys.getenv("TMPDIR")))
        library("sample.package", lib.loc = library_path)
        stopifnot(within(find.package("sample.package"), library_path))
    "#;
    let cases = [
        ("plain", "cat('ok')".to_owned()),
        ("leading-newline", "\ncat('ok')".to_owned()),
        ("trailing-newline", "cat('ok')\n".to_owned()),
        (
            "two-statements-lf",
            "cat('first')\ncat('second')".to_owned(),
        ),
        (
            "two-statements-crlf",
            "cat('first')\r\ncat('second')".to_owned(),
        ),
        ("blank-first-line", "\n\ncat('ok')".to_owned()),
        (
            "read-description",
            "print(read.dcf('DESCRIPTION'))".to_owned(),
        ),
        (
            "read-description-multiline",
            "\nd <- read.dcf('DESCRIPTION')\nprint(d)\n".to_owned(),
        ),
        (
            "load-package",
            "library('sample.package',lib.loc=.libPaths()[1L]); cat('loaded')".to_owned(),
        ),
        ("full", full.to_owned()),
        (
            "full-one-line",
            full.lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("; "),
        ),
    ];
    let mut results = Vec::new();
    let mut execute = |case: &str, route: &str, input: &str, mut command: Command| {
        let program = command.get_program().to_string_lossy().into_owned();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let env = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let result = match command.output() {
            Ok(output) => serde_json::json!({
                "case": case, "route": route, "input": input,
                "program": program, "args": args, "env": env,
                "success": output.status.success(), "code": output.status.code(),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
            }),
            Err(error) => serde_json::json!({
                "case": case, "route": route, "input": input,
                "program": program, "args": args, "env": env,
                "success": false, "spawn_error": error.to_string(),
            }),
        };
        println!("DIAGNOSTIC {}", serde_json::to_string(&result).unwrap());
        results.push(result);
    };
    let mut version = f.command("Rscript", &f.project);
    version.arg("--version");
    execute("version", "direct", "version", version);
    for (name, expression) in cases {
        let script = f.root.join(format!("{name}.R"));
        fs::write(&script, &expression).unwrap();
        for file_input in [false, true] {
            for route in ["direct", "direct-with-library", "wrapped"] {
                let mut command = if route == "wrapped" {
                    let mut command = f.rpx_command(&f.project);
                    command.args(["run", "Rscript"]);
                    command
                } else {
                    f.command("Rscript", &f.project)
                };
                if route == "direct-with-library" {
                    command.env("R_LIBS_USER", &library);
                }
                command.arg("--vanilla");
                if file_input {
                    command.arg(&script);
                } else {
                    command.arg("-e").arg(&expression);
                }
                execute(
                    &name,
                    route,
                    if file_input { "file" } else { "expression" },
                    command,
                );
            }
        }
    }
    let report = std::env::var_os("RPX_DIAGNOSTIC_REPORT")
        .expect("diagnostic report path should be configured");
    fs::write(report, serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    f.close();
    // Fail only after capturing every outcome, ensuring nextest displays all diagnostics.
    assert!(
        results.iter().all(|result| result["success"] == true),
        "Rscript diagnostic found failures; inspect diagnostic-results.json"
    );
}
