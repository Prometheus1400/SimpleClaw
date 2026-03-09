//! Shell quoting utilities.

/// Quote a string for use in a shell command.
/// This wraps the string in single quotes and escapes any existing single quotes.
pub fn quote(s: &str) -> String {
    // If the string is empty, return empty quoted string
    if s.is_empty() {
        return "''".to_string();
    }

    // If the string contains no special characters, return it as-is
    if !needs_quoting(s) {
        return s.to_string();
    }

    // Use single quotes and escape any existing single quotes
    // 'abc' -> 'abc'
    // abc's -> 'abc'"'"'s'
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Quote a string for use as a shell argument, always using quotes.
pub fn quote_always(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Check if a string needs quoting.
fn needs_quoting(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            c,
            ' ' | '\t'
                | '\n'
                | '\r'
                | '"'
                | '\''
                | '\\'
                | '$'
                | '`'
                | '!'
                | '*'
                | '?'
                | '['
                | ']'
                | '{'
                | '}'
                | '('
                | ')'
                | '<'
                | '>'
                | '|'
                | '&'
                | ';'
                | '#'
                | '~'
        )
    })
}

/// Join arguments with proper quoting for shell execution.
pub fn join_args<I, S>(args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .map(|s| quote(s.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse a shell command string into arguments.
/// Uses shell-words crate for proper handling.
pub fn split_args(s: &str) -> Result<Vec<String>, shell_words::ParseError> {
    shell_words::split(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote() {
        assert_eq!(quote("simple"), "simple");
        assert_eq!(quote("with space"), "'with space'");
        assert_eq!(quote("it's"), "'it'\"'\"'s'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("$var"), "'$var'");
    }

    #[test]
    fn test_join_args() {
        let args = vec!["echo", "hello world", "it's"];
        assert_eq!(join_args(args), "echo 'hello world' 'it'\"'\"'s'");
    }

    #[test]
    fn test_split_args() {
        let args = split_args("echo 'hello world' test").unwrap();
        assert_eq!(args, vec!["echo", "hello world", "test"]);
    }
}
