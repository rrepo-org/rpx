use crate::{
    cli::AuditArgs,
    description::{
        DependencyField, DescriptionParseError, add_dependencies, project_dependencies,
        remove_dependencies, root_package,
    },
    output::status,
    project::{
        Project, ProjectLoadError, ProjectWriteError, ResolutionPolicy, ResolveProjectError,
        load_project, pin_unconstrained_dependencies, resolve_project, write_project_files,
    },
};
use miette::{Diagnostic, NamedSource, SourceSpan};
use r_description::Relation;
use r_parser::{ParseStatus, ParserConfig, parse_source};
use r_syntax::{
    Argument, CallExpr, DocumentId, Expr, NodeOrToken, RowanAstNode, SyntaxKind, SyntaxNode,
    TextRange,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::IsTerminal,
    path::{Path, PathBuf},
};
use thiserror::Error;

const NAMESPACE_NAME: &str = "NAMESPACE";

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum Error {
    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectLoad(#[from] ProjectLoadError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    DescriptionParse(#[from] DescriptionParseError),

    #[error("NAMESPACE is required at {}", path.display())]
    #[diagnostic(
        code(rpx::audit::namespace_missing),
        help("Add a NAMESPACE file before auditing package dependencies.")
    )]
    NamespaceMissing { path: PathBuf },

    #[error("failed to read NAMESPACE at {}: {source}", path.display())]
    #[diagnostic(code(rpx::audit::namespace_read_failed))]
    NamespaceRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("a discovered package name is invalid: {package}: {reason}")]
    #[diagnostic(code(rpx::audit::invalid_package))]
    InvalidPackage { package: String, reason: String },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Resolve(#[from] ResolveProjectError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    ProjectWrite(#[from] ProjectWriteError),

    #[error("interactive prompt failed: {0}")]
    #[diagnostic(code(rpx::audit::prompt_failed))]
    InteractivePrompt(#[source] std::io::Error),

    #[error("dependency scan was incomplete")]
    #[diagnostic(
        code(rpx::audit::scan_incomplete),
        help("Fix the reported source issues and rerun `rpx audit`. Pruning was disabled.")
    )]
    IncompleteScan {
        #[related]
        issues: Vec<ScanIssue>,
    },

    #[error("dependency audit found unresolved mismatches")]
    #[diagnostic(
        code(rpx::audit::mismatches),
        help("Run `rpx audit --add --prune` to apply the reported changes.")
    )]
    Mismatches,
}

#[derive(Debug, Error, Diagnostic)]
pub(crate) enum ScanIssue {
    #[error("failed to inspect {}: {source}", path.display())]
    #[diagnostic(code(rpx::audit::source_read_failed))]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{}: {code}: {message}", path.display())]
    #[diagnostic(code(rpx::audit::parse_failed))]
    Parse {
        path: PathBuf,
        code: String,
        message: String,
        #[source_code]
        source_code: NamedSource<String>,
        #[label("here")]
        span: SourceSpan,
    },

    #[error("cannot scan {}: {reason}", path.display())]
    #[diagnostic(code(rpx::audit::source_unsupported))]
    Unsupported { path: PathBuf, reason: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvidenceKind {
    Library,
    Qualified,
    NamespaceImport,
}

impl std::fmt::Display for EvidenceKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Library => formatter.write_str("library"),
            Self::Qualified => formatter.write_str("qualified call"),
            Self::NamespaceImport => formatter.write_str("NAMESPACE import"),
        }
    }
}

#[derive(Debug)]
struct Evidence {
    path: PathBuf,
    line: usize,
    column: usize,
    kind: EvidenceKind,
}

#[derive(Default)]
struct AuditScan {
    packages: BTreeMap<String, Evidence>,
    issues: Vec<ScanIssue>,
}

struct AuditDiff {
    missing: BTreeSet<String>,
    unused: BTreeSet<String>,
}

