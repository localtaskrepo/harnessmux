//! Permission rules: commands, tools and write locations a session refuses or allows, decided before the policy.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Rules applied to each permission request before the policy ([`crate::Profile::policy`]). Agents whose table entry
/// has `rules_meta` also get the tool lists and commands natively, for the tool calls they run without asking.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rules {
    /// Shell commands refused: a command line that runs one of these is denied, e.g. `git push`. The line is read as
    /// a shell reads it (quotes, `&&`, `;`, `|`, `$(…)`, `sh -c "…"`), with the program's options skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_commands: Vec<String>,
    /// Tools allowed without the policy, by the name the agent gives them; a trailing `*` matches a prefix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_tools: Vec<String>,
    /// Tools refused, matched like `allow_tools`; refusing wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_tools: Vec<String>,
    /// Refuse edits, deletes and moves of paths outside the working directory and `add_dirs`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confine_writes: bool,
}

impl Rules {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// `Some((allow, why))` when a rule decides the request (the `requestPermission` params as JSON); `None` leaves it
    /// to the policy. `roots` are the working directory and the extra directories.
    pub fn decide(&self, request: &Value, roots: &[PathBuf]) -> Option<(bool, String)> {
        let call = request.get("toolCall").unwrap_or(&Value::Null);
        if let Some(command) = command(call)
            && let Some(denied) = self.deny_commands.iter().find(|denied| runs(&command, denied))
        {
            return Some((false, format!("rule deny_commands {denied}")));
        }
        let tool = tool_name(call);
        if let Some(pattern) = tool.as_deref().and_then(|tool| self.deny_tools.iter().find(|pattern| matches(pattern, tool))) {
            return Some((false, format!("rule deny_tools {pattern}")));
        }
        if self.confine_writes && writes(call) {
            let cwd = roots.first().map(PathBuf::as_path).unwrap_or(Path::new("/"));
            if let Some(outside) =
                paths(call).into_iter().map(|path| resolve(cwd, &path)).find(|path| !roots.iter().any(|root| path.starts_with(resolve(cwd, root))))
            {
                return Some((false, format!("rule confine_writes: {} is outside the session's directories", outside.display())));
            }
        }
        if let Some(pattern) = tool.as_deref().and_then(|tool| self.allow_tools.iter().find(|pattern| matches(pattern, tool))) {
            return Some((true, format!("rule allow_tools {pattern}")));
        }
        None
    }
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Whether one of the commands in `line` runs `denied`: its program (by file name, after variable assignments and
/// wrappers such as `env` or `sudo`), then its other words with the program's options skipped, so `git -C . push` and
/// `git -c a=b push` run `git push`. The line is read as a shell reads it ([`Shell`]), so a quoted word stays one word
/// (`git -C "/tmp/my repo" push`); commands in `sh -c "…"`, `eval` and `$(…)` count too. What only the shell knows
/// when it runs the line (a program or subcommand from a variable, an alias, a script piped into `sh`) isn't seen.
fn runs(command: &Command, denied: &str) -> bool {
    let denied: Vec<&str> = denied.split_whitespace().collect();
    match command {
        Command::Line(line) => line_runs(line, &denied, 0),
        Command::Args(words) => command_runs(words, &denied, 0),
    }
}

/// Scripts nested deeper than this (`sh -c "sh -c …"`) aren't read.
const MAX_DEPTH: usize = 8;

fn line_runs(line: &str, denied: &[&str], depth: usize) -> bool {
    depth <= MAX_DEPTH && Shell::commands(line).iter().any(|words| command_runs(words, denied, depth))
}

/// Options that take the next word as their value (git's global options), skipped with it.
const VALUE_OPTIONS: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path", "--config-env", "--super-prefix"];
/// Programs that run the command in their arguments. Their own options aren't known, so the command may start at any
/// later word.
const WRAPPERS: &[&str] =
    &["env", "command", "builtin", "exec", "nohup", "sudo", "doas", "time", "nice", "ionice", "timeout", "stdbuf", "setsid", "caffeinate", "xargs", "noglob"];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];
/// Shell words before a command that aren't its program.
const RESERVED: &[&str] = &["{", "}", "!", "if", "then", "elif", "else", "while", "until", "do"];

/// Whether `word` names `program`: by file name, without `.exe`. A backslash before a letter is kept by [`Shell`]
/// (a Windows path), so the word also counts with its backslashes removed, as a POSIX shell reads `g\it`.
fn names(word: &str, program: &str) -> bool {
    let file = |word: &str| {
        let file = word.rsplit(['/', '\\']).next().unwrap_or(word);
        file.strip_suffix(".exe").unwrap_or(file) == program
    };
    file(word) || file(&word.replace('\\', ""))
}

fn command_runs(words: &[String], denied: &[&str], depth: usize) -> bool {
    let Some((program, rest)) = denied.split_first() else { return false };
    let assignment = |word: &str| word.split_once('=').is_some_and(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_'));
    let start = words.iter().position(|word| !RESERVED.contains(&word.as_str()) && !assignment(word)).unwrap_or(words.len());
    let Some((first, args)) = words[start..].split_first() else { return false };
    if WRAPPERS.iter().any(|wrapper| names(first, wrapper)) {
        return (0..args.len()).any(|at| command_runs(&args[at..], denied, depth));
    }
    if SHELLS.iter().any(|shell| names(first, shell)) {
        let flag = args.iter().position(|word| word.starts_with('-') && !word.starts_with("--") && word.contains('c'));
        return flag.and_then(|flag| args.get(flag + 1)).is_some_and(|script| line_runs(script, denied, depth + 1));
    }
    if first == "eval" {
        return line_runs(&args.join(" "), denied, depth + 1);
    }
    if !names(first, program) {
        return false;
    }
    let mut args = args.iter();
    let mut expected = rest.iter();
    let Some(mut want) = expected.next() else { return true };
    while let Some(word) = args.next() {
        if word.starts_with('-') {
            if VALUE_OPTIONS.contains(&word.as_str()) {
                args.next();
            }
            continue;
        }
        if word != want && word.replace('\\', "") != *want {
            return false;
        }
        match expected.next() {
            Some(next) => want = next,
            None => return true,
        }
    }
    false
}

/// What follows a redirection operator: its target word, which isn't a word of the command.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Redirect {
    None,
    /// A file or descriptor.
    Target,
    /// The text of a here-string (`<<<`): a script when the command is a shell, so it is read as one.
    Script,
}

/// Reads a command line into its commands, each a list of words, the way a POSIX shell splits it: quotes, backslashes,
/// `;` `&` `|` and newlines, subshells, `$(…)`, backticks, `<(…)`, redirections and comments. Words keep the text of
/// variables (`$HOME`); a substitution leaves `$` in its word and its commands are listed as well. An unclosed quote
/// runs to the end of the line.
struct Shell {
    chars: Vec<char>,
    at: usize,
    commands: Vec<Vec<String>>,
}

impl Shell {
    fn commands(line: &str) -> Vec<Vec<String>> {
        let mut shell = Self { chars: line.chars().collect(), at: 0, commands: Vec::new() };
        shell.list(None);
        shell.commands
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn next(&mut self) -> Option<char> {
        let next = self.peek()?;
        self.at += 1;
        Some(next)
    }

    /// The commands up to `end` (the `)` or backtick that closes a substitution or subshell), or to the end.
    fn list(&mut self, end: Option<char>) {
        let mut words: Vec<String> = Vec::new();
        let mut word: Option<String> = None;
        let mut redirect = Redirect::None;
        while let Some(c) = self.next() {
            match c {
                // A `)` that closes nothing separates commands (a `case` pattern).
                ')' | '`' if end == Some(c) || c == ')' => {
                    self.end_word(&mut words, &mut word, &mut redirect);
                    self.end_command(&mut words);
                    if end == Some(c) {
                        return;
                    }
                }
                '`' => {
                    self.list(Some('`'));
                    word.get_or_insert_default().push('$');
                }
                ' ' | '\t' | '\r' => self.end_word(&mut words, &mut word, &mut redirect),
                '&' if self.peek() == Some('>') => {}
                '\n' | ';' | '&' | '|' => {
                    self.end_word(&mut words, &mut word, &mut redirect);
                    self.end_command(&mut words);
                }
                '(' => {
                    self.end_word(&mut words, &mut word, &mut redirect);
                    self.end_command(&mut words);
                    self.list(Some(')'));
                }
                '#' if word.is_none() => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.at += 1;
                    }
                }
                '<' | '>' => {
                    // A descriptor number before the operator (`2>`) belongs to it.
                    if word.as_deref().is_some_and(|word| word.chars().all(|c| c.is_ascii_digit())) {
                        word = None;
                    }
                    self.end_word(&mut words, &mut word, &mut redirect);
                    let mut operator = String::from(c);
                    while let Some(more) = self.peek().filter(|more| matches!(more, '<' | '>' | '&' | '|' | '-')) {
                        operator.push(more);
                        self.at += 1;
                    }
                    if operator.len() == 1 && self.peek() == Some('(') {
                        self.at += 1;
                        self.list(Some(')'));
                        word.get_or_insert_default().push('$');
                    } else {
                        redirect = if operator == "<<<" { Redirect::Script } else { Redirect::Target };
                    }
                }
                '\'' => {
                    let word = word.get_or_insert_default();
                    while let Some(c) = self.next().filter(|c| *c != '\'') {
                        word.push(c);
                    }
                }
                '"' => self.double_quoted(word.get_or_insert_default()),
                '\\' => match self.next() {
                    // A line continuation.
                    Some('\n') => {}
                    // Kept: a Windows path (`C:\tools\git.exe`); `names` reads it both ways.
                    Some(c) if c.is_alphanumeric() => word.get_or_insert_default().extend(['\\', c]),
                    Some(c) => word.get_or_insert_default().push(c),
                    None => word.get_or_insert_default().push('\\'),
                },
                '$' => self.dollar(word.get_or_insert_default()),
                c => word.get_or_insert_default().push(c),
            }
        }
        self.end_word(&mut words, &mut word, &mut redirect);
        self.end_command(&mut words);
    }

