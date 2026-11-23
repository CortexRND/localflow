//! Port of click's hidden `prompt` over [`Io`].

use std::io::BufRead;

use anyhow::Result;

use crate::{outln, Io};

/// `click.prompt(text, hide_input=True)` with no default: an empty answer asks
/// again; `None` at end of input (click's `Abort`, after its newline). From a
/// terminal the value is read without echo; otherwise one line of `io.input`
/// is read after the prompt line, as click's test runner lays it out.
pub(crate) fn hidden(text: &str, io: &mut Io<'_>) -> Result<Option<String>> {
    loop {
        let read = if io.terminal {
            write!(io.out, "{text}: ")?;
            io.out.flush()?;
            match read_without_echo() {
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

/// One line from the terminal without echo. Like Python's `getpass`, a
/// process with no controlling terminal reads from stdin, a terminal here.
fn read_without_echo() -> std::io::Result<String> {
    #[cfg(unix)]
    if std::fs::File::open("/dev/tty").is_err() {
        let config = rpassword::ConfigBuilder::new()
            .input_file_path("/dev/stdin")
            .output_writer(std::io::stdout())
            .build();
        return rpassword::read_password_with_config(config);
    }
    rpassword::read_password()
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
    fn hidden_reprompts_on_empty_and_aborts_at_end() {
        let (got, out, _) = with_io("\n\n  v \n", |io| hidden("secret", io).unwrap());
        assert_eq!(got.as_deref(), Some("  v "));
        assert_eq!(out, "secret: \nsecret: \nsecret: \n");
        let (got, out, _) = with_io("\n", |io| hidden("secret", io).unwrap());
        assert_eq!(got, None);
        assert_eq!(out, "secret: \nsecret: \n\n");
    }
}