pub(crate) async fn run(args: AuditArgs) -> Result<(), Error> {
    let mut project = load_project()?;
    let (project_name, _) = root_package(&project.root, &project.description)?;
    let declared_relations = project_dependencies(&project.root, &project.description)?;
    let declared = declared_relations
        .iter()
        .map(|relation| relation.package().to_string())
        .collect::<BTreeSet<_>>();
    let imports = project
        .description
        .imports()
        .map_err(|source| {
            DescriptionParseError::new(
                project.root.join("DESCRIPTION").display().to_string(),
                project.description.to_string(),
                vec![crate::description::DescriptionParseIssue::unpositioned(
                    source,
                )],
            )
        })?
        .map(|relation| relation.package().to_string())
        .collect::<BTreeSet<_>>();

    let scan = scan_project(&project.root)?;
    let used = scan
        .packages
        .keys()
        .filter(|package| package.as_str() != "base" && package.as_str() != project_name)
        .cloned()
        .collect::<BTreeSet<_>>();
    let diff = AuditDiff {
        missing: used.difference(&declared).cloned().collect(),
        unused: imports.difference(&used).cloned().collect(),
    };

    report_diff(&diff, &scan);
    for issue in &scan.issues {
        status(format_args!("Source issue: {issue}"));
    }

    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let add = !diff.missing.is_empty()
        && (args.add || (interactive && confirm("Add missing dependencies to Imports?")?));
    let prune = scan.issues.is_empty()
        && !diff.unused.is_empty()
        && (args.prune
            || (interactive && confirm("Remove unused dependencies from DESCRIPTION?")?));

    if args.prune && !scan.issues.is_empty() && !diff.unused.is_empty() {
        status("Pruning skipped because the dependency scan was incomplete");
    }

    let added_relations = if add {
        diff.missing
            .iter()
            .map(|package| {
                Relation::any(package).map_err(|source| Error::InvalidPackage {
                    package: package.clone(),
                    reason: source.to_string(),
                })
            })
            .collect::<Result<BTreeSet<_>, _>>()?
    } else {
        BTreeSet::new()
    };
    let removed_packages = if prune {
        diff.unused.clone()
    } else {
        BTreeSet::new()
    };

    apply_changes(&mut project, &added_relations, &removed_packages).await?;

    if !scan.issues.is_empty() {
        return Err(Error::IncompleteScan {
            issues: scan.issues,
        });
    }

    let missing_remain = !diff.missing.is_empty() && !add;
    let unused_remain = !diff.unused.is_empty() && !prune;
    if !interactive && (missing_remain || unused_remain) {
        return Err(Error::Mismatches);
    }

    if diff.missing.is_empty() && diff.unused.is_empty() {
        status("Dependencies are in sync");
    }
    Ok(())
}