    fn end_word(&mut self, words: &mut Vec<String>, word: &mut Option<String>, redirect: &mut Redirect) {
        let Some(word) = word.take() else { return };
        match std::mem::replace(redirect, Redirect::None) {
            Redirect::None => words.push(word),
            Redirect::Target => {}
            Redirect::Script => self.commands.extend(Self::commands(&word)),
        }
    }

    fn end_command(&mut self, words: &mut Vec<String>) {
        if !words.is_empty() {
            self.commands.push(std::mem::take(words));
        }
    }

    /// The rest of a `"…"` word: only `\` before `$`, a backtick, `"`, `\` or a newline escapes, and substitutions run.
    fn double_quoted(&mut self, word: &mut String) {
        while let Some(c) = self.next() {
            match c {
                '"' => return,
                '\\' => match self.next() {
                    Some('\n') => {}
                    Some(c) if matches!(c, '$' | '`' | '"' | '\\') => word.push(c),
                    Some(c) => word.extend(['\\', c]),
                    None => word.push('\\'),
                },
                '`' => {
                    self.list(Some('`'));
                    word.push('$');
                }
                '$' => self.dollar(word),
                c => word.push(c),
            }
        }
    }

    /// What follows a `$`: `$(…)`, `$'…'`, `$"…"`, `${…}` or a variable's name, which stays in the word as text.
    fn dollar(&mut self, word: &mut String) {
        match self.peek() {
            Some('(') => {
                self.at += 1;
                self.list(Some(')'));
                word.push('$');
            }
            Some('\'') => {
                self.at += 1;
                while let Some(c) = self.next().filter(|c| *c != '\'') {
                    match c {
                        '\\' => match self.next() {
                            Some('n') => word.push('\n'),
                            Some('t') => word.push('\t'),
                            Some(c) => word.push(c),
                            None => {}
                        },
                        c => word.push(c),
                    }
                }
            }
            Some('"') => {
                self.at += 1;
                self.double_quoted(word);
            }
            Some('{') => {
                word.push('$');
                while let Some(c) = self.next() {
                    word.push(c);
                    if c == '}' {
                        break;
                    }
                }
            }
            _ => word.push('$'),
        }
    }
}

