//! Ports of click's `confirm` and hidden `prompt` over [`Io`].

use std::io::BufRead;

use anyhow::Result;

use crate::pyfmt::py_strip;
use crate::{outln, Io};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    Yes,
    No,
    /// End of input: click raises `Abort` (`Aborted!`, exit 1).
    Aborted,
}

/// `click.confirm(text, default=False)`: y/yes or n/no (any case, surrounding
/// whitespace ignored); empty means no; anything else prints
/// `Error: invalid input` (to stdout, like click) and asks again.
pub(crate) fn confirm(text: &str, io: &mut Io<'_>) -> Result<Answer> {
    loop {
        write!(io.out, "{text} [y/N]: ")?;
        io.out.flush()?;
        let Some(line) = read_line(io.input)? else {
            return Ok(Answer::Aborted);
        };
        match py_strip(&line).to_lowercase().as_str() {
            "y" | "yes" => return Ok(Answer::Yes),
            "n" | "no" | "" => return Ok(Answer::No),
            _ => outln!(io.out, "Error: invalid input")?,
        }
    }
}

/// `click.prompt(text, hide_input=True)` with no default: an empty answer asks
/// again; `None` at end of input (click's `Abort`, after its newline). From a
/// terminal the value is read without echo; otherwise one line of `io.input`
/// is read after the prompt line, as click's test runner lays it out.
pub(crate) fn hidden(text: &str, io: &mut Io<'_>) -> Result<Option<String>> {
    loop {
        let read = if io.terminal {
            write!(io.out, "{text}: ")?;
            io.out.flush()?;
            match rpassword::read_password() {
                Ok(value) => Some(value),
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => None,
                Err(error) => return Err(error.into()),
            }
        } else {
            outln!(io.out, "{text}: ")?;
            io.out.flush()?;
            read_line(io.input)?
        };
        match read {
            None => {
                outln!(io.out)?;
                return Ok(None);
            }
            Some(value) if value.is_empty() => continue,
            Some(value) => return Ok(Some(value)),
        }
    }
}

/// One line without its line ending; `None` at end of input.
fn read_line(input: &mut dyn BufRead) -> Result<Option<String>> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::testing::FakeClipboard;

    fn with_io<T>(input: &str, f: impl FnOnce(&mut Io<'_>) -> T) -> (T, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut clipboard = FakeClipboard::default();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut reader,
            clipboard: &mut clipboard,
            server_url: None,
            terminal: false,
        };
        let value = f(&mut io);
        (
            value,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn confirm_answers() {
        for (input, answer) in [
            ("y\n", Answer::Yes),
            (" YES \r\n", Answer::Yes),
            ("n\n", Answer::No),
            ("No", Answer::No),
            ("\n", Answer::No),
            ("", Answer::Aborted),
        ] {
            let (got, out, _) = with_io(input, |io| confirm("Q?", io).unwrap());
            assert_eq!(got, answer, "{input:?}");
            assert_eq!(out, "Q? [y/N]: ", "{input:?}");
        }
        let (got, out, err) = with_io("maybe\nyes\n", |io| confirm("Q?", io).unwrap());
        assert_eq!(got, Answer::Yes);
        assert_eq!(out, "Q? [y/N]: Error: invalid input\nQ? [y/N]: ");
        assert!(err.is_empty());
        let (got, _, _) = with_io("maybe\n", |io| confirm("Q?", io).unwrap());
        assert_eq!(got, Answer::Aborted);
    }

    #[test]
    fn hidden_reprompts_on_empty_and_aborts_at_end() {
        let (got, out, _) = with_io("\n\n  v \n", |io| hidden("secret", io).unwrap());
        assert_eq!(got.as_deref(), Some("  v "));
        assert_eq!(out, "secret: \nsecret: \nsecret: \n");
        let (got, out, _) = with_io("\n", |io| hidden("secret", io).unwrap());
        assert_eq!(got, None);
        assert_eq!(out, "secret: \nsecret: \n\n");
    }
}
