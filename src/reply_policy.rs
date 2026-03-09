pub(crate) const NO_REPLY_SENTINEL: &str = "NO_REPLY";
const NO_REPLY_PROMPT_INSTRUCTION: &str = "\n\nNO-REPLY BEHAVIOR:\n- If you intentionally want the system to send no user-facing message, respond with exactly NO_REPLY (no extra text).";

pub(crate) fn is_no_reply(reply: &str) -> bool {
    reply.trim() == NO_REPLY_SENTINEL
}

pub(crate) fn no_reply_prompt_instruction() -> &'static str {
    NO_REPLY_PROMPT_INSTRUCTION
}

#[cfg(test)]
mod tests {
    use super::is_no_reply;

    #[test]
    fn matches_exact_sentinel() {
        assert!(is_no_reply("NO_REPLY"));
    }

    #[test]
    fn matches_trimmed_sentinel() {
        assert!(is_no_reply("  NO_REPLY\n"));
    }

    #[test]
    fn does_not_match_other_text() {
        assert!(!is_no_reply("NO_REPLY please"));
    }

    #[test]
    fn does_not_match_different_case() {
        assert!(!is_no_reply("no_reply"));
    }
}