/// The shell command of an execute request: `rawInput.command`, a line for the shell or an argument list.
enum Command {
    Line(String),
    Args(Vec<String>),
}

fn command(call: &Value) -> Option<Command> {
    match call.pointer("/rawInput/command")? {
        Value::String(line) => Some(Command::Line(line.clone())),
        Value::Array(args) => Some(Command::Args(args.iter().filter_map(Value::as_str).map(str::to_string).collect())),
        _ => None,
    }
}

/// The agent's own tool name (`_meta.<agent>.toolName`, as the Claude adapter sends it), else the title.
fn tool_name(call: &Value) -> Option<String> {
    let from_meta = call.get("_meta").and_then(Value::as_object).and_then(|meta| meta.values().find_map(|agent| agent.get("toolName")?.as_str()));
    from_meta.or_else(|| call.get("title")?.as_str()).map(str::to_string)
}

fn writes(call: &Value) -> bool {
    matches!(call.get("kind").and_then(Value::as_str), Some("edit" | "delete" | "move"))
}

/// The paths a request touches: its locations and the usual path fields of its input.
fn paths(call: &Value) -> Vec<PathBuf> {
    let locations = call.get("locations").and_then(Value::as_array).into_iter().flatten().filter_map(|location| location.get("path")?.as_str());
    let input = ["file_path", "path", "notebook_path", "destination"].into_iter().filter_map(|key| call.get("rawInput")?.get(key)?.as_str());
    locations.chain(input).map(PathBuf::from).collect()
}