async fn apply_changes(
    project: &mut Project,
    added_relations: &BTreeSet<Relation>,
    removed_packages: &BTreeSet<String>,
) -> Result<(), Error> {
    if added_relations.is_empty() && removed_packages.is_empty() {
        return Ok(());
    }

    add_dependencies(
        &project.root,
        &mut project.description,
        added_relations,
        DependencyField::Imports,
    )?;
    remove_dependencies(&project.root, &mut project.description, removed_packages)?;

    let mut resolution = resolve_project(project, ResolutionPolicy::ReuseIfValid).await?;
    pin_unconstrained_dependencies(
        project,
        &mut resolution,
        added_relations,
        DependencyField::Imports,
    )?;
    write_project_files(
        &project.root,
        Some(&project.description),
        &resolution.lockfile,
    )?;

    if !added_relations.is_empty() {
        status(format_args!(
            "Added {}",
            added_relations
                .iter()
                .map(Relation::package)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !removed_packages.is_empty() {
        status(format_args!(
            "Removed {}",
            removed_packages
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool, Error> {
    cliclack::confirm(prompt)
        .initial_value(false)
        .interact()
        .map_err(Error::InteractivePrompt)
}

fn report_diff(diff: &AuditDiff, scan: &AuditScan) {
    if !diff.missing.is_empty() {
        status("Missing dependencies:");
        for package in &diff.missing {
            if let Some(evidence) = scan.packages.get(package) {
                status(format_args!(
                    "  {package} ({} at {}:{}:{})",
                    evidence.kind,
                    evidence.path.display(),
                    evidence.line,
                    evidence.column
                ));
            }
        }
    }
    if !diff.unused.is_empty() {
        status(format_args!(
            "Unused dependencies: {}",
            diff.unused.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
}

fn scan_project(root: &Path) -> Result<AuditScan, Error> {
    let mut scan = AuditScan::default();
    let mut paths = Vec::new();
    collect_r_files(&root.join("R"), true, &mut paths, &mut scan.issues);
    paths.sort();

    let mut document = 0_u64;
    for path in paths {
        match fs::read_to_string(&path) {
            Ok(source) => {
                scan_source(root, &path, &source, document, false, &mut scan);
                document += 1;
            }
            Err(source) => scan.issues.push(ScanIssue::Io { path, source }),
        }
    }

    let namespace = root.join(NAMESPACE_NAME);
    let source = match fs::read_to_string(&namespace) {
        Ok(source) => source,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::NamespaceMissing { path: namespace });
        }
        Err(source) => {
            return Err(Error::NamespaceRead {
                path: namespace,
                source,
            });
        }
    };
    scan_source(root, &namespace, &source, document, true, &mut scan);
    Ok(scan)
}

fn collect_r_files(
    directory: &Path,
    root: bool,
    paths: &mut Vec<PathBuf>,
    issues: &mut Vec<ScanIssue>,
) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) if root && source.kind() == std::io::ErrorKind::NotFound => return,
        Err(source) => {
            issues.push(ScanIssue::Io {
                path: directory.to_path_buf(),
                source,
            });
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                issues.push(ScanIssue::Io {
                    path: directory.to_path_buf(),
                    source,
                });
                continue;
            }
        };
        let path = entry.path();
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => collect_r_files(&path, false, paths, issues),
            Ok(file_type)
                if file_type.is_file()
                    && path.extension().is_some_and(|extension| extension == "R") =>
            {
                paths.push(path);
            }
            Ok(file_type) if file_type.is_symlink() => issues.push(ScanIssue::Unsupported {
                path,
                reason: "symbolic links are not followed".to_string(),
            }),
            Ok(_) => {}
            Err(source) => issues.push(ScanIssue::Io { path, source }),
        }
    }
}

fn scan_source(
    root: &Path,
    path: &Path,
    source: &str,
    document: u64,
    namespace: bool,
    scan: &mut AuditScan,
) {
    let parsed = parse_source(
        source,
        &ParserConfig {
            document: DocumentId(document),
            ..ParserConfig::default()
        },
    );
    let source_name = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();

    for diagnostic in parsed.diagnostics() {
        scan.issues.push(parse_issue(
            &source_name,
            source,
            diagnostic.code.as_str(),
            &diagnostic.message,
            diagnostic.range,
        ));
    }
    if parsed.diagnostics_truncated() {
        scan.issues.push(parse_issue(
            &source_name,
            source,
            "R-PARSE-TRUNCATED",
            "parser diagnostics were truncated",
            TextRange::empty(0.into()),
        ));
    }
    if parsed.resource_limited() {
        scan.issues.push(parse_issue(
            &source_name,
            source,
            "R-PARSE-LIMIT",
            "parser resource limit was reached",
            TextRange::empty(0.into()),
        ));
    }
    if matches!(
        parsed.status(),
        ParseStatus::Incomplete | ParseStatus::Invalid
    ) && parsed.diagnostics().is_empty()
        && !parsed.resource_limited()
    {
        scan.issues.push(parse_issue(
            &source_name,
            source,
            "R-PARSE-STATUS",
            "parser did not produce a complete syntax tree",
            TextRange::empty(0.into()),
        ));
    }

    for node in parsed.root().descendants() {
        let evidence = if namespace && node.kind() == SyntaxKind::CALL_EXPR {
            CallExpr::cast(node).map_or_else(Vec::new, |call| namespace_import_packages(&call))
        } else if namespace && node.kind() == SyntaxKind::NAMESPACE_EXPR {
            qualified_package(&node).into_iter().collect()
        } else if !namespace && node.kind() == SyntaxKind::CALL_EXPR {
            CallExpr::cast(node)
                .and_then(|call| library_package(&call))
                .into_iter()
                .collect()
        } else if !namespace && node.kind() == SyntaxKind::NAMESPACE_EXPR {
            qualified_package(&node).into_iter().collect()
        } else {
            Vec::new()
        };
        for (package, range, kind) in evidence {
            let (line, column) = line_column(source, range.start().into());
            scan.packages.entry(package).or_insert_with(|| Evidence {
                path: PathBuf::from(&source_name),
                line,
                column,
                kind,
            });
        }
    }
}

fn parse_issue(
    source_name: &str,
    source: &str,
    code: &str,
    message: &str,
    range: TextRange,
) -> ScanIssue {
    let start = usize::from(range.start());
    let end = usize::from(range.end());
    ScanIssue::Parse {
        path: PathBuf::from(source_name),
        code: code.to_string(),
        message: message.to_string(),
        source_code: NamedSource::new(source_name, source.to_string()),
        span: (start, end.saturating_sub(start)).into(),
    }
}

fn library_package(call: &CallExpr) -> Option<(String, TextRange, EvidenceKind)> {
    if !is_library_call(call) {
        return None;
    }
    let arguments = call.arguments()?.arguments().collect::<Vec<_>>();
    let package = arguments
        .iter()
        .find(|argument| argument_tag(argument).as_deref() == Some("package"))
        .or_else(|| {
            arguments
                .iter()
                .find(|argument| argument_tag(argument).is_none())
        })?;
    let (name, range, quoted) = static_name(&argument_value(package)?)?;

    if !quoted
        && let Some(argument) = arguments
            .iter()
            .find(|argument| argument_tag(argument).as_deref() == Some("character.only"))
    {
        let value = argument_value(argument)?;
        if !is_static_false(&value) {
            return None;
        }
    }
    Some((name, range, EvidenceKind::Library))
}

fn is_library_call(call: &CallExpr) -> bool {
    if call_identifier(call).as_deref() == Some("library") {
        return true;
    }
    let Some(callee) = call.callee() else {
        return false;
    };
    if callee.syntax().kind() != SyntaxKind::NAMESPACE_EXPR
        || qualified_package(callee.syntax()).is_none_or(|(package, _, _)| package != "base")
    {
        return false;
    }
    callee
        .syntax()
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| token.kind() == SyntaxKind::IDENTIFIER && token.text() == "library")
}

fn namespace_import_packages(call: &CallExpr) -> Vec<(String, TextRange, EvidenceKind)> {
    let Some(callee) = call_identifier(call) else {
        return Vec::new();
    };
    let Some(arguments) = call.arguments() else {
        return Vec::new();
    };
    let arguments = arguments.arguments().collect::<Vec<_>>();
    let selected = match callee.as_str() {
        "import" => arguments
            .iter()
            .filter(|argument| argument_tag(argument).is_none())
            .collect::<Vec<_>>(),
        "importFrom" | "importClassesFrom" | "importMethodsFrom" => {
            arguments.first().into_iter().collect()
        }
        _ => return Vec::new(),
    };
    selected
        .into_iter()
        .filter_map(|argument| {
            let (name, range, _) = static_name(&argument_value(argument)?)?;
            Some((name, range, EvidenceKind::NamespaceImport))
        })
        .collect()
}

fn qualified_package(node: &SyntaxNode) -> Option<(String, TextRange, EvidenceKind)> {
    if !node.children_with_tokens().any(|element| {
        element.into_token().is_some_and(|token| {
            matches!(
                token.kind(),
                SyntaxKind::NS_GET | SyntaxKind::NS_GET_INTERNAL
            )
        })
    }) {
        return None;
    }
    let lhs = node.children().find_map(Expr::cast)?;
    let (name, range, _) = static_name(&lhs)?;
    Some((name, range, EvidenceKind::Qualified))
}

fn call_identifier(call: &CallExpr) -> Option<String> {
    let callee = call.callee()?;
    if callee.syntax().kind() != SyntaxKind::IDENTIFIER_EXPR {
        return None;
    }
    callee
        .syntax()
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| token.kind() == SyntaxKind::IDENTIFIER)
        .map(|token| token.text().to_string())
}

fn argument_tag(argument: &Argument) -> Option<String> {
    let mut tokens = argument
        .syntax()
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|token| !token.kind().is_trivia());
    let tag = tokens.next()?;
    if tokens.any(|token| token.kind() == SyntaxKind::EQ) {
        Some(tag.text().to_string())
    } else {
        None
    }
}

