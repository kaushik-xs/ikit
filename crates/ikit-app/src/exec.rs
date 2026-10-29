//! Running external commands (ssh, docker, kubectl) behind a trait so probes can be tested.

use std::io;
use std::time::Duration;

use async_trait::async_trait;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    /// Exit code; `None` if the process was killed by a signal or timed out.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// stdout as bytes, for binary output such as database dumps.
    pub raw_stdout: Vec<u8>,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The most useful one-line explanation of a failure.
    pub fn failure_detail(&self) -> String {
        if self.timed_out {
            return "timed out".to_owned();
        }
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let line = text
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .unwrap_or("");
        match self.code {
            Some(code) if line.is_empty() => format!("exited with {code}"),
            _ => line.to_owned(),
        }
    }
}

#[async_trait]
pub trait Exec: Send + Sync {
    /// Runs `program` with `args`, writing `stdin` to it if given. A missing program is an
    /// `io::ErrorKind::NotFound` error.
    async fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<ExecOutput>;
}

#[derive(Debug, Default)]
pub struct SystemExec;

#[async_trait]
impl Exec for SystemExec {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<ExecOutput> {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;

        tracing::debug!(program, ?args, "exec");
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(input).await?;
        }
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(output) => {
                let output = output?;
                Ok(ExecOutput {
                    code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    timed_out: false,
                    raw_stdout: output.stdout,
                })
            }
            Err(_) => Ok(ExecOutput {
                timed_out: true,
                ..Default::default()
            }),
        }
    }
}