/// `path` made absolute against `cwd`, with `.` and `..` removed and the longest existing ancestor's symlinks resolved.
fn resolve(cwd: &Path, path: &Path) -> PathBuf {
    let mut absolute = PathBuf::new();
    for component in cwd.join(path).components() {
        match component {
            Component::ParentDir => {
                absolute.pop();
            }
            Component::CurDir => {}
            other => absolute.push(other),
        }
    }
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else { return absolute };
        rest.push(name.to_os_string());
        existing = parent;
    }
    let mut resolved = existing.canonicalize().unwrap_or_else(|_| existing.to_path_buf());
    resolved.extend(rest.iter().rev());
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules() -> Rules {
        Rules { deny_commands: vec!["git push".into()], allow_tools: vec!["mcp__lotar__*".into()], deny_tools: vec!["WebFetch".into()], confine_writes: true }
    }

    fn bash(command: &str) -> Value {
        json!({"toolCall": {"kind": "execute", "title": command, "rawInput": {"command": command}}})
    }

    #[test]
    fn denied_commands_match_any_command_of_a_line() {
        let roots = [PathBuf::from("/work/repo")];
        for line in ["git push", "git push origin main", "cd x && git push", "FOO=1 git push -f", "make; git   push"] {
            assert_eq!(rules().decide(&bash(line), &roots).map(|(allow, _)| allow), Some(false), "{line}");
        }
        for line in [
            "git -C . push",
            "git -c credential.helper= push origin",
            "git --git-dir=.git --work-tree . push",
            "/usr/bin/git push",
            "env GIT_TRACE=1 git push",
            "sudo -u me git push",
            "sh -c 'git push origin main'",
            "bash -lc \"cd x && git -C y push\"",
            "echo $(git push)",
        ] {
            assert_eq!(rules().decide(&bash(line), &roots).map(|(allow, _)| allow), Some(false), "{line}");
        }
        for line in ["git pushd", "echo git push", "git status", "git -C push status", "git log --grep push"] {
            assert_eq!(rules().decide(&bash(line), &roots), None, "{line}");
        }
        let args = json!({"toolCall": {"kind": "execute", "rawInput": {"command": ["git", "push"]}}});
        assert_eq!(rules().decide(&args, &roots).map(|(allow, _)| allow), Some(false));
    }

    #[test]
    fn denied_commands_are_read_as_the_shell_reads_them() {
        let roots = [PathBuf::from("/work/repo")];
        for line in [
            r#"git -C "/tmp/my repo" push"#,
            "git -C '/tmp/my repo' push origin main",
            r"git -C /tmp/my\ repo push",
            r#"git -c "user.name=A B" push"#,
            r#"git --git-dir "/tmp/my repo/.git" --work-tree="/tmp/my repo" push"#,
            r#""git" "push""#,
            "g'i't pu\"s\"h",
            r"\git push",
            r"g\it pu\sh",
            "git \\\n  push",
            r#""/opt/my tools/git" push"#,
            r"C:\tools\git.exe push",
            r#"GIT_DIR="/tmp/my repo/.git" git push"#,
            r#"echo "a ; b" && git push"#,
            "echo 'it''s' | git push",
            r#"echo "$(git -C "/tmp/my repo" push)""#,
            r#"echo "`git push`""#,
            r#"bash -c "git -C \"/tmp/my repo\" push""#,
            "sh -c 'cd \"/tmp/my repo\" && git push'",
            "bash --noprofile -e -c 'git push'",
            r#"eval "git push""#,
            "bash <<< 'git push'",
            "(cd x; git push)",
            "{ git push; }",
            "if true; then git push; fi",
            "! git push",
            "git push 2>&1 | tee log",
            "git 2>&1 push",
            "git >'/tmp/my log' push",
            "&>/dev/null git push",
            "git -C x push&",
            "nice -n 10 git push",
            "timeout 30 git push",
            "env -u GIT_DIR git push",
            "sudo -u 'some one' git push",
            "cat <(git push)",
            "true # no\ngit push",
            "git $'push'",
        ] {
            assert_eq!(rules().decide(&bash(line), &roots).map(|(allow, _)| allow), Some(false), "{line}");
        }
        for line in [
            r#"git commit -m "then git push""#,
            "git commit -m 'a; git push'",
            r#"echo "git push""#,
            r"echo a \; git push",
            "git status # git push",
            r#"git -C "/tmp/push it" status"#,
            "git log > push",
            r#"grep "sh -c 'git push'" notes.md"#,
            "git stash push",
            r#"git commit -m "unterminated"#,
        ] {
            assert_eq!(rules().decide(&bash(line), &roots), None, "{line}");
        }
        let args = json!({"toolCall": {"kind": "execute", "rawInput": {"command": ["git", "-C", "/tmp/my repo", "push"]}}});
        assert_eq!(rules().decide(&args, &roots).map(|(allow, _)| allow), Some(false));
        let args = json!({"toolCall": {"kind": "execute", "rawInput": {"command": ["bash", "-lc", "git -C '/tmp/my repo' push"]}}});
        assert_eq!(rules().decide(&args, &roots).map(|(allow, _)| allow), Some(false));
        let args = json!({"toolCall": {"kind": "execute", "rawInput": {"command": ["git", "commit", "-m", "git push"]}}});
        assert_eq!(rules().decide(&args, &roots), None);
    }

    #[test]
    fn tools_match_by_the_agents_name_and_refusing_wins() {
        let roots = [PathBuf::from("/work/repo")];
        let mcp = json!({"toolCall": {"kind": "other", "title": "lotar: list", "_meta": {"claudeCode": {"toolName": "mcp__lotar__list"}}}});
        assert_eq!(rules().decide(&mcp, &roots), Some((true, "rule allow_tools mcp__lotar__*".into())));
        let fetch = json!({"toolCall": {"kind": "fetch", "title": "WebFetch"}});
        assert_eq!(rules().decide(&fetch, &roots), Some((false, "rule deny_tools WebFetch".into())));
        assert_eq!(rules().decide(&json!({"toolCall": {"kind": "read", "title": "Read"}}), &roots), None);
    }

    #[test]
    fn writes_outside_the_directories_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let extra = dir.path().join("shared");
        std::fs::create_dir_all(&cwd).unwrap();
        let roots = [cwd.clone(), extra.clone()];
        let edit = |path: &str| json!({"toolCall": {"kind": "edit", "title": "Edit", "locations": [{"path": path}]}});
        assert_eq!(rules().decide(&edit("src/a.rs"), &roots), None);
        assert_eq!(rules().decide(&edit(&extra.join("x").display().to_string()), &roots), None);
        let (allow, why) = rules().decide(&edit("../elsewhere/a.rs"), &roots).unwrap();
        assert!(!allow && why.contains("elsewhere"), "{why}");
        let raw = json!({"toolCall": {"kind": "delete", "rawInput": {"file_path": "/etc/hosts"}}});
        assert_eq!(rules().decide(&raw, &roots).map(|(allow, _)| allow), Some(false));
        let read = json!({"toolCall": {"kind": "read", "locations": [{"path": "/etc/hosts"}]}});
        assert_eq!(rules().decide(&read, &roots), None, "reads are not confined");
    }
}
