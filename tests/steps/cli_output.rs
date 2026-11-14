//! Steps for `features/cli/output.feature`: the real `ladderback`, run with
//! the command line and the environment a scenario chooses, and what it
//! printed read back as bytes: as JSON, for `--json`, and for the control
//! characters a terminal acts on, for colour and redraws. A command runs as
//! the interop scenarios' do (`interop_steps::run`: the scratch folders and
//! nothing of the developer's environment), and the daemon on folders of its
//! own (`testkit::daemon::isolate`).

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cucumber::gherkin::Step;
use cucumber::{given, then, when};
use serde_json::Value;
use testkit::children::Owned;

use super::interop_steps::{Ran, pipe_without_reader, ran, run, said};
use super::{Agent, LADDERBACK, blocking};

/// How long a freshly started daemon may take to listen or give up.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// What the daemon says once it is up, in English.
const LISTENING: &str = "listening on http://";

#[given("colour is forced on")]
fn colour_is_forced(agent: &mut Agent) {
    agent
        .interop
        .env
        .push(("CLICOLOR_FORCE".to_owned(), "1".to_owned()));
}

#[given("nobody reads standard output")]
fn nobody_reads_stdout(agent: &mut Agent) {
    agent.interop.stdout_unread = true;
}

#[given("nobody reads standard error")]
fn nobody_reads_stderr(agent: &mut Agent) {
    agent.interop.stderr_unread = true;
}

#[given("standard output is not open")]
fn stdout_is_not_open(agent: &mut Agent) {
    agent.interop.stdout_not_open = true;
}

#[given("NO_COLOR is set")]
fn no_colour_is_set(agent: &mut Agent) {
    agent
        .interop
        .env
        .push(("NO_COLOR".to_owned(), "1".to_owned()));
}

/// An empty value is as good as none for the two variables that name the
/// folder (`platform::dirs`). The runner sets both before a scenario's
/// variables, so these replace them.
#[given("there is no home folder and no configuration folder is named")]
fn no_folder_for_the_configuration(agent: &mut Agent) {
    for name in ["HOME", "LADDERBACK_CONFIG_DIR"] {
        agent.interop.env.push((name.to_owned(), String::new()));
    }
}

#[given(expr = "the environment sets {string} to {string}")]
fn environment_sets(agent: &mut Agent, name: String, value: String) {
    agent.interop.env.push((name, value));
}

#[when(expr = "the owner runs {string}")]
async fn runs(agent: &mut Agent, command_line: String) {
    let args: Vec<&str> = command_line.split_whitespace().collect();
    run(agent, &args).await;
}

#[when(expr = "the daemon is started with {string}")]
async fn daemon_started(agent: &mut Agent, options: String) {
    let scratch = agent.scratch().to_owned();
    let env = agent.interop.env.clone();
    // A daemon whose log has no reader ends by itself.
    if agent.interop.stderr_unread {
        let ran = blocking(move || serve_without_a_reader(&scratch, &options, &env)).await;
        agent.interop.ran = Some(ran);
        return;
    }
    let log = blocking(move || serve_until_listening(&scratch, &options, &env)).await;
    // A daemon prints nothing on standard output, and is stopped, not ended.
    agent.interop.ran = Some(Ran {
        success: false,
        code: None,
        stdout: String::new(),
        stderr: log,
    });
}

/// `ladderback serve` with `options`, on folders of its own and with the
/// environment a scenario chose, and nothing of the developer's.
fn daemon_command(scratch: &Path, options: &str, env: &[(String, String)]) -> Command {
    let mut command = Command::new(LADDERBACK);
    command
        .env_clear()
        .env("HOME", scratch)
        .env("LANGUAGE", "en-US");
    // Folders of its own, not the platform's.
    testkit::daemon::isolate(&mut command, &scratch.join("daemon"));
    for (name, value) in env {
        command.env(name, value);
    }
    command
        .arg("serve")
        .args(options.split_whitespace())
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    command
}

