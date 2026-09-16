//! Presentation of PubGrub proofs. Reductions only affect the failure report,
//! never the constraints used by the solver.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
    sync::Arc,
};

use pubgrub::{
    DefaultStringReporter, DerivationTree, Derived, External, Map, Ranges, ReportFormatter,
    Reporter, Term,
};

use super::PackageVersion;

type Range = Ranges<PackageVersion>;
type Tree = DerivationTree<String, Range, String>;
type Cause = External<String, Range, String>;
type Conclusion = Derived<String, Range, String>;
type Terms = Map<String, Term<Range>>;
type Versions = BTreeMap<String, BTreeSet<PackageVersion>>;

#[derive(Debug, thiserror::Error)]
#[error("{explanation}")]
pub(crate) struct ResolutionReport {
    pub(crate) explanation: String,
    pub(crate) help: &'static str,
}

pub(crate) fn render(mut tree: Tree, root: Option<String>, versions: Versions) -> ResolutionReport {
    let metadata_failure = has_metadata_failure(&tree);
    tree = collapse_unavailable(tree);
    tree.collapse_no_versions();
    let formatter = RpxReportFormatter { root, versions };
    tree = simplify(tree, &formatter);
    ResolutionReport {
        explanation: DefaultStringReporter::report_with_formatter(&tree, &formatter),
        help: if metadata_failure {
            "Dependency metadata could not be parsed. Check the affected package's DESCRIPTION or use a version/repository with valid metadata."
        } else {
            "Check the requested versions in DESCRIPTION and the packages available in your configured repositories."
        },
    }
}

fn has_metadata_failure(tree: &Tree) -> bool {
    match tree {
        Tree::External(External::Custom(_, _, reason)) => {
            reason.starts_with("invalid dependency metadata:")
        }
        Tree::Derived(derived) => {
            has_metadata_failure(&derived.cause1) || has_metadata_failure(&derived.cause2)
        }
        Tree::External(_) => false,
    }
}

fn positive_package(terms: &Terms, package: &str) -> bool {
    terms.len() == 1 && matches!(terms.get(package), Some(Term::Positive(_)))
}

