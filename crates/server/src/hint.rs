/// Bullet lists deliberately excluded — too many false positives.
pub fn looks_like_workflow(content: &str) -> bool {
    let lower = content.to_lowercase();
    if lower.contains("step 1") || lower.contains("step 2") {
        return true;
    }

    let numbered_count = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            let mut chars = trimmed.chars();
            match chars.next() {
                Some(c) if c.is_ascii_digit() => {
                    let sep = chars.find(|c| !c.is_ascii_digit());
                    sep == Some('.') || sep == Some(')')
                }
                _ => false,
            }
        })
        .count();

    numbered_count >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_list_triggers() {
        let content = "1. Clone the repo\n2. Run cargo build\n3. Run cargo test";
        assert!(looks_like_workflow(content));
    }

    #[test]
    fn step_keyword_triggers() {
        let content = "First do step 1, then proceed to step 2.";
        assert!(looks_like_workflow(content));
    }

    #[test]
    fn step_keyword_case_insensitive() {
        let content = "Complete Step 1 before moving on.";
        assert!(looks_like_workflow(content));
    }

    #[test]
    fn bullet_list_does_not_trigger() {
        let content = "- Review the PR\n- Check the tests\n- Merge if green\n- Deploy";
        assert!(!looks_like_workflow(content));
    }

    #[test]
    fn short_content_does_not_trigger() {
        let content = "The server crashed at 3pm.";
        assert!(!looks_like_workflow(content));
    }

    #[test]
    fn two_numbered_lines_not_enough() {
        let content = "1. First thing\n2. Second thing";
        assert!(!looks_like_workflow(content));
    }

    #[test]
    fn parenthesis_style_numbering() {
        let content = "1) Open the file\n2) Edit the config\n3) Save and restart";
        assert!(looks_like_workflow(content));
    }
}