/// Starts `ladderback serve` with `options`, waits until it listens, stops
/// it, and returns what it logged. Runs on a blocking thread: see
/// [`blocking`].
fn serve_until_listening(scratch: &Path, options: &str, env: &[(String, String)]) -> String {
    let log_path = scratch.join("serve-output.log");
    let log = File::create(&log_path).expect("a log file for the daemon");
    let mut command = daemon_command(scratch, options, env);
    command.stderr(log);
    let child = Owned::spawn(&mut command).expect("ladderback starts");
    let began = Instant::now();
    loop {
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        // The log's last line may be half written: wait for its newline.
        if text
            .split(LISTENING)
            .nth(1)
            .is_some_and(|rest| rest.contains('\n'))
        {
            drop(child);
            return std::fs::read_to_string(&log_path).unwrap_or(text);
        }
        if let Some(status) = child.try_wait().expect("the daemon's status") {
            // Read again: it may have written its last words and exited
            // between the read above and this wait.
            let text = std::fs::read_to_string(&log_path).unwrap_or(text);
            panic!("ladderback exited with {status} before it listened: {text}");
        }
        if began.elapsed() >= START_TIMEOUT {
            panic!("ladderback neither listened nor exited within {START_TIMEOUT:?}: {text}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Starts `ladderback serve` with `options` and a standard error that nothing
/// can be written to, and waits for it to end by itself. Runs on a blocking
/// thread: see [`blocking`].
fn serve_without_a_reader(scratch: &Path, options: &str, env: &[(String, String)]) -> Ran {
    let mut command = daemon_command(scratch, options, env);
    command.stderr(pipe_without_reader());
    let child = Owned::spawn(&mut command).expect("ladderback starts");
    let began = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("the daemon's status") {
            return Ran {
                success: status.success(),
                code: status.code(),
                stdout: String::new(),
                stderr: String::new(),
            };
        }
        if began.elapsed() >= START_TIMEOUT {
            panic!(
                "ladderback was still running {START_TIMEOUT:?} after nothing could read its log"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[then(expr = "the command exits with status {int}")]
fn exits_with(agent: &mut Agent, status: i32) {
    let ran = ran(agent);
    assert_eq!(
        ran.code,
        Some(status),
        "standard output {:?}, standard error {:?}",
        ran.stdout,
        ran.stderr
    );
}

/// The one JSON document standard output holds: an object on a single line,
/// ended by its newline.
fn document(agent: &Agent) -> Value {
    let stdout = &ran(agent).stdout;
    let Some(line) = stdout.strip_suffix('\n') else {
        panic!("standard output doesn't end with a newline: {stdout:?}")
    };
    assert!(
        !line.contains('\n'),
        "standard output is more than one line: {stdout:?}"
    );
    let document: Value = serde_json::from_str(line)
        .unwrap_or_else(|error| panic!("standard output is not JSON ({error}): {stdout:?}"));
    assert!(document.is_object(), "not a JSON object: {document}");
    document
}

/// The value at `path` of a document. A segment names a key; in an array it
/// is the position of an item.
fn lookup<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(root, |found, segment| match found {
            Value::Object(members) => members.get(segment),
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?),
            _ => None,
        })
}

#[then("standard output is one line holding one JSON document")]
fn one_document(agent: &mut Agent) {
    document(agent);
}

#[then("the document has:")]
fn document_has(agent: &mut Agent, step: &Step) {
    let found = document(agent);
    let table = step.table.as_ref().expect("the step has a table");
    for row in table.rows.iter().skip(1) {
        let [key, value] = row.as_slice() else {
            panic!("a row of a key and a value: {row:?}")
        };
        let expected: Value = serde_json::from_str(value)
            .unwrap_or_else(|error| panic!("{value:?} is not JSON: {error}"));
        assert_eq!(
            lookup(&found, key),
            Some(&expected),
            "the value at {key:?} in {found}"
        );
    }
}

#[then(expr = "the document has no key {string}")]
fn document_lacks(agent: &mut Agent, key: String) {
    let found = document(agent);
    assert_eq!(lookup(&found, &key), None, "a key {key:?} in {found}");
}

#[then(expr = "the document has some text at {string}")]
fn document_has_text(agent: &mut Agent, key: String) {
    let found = document(agent);
    let text = lookup(&found, &key).and_then(Value::as_str);
    assert!(
        text.is_some_and(|text| !text.is_empty()),
        "no text at {key:?} in {found}"
    );
}

#[then("standard error is empty")]
fn stderr_is_empty(agent: &mut Agent) {
    let stderr = &ran(agent).stderr;
    assert!(stderr.is_empty(), "standard error holds {stderr:?}");
}

#[then(expr = "standard error says {string}")]
fn stderr_says(agent: &mut Agent, text: String) {
    let stderr = &ran(agent).stderr;
    assert!(
        stderr.contains(&text),
        "standard error {stderr:?} doesn't say {text:?}"
    );
}

#[then("standard output is empty")]
fn stdout_is_empty(agent: &mut Agent) {
    let stdout = &ran(agent).stdout;
    assert!(stdout.is_empty(), "standard output holds {stdout:?}");
}

#[then(expr = "standard output has {int} line(s)")]
fn stdout_lines(agent: &mut Agent, count: usize) {
    let stdout = &ran(agent).stdout;
    assert_eq!(stdout.lines().count(), count, "standard output {stdout:?}");
}

#[then(expr = "standard error has {int} line(s)")]
fn stderr_lines(agent: &mut Agent, count: usize) {
    let stderr = &ran(agent).stderr;
    assert_eq!(stderr.lines().count(), count, "standard error {stderr:?}");
}

#[then("the output is plain text")]
fn output_is_plain(agent: &mut Agent) {
    let said = said(agent);
    assert!(!said.trim().is_empty(), "the command said nothing");
    // Colour is an escape sequence and a redraw is a carriage return or a
    // cursor sequence: all of them are control characters, and text has none
    // but its line ends.
    let controls: Vec<char> = said
        .chars()
        .filter(|c| c.is_control() && *c != '\n')
        .collect();
    assert!(
        controls.is_empty(),
        "the output holds control characters {controls:?}: {said:?}"
    );
}

#[then("the output has colour")]
fn output_has_colour(agent: &mut Agent) {
    let said = said(agent);
    // The start of a control sequence that sets a style.
    assert!(
        said.contains("\u{1b}["),
        "the output has no colour: {said:?}"
    );
}

#[given("the daemon's config file holds:")]
fn daemon_config_holds(agent: &mut Agent, step: &Step) {
    let text = step.docstring.as_deref().expect("the step has a text");
    let folder = testkit::daemon::config_dir(&agent.scratch().join("daemon"));
    std::fs::create_dir_all(&folder).expect("the daemon's config folder");
    std::fs::write(
        folder.join("config.toml"),
        format!("{}\n", text.trim_start_matches('\n')),
    )
    .expect("the daemon's config file");
}

#[then(
    expr = "the daemon warns that the address in the setting {string} of the repository {string} holds a password"
)]
fn daemon_warns_location(agent: &mut Agent, field: String, alias: String) {
    let log = &ran(agent).stderr;
    let line =
        format!("repository {alias}: The address in the {field} setting holds a password as text");
    assert!(log.contains(&line), "no {line:?} in the log: {log}");
}

#[then(
    expr = "the daemon warns that the address in the setting {string} of location {int} of the repository {string} holds a password"
)]
fn daemon_warns_further_location(agent: &mut Agent, field: String, position: u8, alias: String) {
    let log = &ran(agent).stderr;
    let line = format!(
        "repository {alias}: The address in the {field} setting of location {position} holds a password as text"
    );
    assert!(log.contains(&line), "no {line:?} in the log: {log}");
}

#[then(
    expr = "the daemon warns that the address in the setting {string} of the repository {string} holds a credential that belongs in rclone's own configuration"
)]
fn daemon_warns_rclone_location(agent: &mut Agent, field: String, alias: String) {
    let log = &ran(agent).stderr;
    let warning = format!(
        "repository {alias}: The address in the {field} setting holds a credential as text"
    );
    let line = log
        .lines()
        .find(|line| line.contains(&warning))
        .unwrap_or_else(|| panic!("no {warning:?} in the log: {log}"));
    // rclone is started with the location in its arguments, where a `${…}`
    // reference is refused, so the warning sends the credential to rclone.
    assert!(
        line.contains("rclone's own configuration file")
            && line.contains("rclone:remote:path")
            && !line.contains("${"),
        "{line}"
    );
}

