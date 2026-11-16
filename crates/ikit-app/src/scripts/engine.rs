//! The program ikit feeds to `sh -s` to run a script.
//!
//! Everything travels on stdin: the script's body and its CSV input (as quoted heredocs, so
//! nothing in them is expanded) and its env (as quoted `export`s). Nothing secret is ever on
//! a command line, where `ps`, shell history or `docker inspect` would show it. The script
//! itself runs with stdin from `/dev/null`: scripts are non-interactive.
//!
//! SQL scripts run in a database service's container with its own client and credentials
//! (`psql` or `mysql`, like `--via exec` SQL hooks). Instead of env vars they get their params
//! as SQL variables, written to a private file that is read before the script and then
//! deleted: `:'NAME'` (a literal), `:"NAME"` (an identifier) or `:NAME` in Postgres, and
//! `@NAME` in MySQL.

use ikit_core::{Interpreter, SqlEngine};

use crate::deploy::{sh, sh_word};

/// Where the script's files are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkDir {
    /// The current directory (the workload's compose project folder); the body stays in
    /// `scripts/` afterwards for reference.
    Here,
    /// A fresh temporary directory, removed afterwards.
    Temp,
}

pub(crate) struct Payload<'a> {
    pub name: &'a str,
    pub file_name: &'a str,
    pub interpreter: Interpreter,
    pub body: &'a str,
    pub checksum: &'a str,
    /// The CSV file's content, if the script takes one.
    pub input: Option<&'a str>,
    /// Pass the CSV file's path as the last argument.
    pub input_arg: bool,
    /// Names must be valid env names (checked when the script and params are validated).
    pub env: &'a [(String, String)],
    pub args: &'a [String],
    pub workdir: WorkDir,
    /// Set for SQL scripts.
    pub sql: Option<&'a SqlRun>,
}

/// How a SQL script reaches its database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SqlRun {
    pub engine: SqlEngine,
    /// `None` uses the database the container's env configures.
    pub database: Option<String>,
    pub transaction: bool,
}