fn argument_value(argument: &Argument) -> Option<Expr> {
    argument.syntax().children().find_map(Expr::cast)
}

fn static_name(expression: &Expr) -> Option<(String, TextRange, bool)> {
    let token = expression
        .syntax()
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|token| !token.kind().is_trivia())?;
    match (expression.syntax().kind(), token.kind()) {
        (SyntaxKind::IDENTIFIER_EXPR, SyntaxKind::IDENTIFIER) => {
            let name = token.text();
            (!name.starts_with('`')).then(|| (name.to_string(), token.text_range(), false))
        }
        (SyntaxKind::LITERAL_EXPR, SyntaxKind::STRING) => {
            let text = token.text();
            let quote = text.chars().next()?;
            if text.len() < 2
                || !matches!(quote, '\'' | '"')
                || !text.ends_with(quote)
                || text[1..text.len() - 1].contains('\\')
            {
                return None;
            }
            Some((
                text[1..text.len() - 1].to_string(),
                token.text_range(),
                true,
            ))
        }
        _ => None,
    }
}

fn is_static_false(expression: &Expr) -> bool {
    expression
        .syntax()
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .any(|token| token.kind() == SyntaxKind::FALSE_KW)
}

fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let prefix = &source[..offset.min(source.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.len(), |(_, current)| current.len())
        + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(source: &str, namespace: bool) -> AuditScan {
        let mut scan = AuditScan::default();
        scan_source(
            Path::new("/project"),
            Path::new(if namespace {
                "/project/NAMESPACE"
            } else {
                "/project/R/code.R"
            }),
            source,
            0,
            namespace,
            &mut scan,
        );
        assert!(scan.issues.is_empty());
        scan
    }

    #[test]
    fn discovers_library_and_qualified_usage() {
        let scan = scan(
            "library(dplyr)\nlibrary(package = 'tidyr')\nlibrary(pkgfalse, character.only = FALSE)\nbase::library(basequalified)\njsonlite::toJSON(x)\nrlang:::foo()\n",
            false,
        );
        assert_eq!(
            scan.packages.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "base".to_string(),
                "dplyr".to_string(),
                "basequalified".to_string(),
                "jsonlite".to_string(),
                "pkgfalse".to_string(),
                "rlang".to_string(),
                "tidyr".to_string(),
            ])
        );
    }

    #[test]
    fn ignores_dynamic_and_non_code_usage() {
        let scan = scan(
            "# library(commented)\nx <- 'string::value'\nlibrary(package_name, character.only = TRUE)\nlibrary(\"escaped\\x6eame\")\n",
            false,
        );
        assert!(scan.packages.is_empty());
    }

    #[test]
    fn discovers_namespace_imports() {
        let scan = scan(
            "import(dplyr, tidyr)\nimportFrom(jsonlite, toJSON)\nimportClassesFrom(methods, class)\nimportMethodsFrom(rlang, method)\nS3method(pillar::pillar_shaft, fixture)\n",
            true,
        );
        assert_eq!(
            scan.packages.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "dplyr".to_string(),
                "jsonlite".to_string(),
                "methods".to_string(),
                "pillar".to_string(),
                "rlang".to_string(),
                "tidyr".to_string(),
            ])
        );
    }

    #[test]
    fn surfaces_parser_diagnostics() {
        let mut scan = AuditScan::default();
        scan_source(
            Path::new("/project"),
            Path::new("/project/R/code.R"),
            "library(",
            0,
            false,
            &mut scan,
        );
        assert!(!scan.issues.is_empty());
    }

    #[test]
    fn requires_namespace() {
        let project = tempfile::tempdir().expect("project directory should be created");
        assert!(matches!(
            scan_project(project.path()),
            Err(Error::NamespaceMissing { .. })
        ));
    }

    #[test]
    fn scans_nested_r_files_in_deterministic_order() {
        let project = tempfile::tempdir().expect("project directory should be created");
        fs::create_dir_all(project.path().join("R/nested")).expect("R directory should be created");
        fs::write(project.path().join("R/z.R"), "zpkg::z()\n").expect("R source should be written");
        fs::write(project.path().join("R/nested/a.R"), "apkg::a()\n")
            .expect("nested R source should be written");
        fs::write(project.path().join(NAMESPACE_NAME), "export(a)\n")
            .expect("NAMESPACE should be written");

        let scan = scan_project(project.path()).expect("project should scan");
        assert!(scan.issues.is_empty());
        assert_eq!(
            scan.packages.keys().cloned().collect::<Vec<_>>(),
            ["apkg", "zpkg"]
        );
        assert_eq!(scan.packages["apkg"].path, PathBuf::from("R/nested/a.R"));
    }
}