#[then(expr = "the daemon gives no warning about the repository {string}")]
fn daemon_no_warning(agent: &mut Agent, alias: String) {
    let log = &ran(agent).stderr;
    assert!(log.contains(LISTENING), "the daemon didn't start: {log}");
    assert!(!log.contains(&format!("repository {alias}:")), "{log}");
}

#[then(
    expr = "the daemon warns that the glob {string} of the plan {string} matches nothing, and to write {string} instead"
)]
fn daemon_warns_glob(agent: &mut Agent, glob: String, plan: String, whole: String) {
    let log = &ran(agent).stderr;
    let warning = format!("The glob {glob} of the plan {plan} matches nothing");
    let line = log
        .lines()
        .find(|line| line.contains(&warning))
        .unwrap_or_else(|| panic!("no {warning:?} in the log: {log}"));
    assert!(line.contains(&format!("as in {whole}.")), "{line}");
}

#[then(expr = "the daemon gives no warning about the plan {string}")]
fn daemon_no_plan_warning(agent: &mut Agent, plan: String) {
    let log = &ran(agent).stderr;
    assert!(log.contains(LISTENING), "the daemon didn't start: {log}");
    assert!(!log.contains(&format!("of the plan {plan} ")), "{log}");
}

#[then(expr = "the daemon's log never shows {string}")]
fn daemon_log_never_shows(agent: &mut Agent, text: String) {
    let log = &ran(agent).stderr;
    assert!(!log.contains(&text), "{log}");
}

#[then(
    expr = "the daemon warns that the repository {string} sends its credentials over plain HTTP"
)]
fn daemon_warns_plain_http(agent: &mut Agent, alias: String) {
    let log = &ran(agent).stderr;
    let warning =
        format!("repository {alias}: This repository sends its credentials over plain HTTP");
    let line = log
        .lines()
        .find(|line| line.contains(&warning))
        .unwrap_or_else(|| panic!("no {warning:?} in the log: {log}"));
    assert!(line.contains("WARN"), "{line}");
}

/// A repository of one location is listed without its locations, so
/// nothing it is told names one by its position (#473).
#[then("the daemon's log names no location by its position")]
fn daemon_log_names_no_position(agent: &mut Agent) {
    let log = &ran(agent).stderr;
    assert!(log.contains(LISTENING), "the daemon didn't start: {log}");
    assert!(!log.to_lowercase().contains("location 1"), "{log}");
}
