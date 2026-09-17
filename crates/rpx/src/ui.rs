pub(crate) fn progress_spinner_style() -> tracing_indicatif::style::ProgressStyle {
    tracing_indicatif::style::ProgressStyle::with_template("{span_child_prefix}{spinner} {msg}")
        .expect("progress spinner style should be valid")
}

pub(crate) fn progress_count_style() -> tracing_indicatif::style::ProgressStyle {
    tracing_indicatif::style::ProgressStyle::with_template(
        "{span_child_prefix}{spinner} {msg} [{bar:24.cyan/blue}] {pos}/{len}",
    )
    .expect("progress count style should be valid")
}

pub(crate) fn progress_bar_style() -> tracing_indicatif::style::ProgressStyle {
    tracing_indicatif::style::ProgressStyle::with_template(
        "{span_child_prefix}{spinner} {msg} [{bar:24.cyan/blue}] {bytes}/{total_bytes}",
    )
    .expect("progress bar style should be valid")
}
