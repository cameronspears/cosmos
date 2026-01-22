pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }

    let char_count = s.chars().count();
    if char_count <= max {
        return s.to_string();
    }

    if max <= 3 {
        return s.chars().take(max).collect();
    }

    let truncated: String = s.chars().take(max - 3).collect();
    format!("{}...", truncated)
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn test_truncate_unicode_safe() {
        let input = "ééééé";
        assert_eq!(truncate(input, 4), "é...");
    }

    #[test]
    fn test_truncate_small_max() {
        let input = "こんにちは";
        assert_eq!(truncate(input, 3), "こんに");
        assert_eq!(truncate(input, 0), "");
    }
}
