//! Glob pattern to regex conversion for Seatbelt profiles.

/// Convert a glob pattern to a Seatbelt-compatible regex.
///
/// Conversion rules:
/// - `*` matches any characters except `/`
/// - `**` matches any characters including `/`
/// - `?` matches any single character except `/`
/// - `{a,b}` matches either `a` or `b`
/// - Special regex characters are escaped
pub fn glob_to_regex(pattern: &str) -> String {
    let mut result = String::with_capacity(pattern.len() * 2);
    result.push('^');

    // First, handle globstar patterns by replacing them with placeholders
    let pattern = pattern
        .replace("**/", "__GLOBSTAR_SLASH__")
        .replace("**", "__GLOBSTAR__");

    let mut chars = pattern.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // Escape special regex characters (except our placeholders)
            '.' | '^' | '$' | '+' | '|' | '\\' | '(' | ')' => {
                result.push('\\');
                result.push(c);
            }
            '[' => {
                // Handle character classes
                result.push('[');
                // Copy until closing bracket
                while let Some(cc) = chars.next() {
                    if cc == ']' {
                        result.push(']');
                        break;
                    }
                    result.push(cc);
                }
            }
            '{' => {
                // Handle alternation {a,b,c}
                result.push('(');
                while let Some(cc) = chars.next() {
                    match cc {
                        '}' => {
                            result.push(')');
                            break;
                        }
                        ',' => result.push('|'),
                        _ => result.push(cc),
                    }
                }
            }
            '*' => {
                // Single wildcard - matches anything except /
                result.push_str("[^/]*");
            }
            '?' => {
                // Single character - matches anything except /
                result.push_str("[^/]");
            }
            '_' => {
                // Check for our placeholders
                if pattern[chars.clone().count()..].starts_with("_GLOBSTAR_SLASH__") {
                    // Skip the placeholder characters
                    for _ in 0..16 {
                        chars.next();
                    }
                    // **/ matches zero or more directories
                    result.push_str("(.*/)?");
                } else if pattern[chars.clone().count()..].starts_with("_GLOBSTAR__") {
                    // Skip the placeholder characters
                    for _ in 0..10 {
                        chars.next();
                    }
                    // ** matches anything
                    result.push_str(".*");
                } else {
                    result.push(c);
                }
            }
            _ => result.push(c),
        }
    }

    result.push('$');
    result
}

/// Convert a glob pattern to a Seatbelt-compatible regex (simpler version).
/// This version is used for the actual implementation.
pub fn glob_to_seatbelt_regex(pattern: &str) -> String {
    // Handle the pattern step by step
    let mut result = String::with_capacity(pattern.len() * 2);
    result.push('^');

    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        match c {
            // Escape special regex characters
            '.' | '^' | '$' | '+' | '|' | '\\' | '(' | ')' => {
                result.push('\\');
                result.push(c);
            }
            '[' => {
                // Copy character class as-is
                result.push('[');
                i += 1;
                while i < chars.len() && chars[i] != ']' {
                    result.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    result.push(']');
                }
            }
            '{' => {
                // Convert {a,b,c} to (a|b|c)
                result.push('(');
                i += 1;
                while i < chars.len() && chars[i] != '}' {
                    if chars[i] == ',' {
                        result.push('|');
                    } else {
                        result.push(chars[i]);
                    }
                    i += 1;
                }
                result.push(')');
            }
            '*' => {
                // Check for **
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    // Check for **/
                    if i + 2 < chars.len() && chars[i + 2] == '/' {
                        // **/ matches zero or more directories
                        result.push_str("(.*/)?");
                        i += 2; // Skip the second * and /
                    } else {
                        // ** matches anything
                        result.push_str(".*");
                        i += 1; // Skip the second *
                    }
                } else {
                    // * matches anything except /
                    result.push_str("[^/]*");
                }
            }
            '?' => {
                // ? matches any single character except /
                result.push_str("[^/]");
            }
            _ => result.push(c),
        }

        i += 1;
    }

    result.push('$');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn test_simple_patterns() {
        // *.ts should match file.ts but not dir/file.ts
        let pattern = glob_to_seatbelt_regex("*.ts");
        let re = Regex::new(&pattern).unwrap();
        assert!(re.is_match("file.ts"));
        assert!(re.is_match("test.ts"));
        assert!(!re.is_match("dir/file.ts"));
    }

    #[test]
    fn test_globstar() {
        // src/** should match anything under src/
        let pattern = glob_to_seatbelt_regex("src/**");
        let re = Regex::new(&pattern).unwrap();
        assert!(re.is_match("src/"));
        assert!(re.is_match("src/file.ts"));
        assert!(re.is_match("src/deep/file.ts"));
    }

    #[test]
    fn test_globstar_slash() {
        // **/*.ts should match any .ts file in any directory
        let pattern = glob_to_seatbelt_regex("**/*.ts");
        let re = Regex::new(&pattern).unwrap();
        assert!(re.is_match("file.ts"));
        assert!(re.is_match("dir/file.ts"));
        assert!(re.is_match("deep/dir/file.ts"));
    }

    #[test]
    fn test_braces() {
        // *.{ts,js} should match .ts or .js files
        let pattern = glob_to_seatbelt_regex("*.{ts,js}");
        let re = Regex::new(&pattern).unwrap();
        assert!(re.is_match("file.ts"));
        assert!(re.is_match("file.js"));
        assert!(!re.is_match("file.py"));
    }

    #[test]
    fn test_question_mark() {
        // file?.txt should match file1.txt but not file.txt or file12.txt
        let pattern = glob_to_seatbelt_regex("file?.txt");
        let re = Regex::new(&pattern).unwrap();
        assert!(re.is_match("file1.txt"));
        assert!(re.is_match("fileA.txt"));
        assert!(!re.is_match("file.txt"));
        assert!(!re.is_match("file12.txt"));
    }

    #[test]
    fn test_special_chars_escaped() {
        // path.with.dots should escape the dots
        let pattern = glob_to_seatbelt_regex("path.with.dots");
        let re = Regex::new(&pattern).unwrap();
        assert!(re.is_match("path.with.dots"));
        assert!(!re.is_match("pathXwithYdots"));
    }
}
