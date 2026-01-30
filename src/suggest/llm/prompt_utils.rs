/// Format optional repo memory into a prompt section.
pub(crate) fn format_repo_memory_section(repo_memory: Option<&str>, heading: &str) -> String {
    repo_memory
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("\n\n{}:\n{}", heading, m))
        .unwrap_or_default()
}
