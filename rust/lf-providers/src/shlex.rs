//! Python `shlex.shlex(raw, posix=True)` with `whitespace_split = True`,
//! default `commenters="#"`, quotes `'"`, escape `\`, escapedquotes `"`.
//! Used only to read literal `.env` assignments; nothing is executed.

const WHITESPACE: &[char] = &[' ', '\t', '\r', '\n'];

fn is_ws(c: char) -> bool {
    WHITESPACE.contains(&c)
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Space,
    Word,
    Quote(char),
    Escape(char), // char = the state to return to: 'a' word or the quote
}

/// `list(lexer)`; `Err` where Python raises `ValueError`.
pub(crate) fn split(raw: &str) -> Result<Vec<String>, &'static str> {
    let mut tokens = Vec::new();
    let mut chars = raw.chars();
    loop {
        // One `read_token` call.
        let mut token = String::new();
        let mut quoted = false;
        let mut state = State::Space;
        loop {
            let next = chars.next();
            match state {
                State::Space => match next {
                    None => break,
                    Some(c) if is_ws(c) => {
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some('#') => {
                        // readline(): the input is a single line.
                        chars = "".chars();
                    }
                    Some('\\') => state = State::Escape('a'),
                    Some(c) if c == '\'' || c == '"' => state = State::Quote(c),
                    Some(c) => {
                        token.push(c);
                        state = State::Word;
                    }
                },
                State::Quote(q) => {
                    quoted = true;
                    match next {
                        None => return Err("No closing quotation"),
                        Some(c) if c == q => state = State::Word,
                        Some('\\') if q == '"' => state = State::Escape(q),
                        Some(c) => token.push(c),
                    }
                }
                State::Escape(back) => match next {
                    None => return Err("No escaped character"),
                    Some(c) => {
                        if back != 'a' && c != '\\' && c != back {
                            token.push('\\');
                        }
                        token.push(c);
                        state = if back == 'a' {
                            State::Word
                        } else {
                            State::Quote(back)
                        };
                    }
                },
                State::Word => match next {
                    None => break,
                    Some(c) if is_ws(c) => {
                        if !token.is_empty() || quoted {
                            break;
                        }
                        state = State::Space;
                    }
                    Some('#') => {
                        chars = "".chars();
                        if !token.is_empty() || quoted {
                            break;
                        }
                        state = State::Space;
                    }
                    Some(c) if c == '\'' || c == '"' => state = State::Quote(c),
                    Some('\\') => state = State::Escape('a'),
                    Some(c) => token.push(c),
                },
            }
        }
        if token.is_empty() && !quoted {
            return Ok(tokens); // posix eof (None)
        }
        tokens.push(token);
    }
}

#[cfg(test)]
mod tests {
    use super::split;

    fn s(raw: &str) -> Result<Vec<String>, &'static str> {
        split(raw)
    }

    #[test]
    fn matches_python_shlex() {
        assert_eq!(s("abc").unwrap(), vec!["abc"]);
        assert_eq!(s("\"fire works\" # comment").unwrap(), vec!["fire works"]);
        assert_eq!(s("'open#ai'").unwrap(), vec!["open#ai"]);
        assert_eq!(s("groq # comment").unwrap(), vec!["groq"]);
        assert_eq!(s("a#b").unwrap(), vec!["a"]);
        assert_eq!(s("").unwrap(), Vec::<String>::new());
        assert_eq!(s("   ").unwrap(), Vec::<String>::new());
        assert_eq!(s("\"\"").unwrap(), vec![""]);
        assert_eq!(s("a b").unwrap(), vec!["a", "b"]);
        assert_eq!(s("a'b c'd").unwrap(), vec!["ab cd"]);
        assert_eq!(s(r#""a\"b\\c\d""#).unwrap(), vec![r#"a"b\c\d"#]);
        assert_eq!(s(r"'a\b'").unwrap(), vec![r"a\b"]);
        assert_eq!(s(r"a\ b").unwrap(), vec!["a b"]);
        assert_eq!(s("$(command)").unwrap(), vec!["$(command)"]);
        assert_eq!(s("x;y").unwrap(), vec!["x;y"]);
        assert_eq!(s("'unterminated"), Err("No closing quotation"));
        assert_eq!(s("a\\"), Err("No escaped character"));
        assert_eq!(s("# only comment").unwrap(), Vec::<String>::new());
    }
}
