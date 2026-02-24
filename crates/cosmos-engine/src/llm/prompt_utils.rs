const MAX_REPO_MEMORY_SECTION_CHARS: usize = 1200;

/// Format optional repo memory into a prompt section.
pub(crate) fn format_repo_memory_section(repo_memory: Option<&str>, heading: &str) -> String {
    repo_memory
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| {
            let content = if m.chars().count() > MAX_REPO_MEMORY_SECTION_CHARS {
                let prefix: String = m.chars().take(MAX_REPO_MEMORY_SECTION_CHARS).collect();
                format!("{}...", prefix)
            } else {
                m.to_string()
            };
            format!("\n\n{}:\n{}", heading, content)
        })
        .unwrap_or_default()
}