/// `value` as a psql meta-command argument: single-quoted, with `'` doubled and backslash
/// escapes for backslashes and line breaks.
fn psql_quote(value: &str) -> String {
    let mut out = String::from("'");
    for c in value.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// `value` as a MySQL string literal.
fn mysql_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The file that sets a SQL script's variables. Names are valid env names, so they are
/// valid variable names too.
pub(crate) fn sql_vars(engine: SqlEngine, env: &[(String, String)]) -> String {
    env.iter()
        .map(|(k, v)| match engine {
            SqlEngine::Postgres => format!("\\set {k} {}\n", psql_quote(v)),
            SqlEngine::Mysql => format!("SET @{k} = {};\n", mysql_quote(v)),
        })
        .collect()
}

/// The client command, run in the container, for a SQL script and its variables file.
fn sql_command(sql: &SqlRun, script: &str, vars: &str) -> String {
    match sql.engine {
        SqlEngine::Postgres => {
            let db =
                sql.database.as_deref().map(sh).unwrap_or_else(|| {
                    r#""${POSTGRES_DB:-${POSTGRES_USER:-postgres}}""#.to_owned()
                });
            let single = if sql.transaction {
                " --single-transaction"
            } else {
                ""
            };
            format!(
                r#"psql -X -q -v ON_ERROR_STOP=1{single} -U "${{POSTGRES_USER:-postgres}}" -d {db} -f {} -f {} </dev/null"#,
                sh(vars),
                sh(script)
            )
        }
        SqlEngine::Mysql => {
            let db = sql
                .database
                .as_deref()
                .map(sh)
                .unwrap_or_else(|| r#"${MYSQL_DATABASE:+"$MYSQL_DATABASE"}"#.to_owned());
            let (begin, commit) = if sql.transaction {
                ("echo 'START TRANSACTION;'; ", "; echo 'COMMIT;'")
            } else {
                ("", "")
            };
            format!(
                r#"{{ cat {}; {begin}cat {}{commit}; }} | MYSQL_PWD="$MYSQL_ROOT_PASSWORD" mysql -uroot {db}"#,
                sh(vars),
                sh(script)
            )
        }
    }
}

/// A heredoc delimiter that no line of `texts` equals.
fn delimiter(checksum: &str, texts: &[&str]) -> String {
    let mut d = format!("__IKIT_EOF_{}__", &checksum[..checksum.len().min(8)]);
    while texts.iter().any(|t| t.lines().any(|l| l == d)) {
        d.push('_');
    }
    d
}

fn heredoc(out: &mut String, path: &str, content: &str, delim: &str) {
    out.push_str(&format!("cat > {path} <<'{delim}'\n"));
    out.push_str(content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(delim);
    out.push('\n');
}

fn input_name(name: &str) -> String {
    format!("{name}.input")
}

/// How the script is invoked, for previews: `bash scripts/create-db.sh -y <input>`.
pub(crate) fn command_line(p: &Payload) -> String {
    if let Some(sql) = p.sql {
        let db = sql.database.as_deref().unwrap_or("<its default database>");
        let file = format!("scripts/{}", p.file_name);
        return match sql.engine {
            SqlEngine::Postgres if sql.transaction => {
                format!("psql -d {db} --single-transaction -f {file}")
            }
            SqlEngine::Postgres => format!("psql -d {db} -f {file}"),
            SqlEngine::Mysql if sql.transaction => {
                format!("mysql {db} < {file} (in one transaction)")
            }
            SqlEngine::Mysql => format!("mysql {db} < {file}"),
        };
    }
    let mut parts = vec![
        p.interpreter.to_string(),
        format!("scripts/{}", p.file_name),
    ];
    parts.extend(p.args.iter().map(|a| sh_word(a)));
    if p.input.is_some() && p.input_arg {
        parts.push(format!("scripts/{}", input_name(p.name)));
    }
    parts.join(" ")
}

pub(crate) fn bootstrap(p: &Payload) -> String {
    let vars = p.sql.map(|sql| sql_vars(sql.engine, p.env));
    let delim = delimiter(
        p.checksum,
        &[
            p.body,
            p.input.unwrap_or_default(),
            vars.as_deref().unwrap_or_default(),
        ],
    );
    let mut out = String::from("set -e\numask 077\n");
    if p.workdir == WorkDir::Temp {
        out.push_str("__ikit_dir=$(mktemp -d \"${TMPDIR:-/tmp}/ikit-script.XXXXXX\")\n");
        out.push_str("cd \"$__ikit_dir\"\n");
    }
    out.push_str("mkdir -p scripts\n");
    heredoc(
        &mut out,
        &sh(&format!("scripts/{}", p.file_name)),
        p.body,
        &delim,
    );
    if let (Some(sql), Some(vars)) = (p.sql, &vars) {
        let vars_file = format!("scripts/{}.vars.sql", p.name);
        let script_file = format!("scripts/{}", p.file_name);
        heredoc(&mut out, &sh(&vars_file), vars, &delim);
        out.push_str("set +e\n");
        out.push_str(&sql_command(sql, &script_file, &vars_file));
        out.push_str("\n__ikit_rc=$?\n");
        out.push_str(&format!("rm -f {}\n", sh(&vars_file)));
        if p.workdir == WorkDir::Temp {
            out.push_str("cd / && rm -rf \"$__ikit_dir\"\n");
        }
        out.push_str("exit $__ikit_rc\n");
        return out;
    }
    let input = input_name(p.name);
    if let Some(content) = p.input {
        heredoc(&mut out, &sh(&format!("scripts/{input}")), content, &delim);
    }
    for (k, v) in p.env {
        out.push_str(&format!("export {k}={}\n", sh(v)));
    }
    if p.input.is_some() {
        out.push_str(&format!(
            "export IKIT_INPUT=\"$PWD\"/{}\n",
            sh(&format!("scripts/{input}"))
        ));
    }
    out.push_str("set +e\n");
    let mut cmd = vec![
        p.interpreter.to_string(),
        sh(&format!("scripts/{}", p.file_name)),
    ];
    cmd.extend(p.args.iter().map(|a| sh_word(a)));
    if p.input.is_some() && p.input_arg {
        cmd.push("\"$IKIT_INPUT\"".to_owned());
    }
    out.push_str(&format!("{} </dev/null\n", cmd.join(" ")));
    out.push_str("__ikit_rc=$?\n");
    if p.input.is_some() {
        out.push_str(&format!("rm -f {}\n", sh(&format!("scripts/{input}"))));
    }
    if p.workdir == WorkDir::Temp {
        out.push_str("cd / && rm -rf \"$__ikit_dir\"\n");
    }
    out.push_str("exit $__ikit_rc\n");
    out
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::*;

    fn payload<'a>(
        body: &'a str,
        input: Option<&'a str>,
        env: &'a [(String, String)],
        args: &'a [String],
        workdir: WorkDir,
    ) -> Payload<'a> {
        Payload {
            name: "demo",
            file_name: "demo.sh",
            interpreter: Interpreter::Sh,
            body,
            checksum: "0123456789abcdef",
            input,
            input_arg: true,
            env,
            args,
            workdir,
            sql: None,
        }
    }

    /// Runs the program with the real `sh`, in `dir`.
    fn run(program: &str, dir: &std::path::Path) -> (i32, String) {
        let mut child = Command::new("sh")
            .arg("-s")
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(program.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr),
        )
    }

    #[test]
    fn quotes_sql_variables() {
        let env = vec![
            ("PW".to_owned(), "it's a \\ $x\nnext".to_owned()),
            ("DB".to_owned(), "core".to_owned()),
        ];
        assert_eq!(
            sql_vars(SqlEngine::Postgres, &env),
            "\\set PW 'it''s a \\\\ $x\\nnext'\n\\set DB 'core'\n"
        );
        assert_eq!(
            sql_vars(SqlEngine::Mysql, &env),
            "SET @PW = 'it\\'s a \\\\ $x\nnext';\nSET @DB = 'core';\n"
        );
    }

    #[test]
    fn sql_scripts_run_the_client_on_a_private_vars_file() {
        let env = vec![("PW".to_owned(), "s3cret".to_owned())];
        let sql = SqlRun {
            engine: SqlEngine::Postgres,
            database: Some("core".into()),
            transaction: true,
        };
        let mut p = payload("SELECT :'PW';\n", None, &env, &[], WorkDir::Temp);
        p.file_name = "demo.sql";
        p.interpreter = Interpreter::Sql;
        p.sql = Some(&sql);
        let program = bootstrap(&p);
        assert!(program.contains("\\set PW 's3cret'\n"), "{program}");
        assert!(!program.contains("export PW"), "{program}");
        assert!(
            program.contains("psql -X -q -v ON_ERROR_STOP=1 --single-transaction -U \"${POSTGRES_USER:-postgres}\" -d 'core' -f 'scripts/demo.vars.sql' -f 'scripts/demo.sql' </dev/null"),
            "{program}"
        );
        assert!(program.contains("rm -f 'scripts/demo.vars.sql'"));
        assert_eq!(
            command_line(&p),
            "psql -d core --single-transaction -f scripts/demo.sql"
        );
    }

    /// A fake `psql` on PATH that prints its args and the files it reads, run by the real
    /// shell.
    #[test]
    fn sql_programs_run_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let fake = bin.join("psql");
        std::fs::write(
            &fake,
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do if [ \"$1\" = -f ]; then cat \"$2\"; shift; fi; shift; done\nexit 4\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = vec![("PW".to_owned(), "s3cret".to_owned())];
        let sql = SqlRun {
            engine: SqlEngine::Postgres,
            database: None,
            transaction: false,
        };
        let mut p = payload("SELECT :'PW';\n", None, &env, &[], WorkDir::Here);
        p.file_name = "demo.sql";
        p.interpreter = Interpreter::Sql;
        p.sql = Some(&sql);
        let program = format!("PATH={}:$PATH\n{}", bin.display(), bootstrap(&p));
        let (code, out) = run(&program, dir.path());
        assert_eq!(code, 4, "the client's exit code is kept: {out}");
        assert_eq!(out, "\\set PW 's3cret'\nSELECT :'PW';\n");
        assert!(!dir.path().join("scripts/demo.vars.sql").exists());
        assert!(dir.path().join("scripts/demo.sql").exists());
    }

    #[test]
    fn delimiters_avoid_the_content() {
        let d = delimiter("abcdef0123", &["x\n__IKIT_EOF_abcdef01__\n"]);
        assert_eq!(d, "__IKIT_EOF_abcdef01___");
    }

    #[test]
    fn runs_the_body_with_env_args_and_input() {
        let dir = tempfile::tempdir().unwrap();
        let body = "#!/bin/sh\n# $NOT_EXPANDED `nor this`\necho \"pw=$PGPASS\"\necho \"args=$*\"\ncat \"$IKIT_INPUT\"\nread line && echo \"stdin=$line\"\nexit 3\n";
        let env = vec![
            ("PGPASS".to_owned(), "it's $ecret".to_owned()),
            ("IKIT_WORKLOAD".to_owned(), "wl-acme-dev-001".to_owned()),
        ];
        let args = vec!["-y".to_owned(), "two words".to_owned()];
        let input = "db_name,db_user,db_password\ncore,core,pw\n";
        let p = payload(body, Some(input), &env, &args, WorkDir::Here);
        let program = bootstrap(&p);
        assert!(
            program.contains("export PGPASS='it'\\''s $ecret'\n"),
            "{program}"
        );
        let (code, out) = run(&program, dir.path());
        assert_eq!(code, 3, "{out}");
        assert!(out.contains("pw=it's $ecret"), "{out}");
        assert!(out.contains("args=-y two words "), "{out}");
        assert!(out.contains("core,core,pw"), "{out}");
        assert!(!out.contains("stdin="), "stdin is /dev/null: {out}");
        // The body stays for reference, unchanged; the input (with secrets) is gone.
        let kept = std::fs::read_to_string(dir.path().join("scripts/demo.sh")).unwrap();
        assert_eq!(kept, body);
        assert!(!dir.path().join("scripts/demo.input").exists());
        assert_eq!(
            command_line(&p),
            "sh scripts/demo.sh -y 'two words' scripts/demo.input"
        );
    }

    #[test]
    fn temp_workdirs_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let body = "pwd > \"$OUT\"\n";
        let out_file = dir.path().join("where");
        let env = vec![("OUT".to_owned(), out_file.display().to_string())];
        let program = bootstrap(&payload(body, None, &env, &[], WorkDir::Temp));
        let (code, out) = run(&program, dir.path());
        assert_eq!(code, 0, "{out}");
        let used = std::fs::read_to_string(&out_file).unwrap();
        assert!(used.contains("ikit-script."), "{used}");
        assert!(!std::path::Path::new(used.trim()).exists());
        assert!(!dir.path().join("scripts").exists());
    }
}
