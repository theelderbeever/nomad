/// Split a SQL script into individual statements on `;`, respecting single-quoted
/// strings, double-quoted and backtick-quoted identifiers, `--` line comments, and
/// (non-nested) `/* */` block comments — matching ClickHouse's lexer. Needed because
/// the ClickHouse HTTP interface accepts exactly one statement per request.
pub fn split_statements(sql: &str) -> Vec<String> {
    #[derive(PartialEq)]
    enum State {
        Normal,
        Quoted(char), // '\'', '"', or '`'
        LineComment,
        BlockComment,
    }

    let mut statements = Vec::new();
    let mut current = String::new();
    let mut state = State::Normal;
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match state {
            State::Normal => match c {
                ';' => {
                    push_statement(&mut statements, &mut current);
                    continue;
                }
                '\'' | '"' | '`' => state = State::Quoted(c),
                '-' if chars.peek() == Some(&'-') => state = State::LineComment,
                '/' if chars.peek() == Some(&'*') => state = State::BlockComment,
                _ => {}
            },
            State::Quoted(q) => {
                if c == '\\' {
                    // Backslash escapes the next char; copy it through verbatim.
                    current.push(c);
                    if let Some(escaped) = chars.next() {
                        current.push(escaped);
                    }
                    continue;
                }
                if c == q {
                    if chars.peek() == Some(&q) {
                        // Doubled quote ('' / "" / ``) is an escaped quote.
                        current.push(c);
                        current.push(chars.next().unwrap());
                        continue;
                    }
                    state = State::Normal;
                }
            }
            State::LineComment => {
                if c == '\n' {
                    state = State::Normal;
                }
            }
            State::BlockComment => {
                if c == '*' && chars.peek() == Some(&'/') {
                    current.push(c);
                    current.push(chars.next().unwrap());
                    state = State::Normal;
                    continue;
                }
            }
        }
        current.push(c);
    }
    push_statement(&mut statements, &mut current);
    statements
}

fn push_statement(statements: &mut Vec<String>, current: &mut String) {
    let stmt = std::mem::take(current);
    let stmt = stmt.trim();
    if !stmt.is_empty() && !is_comments_only(stmt) {
        statements.push(stmt.to_string());
    }
}

/// True if the text contains nothing but whitespace and comments.
fn is_comments_only(sql: &str) -> bool {
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '-' && chars.peek() == Some(&'-') {
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == '\n' {
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            loop {
                match chars.next() {
                    Some('*') if chars.peek() == Some(&'/') => {
                        chars.next();
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        } else {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_simple_statements() {
        let got = split_statements("CREATE TABLE a (x Int64) ENGINE = Memory;\nDROP TABLE a;");
        assert_eq!(got, vec!["CREATE TABLE a (x Int64) ENGINE = Memory", "DROP TABLE a"]);
    }

    #[test]
    fn trailing_semicolon_and_whitespace_are_harmless() {
        let got = split_statements("SELECT 1;\n\n  ;\n");
        assert_eq!(got, vec!["SELECT 1"]);
    }

    #[test]
    fn no_trailing_semicolon() {
        let got = split_statements("SELECT 1");
        assert_eq!(got, vec!["SELECT 1"]);
    }

    #[test]
    fn semicolon_inside_single_quotes() {
        let got = split_statements("INSERT INTO t VALUES ('a;b'); SELECT 1;");
        assert_eq!(got, vec!["INSERT INTO t VALUES ('a;b')", "SELECT 1"]);
    }

    #[test]
    fn doubled_quote_escape() {
        let got = split_statements("SELECT 'it''s; fine'; SELECT 2;");
        assert_eq!(got, vec!["SELECT 'it''s; fine'", "SELECT 2"]);
    }

    #[test]
    fn backslash_escape_in_string() {
        let got = split_statements(r"SELECT 'a\';b'; SELECT 2;");
        assert_eq!(got, vec![r"SELECT 'a\';b'", "SELECT 2"]);
    }

    #[test]
    fn semicolon_inside_backtick_identifier() {
        let got = split_statements("CREATE TABLE `weird;name` (x Int64) ENGINE = Memory;");
        assert_eq!(got, vec!["CREATE TABLE `weird;name` (x Int64) ENGINE = Memory"]);
    }

    #[test]
    fn semicolon_inside_double_quoted_identifier() {
        let got = split_statements(r#"CREATE TABLE "weird;name" (x Int64) ENGINE = Memory;"#);
        assert_eq!(got, vec![r#"CREATE TABLE "weird;name" (x Int64) ENGINE = Memory"#]);
    }

    #[test]
    fn semicolon_in_line_comment() {
        let got = split_statements("SELECT 1 -- not a break; really\n+ 2;");
        assert_eq!(got, vec!["SELECT 1 -- not a break; really\n+ 2"]);
    }

    #[test]
    fn semicolon_in_block_comment() {
        let got = split_statements("SELECT 1 /* ; */ + 2;");
        assert_eq!(got, vec!["SELECT 1 /* ; */ + 2"]);
    }

    #[test]
    fn comment_only_chunks_are_dropped() {
        let got = split_statements("-- header comment\n;\n/* block */;\nSELECT 1;");
        assert_eq!(got, vec!["SELECT 1"]);
    }

    #[test]
    fn leading_comment_is_kept_with_statement() {
        let got = split_statements("-- adds the users table\nCREATE TABLE users (id UInt64) ENGINE = Memory;");
        assert_eq!(got, vec!["-- adds the users table\nCREATE TABLE users (id UInt64) ENGINE = Memory"]);
    }

    #[test]
    fn empty_input() {
        assert!(split_statements("").is_empty());
        assert!(split_statements("   \n  ").is_empty());
        assert!(split_statements("-- only a comment\n").is_empty());
    }
}