/// Like uv's unavailable-version reduction, combine equal reasons in sibling
/// and nested causes. Keep distinct reasons and the surrounding proof intact.
fn collapse_unavailable(tree: Tree) -> Tree {
    let Tree::Derived(mut derived) = tree else {
        return tree;
    };
    derived.cause1 = Arc::new(collapse_unavailable(Arc::unwrap_or_clone(derived.cause1)));
    derived.cause2 = Arc::new(collapse_unavailable(Arc::unwrap_or_clone(derived.cause2)));

    for (external, other) in [
        (&derived.cause1, &derived.cause2),
        (&derived.cause2, &derived.cause1),
    ] {
        let Tree::External(External::Custom(package, range, reason)) = external.as_ref() else {
            continue;
        };
        if !positive_package(&derived.terms, package) {
            continue;
        }
        match other.as_ref() {
            Tree::External(External::Custom(other_package, other_range, other_reason))
                if package == other_package && reason == other_reason =>
            {
                return Tree::External(External::Custom(
                    package.clone(),
                    range.union(other_range),
                    reason.clone(),
                ));
            }
            Tree::Derived(inner) if positive_package(&inner.terms, package) => {
                for (candidate, remaining) in [
                    (&inner.cause1, &inner.cause2),
                    (&inner.cause2, &inner.cause1),
                ] {
                    if let Tree::External(External::Custom(
                        other_package,
                        other_range,
                        other_reason,
                    )) = candidate.as_ref()
                        && package == other_package
                        && reason == other_reason
                    {
                        return Tree::Derived(Derived {
                            terms: derived.terms.clone(),
                            shared_id: derived.shared_id,
                            cause1: Arc::clone(remaining),
                            cause2: Arc::new(Tree::External(External::Custom(
                                package.clone(),
                                range.union(other_range),
                                reason.clone(),
                            ))),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    Tree::Derived(derived)
}

fn simplify(tree: Tree, formatter: &RpxReportFormatter) -> Tree {
    match tree {
        Tree::External(External::Custom(package, range, reason)) => {
            let range = formatter.simplify_range(&package, &range);
            Tree::External(External::Custom(package, range, reason))
        }
        Tree::External(External::FromDependencyOf(package, range, dependency, requested)) => {
            let range = formatter.simplify_range(&package, &range);
            // Keep the requested constraint as written, rather than turning it
            // into a claim about which versions the repository happens to have.
            Tree::External(External::FromDependencyOf(
                package, range, dependency, requested,
            ))
        }
        Tree::Derived(mut derived) => {
            derived.cause1 = Arc::new(simplify(Arc::unwrap_or_clone(derived.cause1), formatter));
            derived.cause2 = Arc::new(simplify(Arc::unwrap_or_clone(derived.cause2), formatter));
            for (package, term) in &mut derived.terms {
                if let Term::Positive(range) = term {
                    *range = formatter.simplify_range(package, range);
                }
            }
            for (availability, other) in [
                (&derived.cause1, &derived.cause2),
                (&derived.cause2, &derived.cause1),
            ] {
                if let Tree::External(External::NoVersions(package, _)) = availability.as_ref()
                    && positive_package(&derived.terms, package)
                    && let Tree::External(External::Custom(other_package, range, _)) =
                        other.as_ref()
                    && package == other_package
                    && *range == Range::full()
                    && formatter
                        .versions
                        .get(package)
                        .is_some_and(|versions| !versions.is_empty())
                {
                    // The rejection already covers every listed version. The
                    // additional statement about unlisted versions is redundant.
                    return other.as_ref().clone();
                }
            }
            Tree::Derived(derived)
        }
        tree => tree,
    }
}

struct RpxReportFormatter {
    root: Option<String>,
    versions: Versions,
}

impl RpxReportFormatter {
    fn simplify_range(&self, package: &str, range: &Range) -> Range {
        let Some(versions) = self
            .versions
            .get(package)
            .filter(|versions| !versions.is_empty())
        else {
            return range.clone();
        };
        if versions.len() == 1 && range.contains(versions.first().unwrap()) {
            return Range::singleton(versions.first().unwrap().clone());
        }
        range.simplify(versions.iter())
    }

    fn is_root(&self, package: &str) -> bool {
        self.root.as_deref() == Some(package)
    }

    fn package_range(&self, package: &str, range: &Range) -> String {
        if self.is_root(package) {
            return "your project".into();
        }
        if *range == Range::full() {
            if self
                .versions
                .get(package)
                .is_some_and(|versions| !versions.is_empty())
            {
                return format!("all available versions of {package}");
            }
            return format!("all versions of {package}");
        }
        format!("{package} {}", format_range(range))
    }

    fn requirement(&self, package: &str, range: &Range) -> String {
        if *range == Range::full() {
            package.to_owned()
        } else {
            format!("{package} {}", format_range(range))
        }
    }
}

fn format_range(range: &Range) -> String {
    if *range == Range::empty() {
        return "(no versions)".into();
    }
    if *range == Range::full() {
        return "(any version)".into();
    }
    if let Some(version) = range.as_singleton() {
        return format!("(== {version})");
    }
    range
        .iter()
        .map(|(lower, upper)| {
            let mut bounds = Vec::new();
            match lower {
                Bound::Included(version) => bounds.push(format!(">= {version}")),
                Bound::Excluded(version) => bounds.push(format!("> {version}")),
                Bound::Unbounded => {}
            }
            match upper {
                Bound::Included(version) => bounds.push(format!("<= {version}")),
                Bound::Excluded(version) => bounds.push(format!("< {version}")),
                Bound::Unbounded => {}
            }
            format!("({})", bounds.join(", "))
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

impl ReportFormatter<String, Range, String> for RpxReportFormatter {
    type Output = String;

    fn format_external(&self, external: &Cause) -> String {
        match external {
            External::NotRoot(package, version) => format!("we are resolving {package} {version}"),
            External::Custom(package, range, reason) => format!(
                "{} cannot be used because of {reason}",
                self.package_range(package, range)
            ),
            External::FromDependencyOf(package, range, dependency, requested) => {
                let subject = self.package_range(package, range);
                let requires = if self.is_root(package) || range.as_singleton().is_some() {
                    "requires"
                } else {
                    "require"
                };
                format!(
                    "{subject} {requires} {}",
                    self.requirement(dependency, requested)
                )
            }
            External::NoVersions(package, range) => {
                if *range == Range::full() {
                    return format!("{package} is not available in the configured repositories");
                }
                // Without a complete listing, a complement is only an abstract
                // set of possible versions, not evidence of available versions.
                if let Some(versions) = self.versions.get(package) {
                    if versions.is_empty() {
                        return format!(
                            "{package} is not available in the configured repositories"
                        );
                    }
                    if versions.iter().all(|version| !range.contains(version)) {
                        let available = versions.iter().fold(Range::empty(), |set, version| {
                            set.union(&Range::singleton(version.clone()))
                        });
                        return format!(
                            "the available versions of {package} are {}",
                            format_range(&available)
                        );
                    }
                }
                format!(
                    "no available version of {} satisfies {}",
                    package,
                    format_range(range)
                )
            }
        }
    }

    fn format_terms(&self, terms: &Terms) -> String {
        let mut terms = terms.iter().collect::<Vec<_>>();
        terms.sort_by_key(|(package, _)| *package);
        match terms.as_slice() {
            [] => "the requirements cannot be satisfied".into(),
            [(package, _)] if self.is_root(package) => {
                "your project's requirements cannot be satisfied".into()
            }
            [(package, Term::Positive(range))] => {
                format!("{} cannot be used", self.package_range(package, range))
            }
            [(package, Term::Negative(range))] => {
                format!("{} must be used", self.requirement(package, range))
            }
            [
                (package, Term::Positive(range)),
                (dependency, Term::Negative(requested)),
            ]
            | [
                (dependency, Term::Negative(requested)),
                (package, Term::Positive(range)),
            ] => self.format_external(&External::FromDependencyOf(
                (*package).clone(),
                range.clone(),
                (*dependency).clone(),
                requested.clone(),
            )),
            terms => {
                let terms = terms
                    .iter()
                    .map(|(package, term)| match term {
                        Term::Positive(range) => self.package_range(package, range),
                        Term::Negative(range) => {
                            format!("excluding {}", self.requirement(package, range))
                        }
                    })
                    .collect::<Vec<_>>();
                format!("{} are incompatible", terms.join(" and "))
            }
        }
    }

    fn explain_both_external(&self, first: &Cause, second: &Cause, terms: &Terms) -> String {
        format!(
            "Because {} and {}, {}.",
            self.format_external(first),
            self.format_external(second),
            self.format_terms(terms)
        )
    }

    fn explain_both_ref(
        &self,
        first_id: usize,
        first: &Conclusion,
        second_id: usize,
        second: &Conclusion,
        terms: &Terms,
    ) -> String {
        format!(
            "Because {} ({first_id}) and {} ({second_id}), {}.",
            self.format_terms(&first.terms),
            self.format_terms(&second.terms),
            self.format_terms(terms)
        )
    }

    fn explain_ref_and_external(
        &self,
        id: usize,
        derived: &Conclusion,
        external: &Cause,
        terms: &Terms,
    ) -> String {
        format!(
            "Because {} ({id}) and {}, {}.",
            self.format_terms(&derived.terms),
            self.format_external(external),
            self.format_terms(terms)
        )
    }

    fn and_explain_external(&self, external: &Cause, terms: &Terms) -> String {
        format!(
            "And because {}, {}.",
            self.format_external(external),
            self.format_terms(terms)
        )
    }

    fn and_explain_ref(&self, id: usize, derived: &Conclusion, terms: &Terms) -> String {
        format!(
            "And because {} ({id}), {}.",
            self.format_terms(&derived.terms),
            self.format_terms(terms)
        )
    }

    fn and_explain_prior_and_external(
        &self,
        prior: &Cause,
        external: &Cause,
        terms: &Terms,
    ) -> String {
        format!(
            "And because {} and {}, {}.",
            self.format_external(prior),
            self.format_external(external),
            self.format_terms(terms)
        )
    }
}
