//! Runs the `ikit` binary itself. The TUI never touches state directly: every read and every
//! change goes through the same commands a person would type, so remote locking, syncing,
//! prompts and output stay exactly the CLI's.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

use serde_json::Value;

/// How to start `ikit`, and the global flags every invocation carries.
#[derive(Debug, Clone)]
pub struct Runner {
    pub exe: PathBuf,
    /// Global flags forwarded as given to `ikit tui` (--home, --identity-file, ...).
    pub base_args: Vec<String>,
}

/// Messages background jobs send to the UI thread.
pub enum Msg {
    /// A JSON read finished: (request id, result).
    Loaded(u64, Result<Value, String>),
    /// A line of output from a captured job.
    Line(u64, String),
    /// A captured job exited.
    Done(u64, Result<ExitStatus, String>),
}

impl Runner {
    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.args(&self.base_args).args(args);
        cmd
    }

    /// `ikit <args> -o json --no-input` in the background; parses stdout as JSON.
    pub fn read_json(&self, id: u64, args: Vec<String>, tx: Sender<Msg>) {
        let mut cmd = self.command(&args);
        cmd.args(["--output", "json", "--no-input"])
            .stdin(Stdio::null());
        thread::spawn(move || {
            let result = match cmd.output() {
                Err(e) => Err(format!("could not start ikit: {e}")),
                Ok(out) if !out.status.success() && out.stdout.is_empty() => {
                    Err(clean(&String::from_utf8_lossy(&out.stderr))
                        .trim()
                        .to_string())
                }
                Ok(out) => serde_json::from_slice(&out.stdout).map_err(|e| {
                    let err = clean(&String::from_utf8_lossy(&out.stderr));
                    if err.trim().is_empty() {
                        format!("unexpected output: {e}")
                    } else {
                        err.trim().to_string()
                    }
                }),
            };
            let _ = tx.send(Msg::Loaded(id, result));
        });
    }

    /// `ikit <args> --no-input` in the background, streaming its output line by line.
    pub fn run_captured(&self, id: u64, args: Vec<String>, tx: Sender<Msg>) {
        let mut cmd = self.command(&args);
        cmd.arg("--no-input")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        thread::spawn(move || {
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Msg::Done(id, Err(format!("could not start ikit: {e}"))));
                    return;
                }
            };
            let mut readers = Vec::new();
            if let Some(out) = child.stdout.take() {
                readers.push(pipe_lines(id, out, tx.clone()));
            }
            if let Some(err) = child.stderr.take() {
                readers.push(pipe_lines(id, err, tx.clone()));
            }
            for r in readers {
                let _ = r.join();
            }
            let status = child.wait().map_err(|e| e.to_string());
            let _ = tx.send(Msg::Done(id, status));
        });
    }

    /// `ikit <args>` attached to the real terminal, for commands that prompt.
    pub fn run_attached(&self, args: &[String]) -> std::io::Result<ExitStatus> {
        self.command(args).status()
    }

    /// The command line shown to the user.
    pub fn display(&self, args: &[String]) -> String {
        let mut parts = vec!["ikit".to_string()];
        parts.extend(args.iter().map(|a| quote(a)));
        parts.join(" ")
    }
}

fn pipe_lines(
    id: u64,
    pipe: impl std::io::Read + Send + 'static,
    tx: Sender<Msg>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(pipe).lines() {
            match line {
                Ok(l) => {
                    if tx.send(Msg::Line(id, clean(&l))).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    })
}

/// Strips ANSI escape sequences and carriage-return progress noise.
pub fn clean(s: &str) -> String {
    let s = s.rsplit('\r').next().unwrap_or(s);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else if c == '\t' {
            out.push_str("    ");
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

fn quote(a: &str) -> String {
    if !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@,^+%".contains(c))
    {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', r"'\''"))
    }
}

/// Splits a command line typed into the palette, honouring single and double quotes.
pub fn split_args(line: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_arg = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    in_arg = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_arg = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unterminated quote".into());
    }
    if in_arg {
        args.push(cur);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_args() {
        assert_eq!(
            split_args(r#"route add web --host "a b.com" --path '/x y' c\ d"#).unwrap(),
            vec![
                "route", "add", "web", "--host", "a b.com", "--path", "/x y", "c d"
            ]
        );
        assert_eq!(split_args("  ").unwrap(), Vec::<String>::new());
        assert_eq!(split_args("a ''").unwrap(), vec!["a", ""]);
        assert!(split_args("a 'b").is_err());
    }

    #[test]
    fn cleans_ansi() {
        assert_eq!(clean("\u{1b}[1;32mok\u{1b}[0m done"), "ok done");
        assert_eq!(clean("10%\r50%\r100%"), "100%");
    }

    #[test]
    fn quotes_for_display() {
        let r = Runner {
            exe: "ikit".into(),
            base_args: vec![],
        };
        assert_eq!(
            r.display(&["route".into(), "add".into(), "a b".into()]),
            "ikit route add 'a b'"
        );
    }
}
