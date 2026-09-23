use super::jsonl::Collector;
use crate::{
    evidence::Change,
    git::{Repo, Target},
};
use std::{io::Write, path::Path, process::Stdio};

struct Pending {
    before: Vec<u8>,
    time: Option<i64>,
    model: Option<String>,
    id: String,
}

pub(super) struct Formatter {
    state: Option<Vec<u8>>,
    pending: Option<Pending>,
}

pub(super) struct CommandRecord<'a> {
    pub command: &'a str,
    pub output: &'a str,
    pub completed: bool,
    pub cwd: Option<&'a str>,
    pub id: Option<&'a str>,
    pub time: Option<i64>,
    pub model: Option<String>,
}

impl Formatter {
    pub fn new(target: &Target) -> Self {
        Self {
            state: Some(target.before.clone()),
            pending: None,
        }
    }

    pub fn edit(&mut self, change: &Change) {
        self.pending = None;
        self.state = self
            .state
            .as_deref()
            .and_then(|before| change.apply(before));
    }

    pub fn command(&mut self, c: &mut Collector<'_>, record: CommandRecord<'_>) {
        let cwd_matches = record
            .cwd
            .is_none_or(|cwd| Path::new(cwd.strip_prefix("file://").unwrap_or(cwd)) == c.cwd);
        if !record.completed || !cwd_matches {
            self.pending = None;
            self.state = None;
            return;
        }
        if formatter_and_diff(record.command).is_some() {
            let verified = if let (Some(before), Some(id)) = (self.state.as_ref(), record.id) {
                self.prove(
                    c,
                    before.clone(),
                    record.output,
                    id,
                    record.time,
                    record.model,
                )
            } else {
                false
            };
            // The combined command's output is its only observation.
            self.pending = None;
            // A command may have changed the file even when the captured diff
            // is incomplete. Do not carry an unverified preimage forward.
            if !verified {
                self.state = None;
            }
            return;
        }
        if is_diff_observation(record.command) {
            if let Some(pending) = self.pending.take()
                && !self.prove(
                    c,
                    pending.before,
                    record.output,
                    &pending.id,
                    pending.time,
                    pending.model,
                )
            {
                self.state = None;
            }
            return;
        }
        if self.pending.take().is_some() {
            self.state = None;
        }
        if is_formatter(record.command) {
            self.pending = self
                .state
                .as_ref()
                .zip(record.id)
                .map(|(before, id)| Pending {
                    before: before.clone(),
                    time: record.time,
                    model: record.model,
                    id: id.into(),
                });
        } else if !matches!(
            record.command,
            "git status" | "git status --short" | "git diff --check"
        ) {
            self.state = None;
        }
    }

    fn prove(
        &mut self,
        c: &mut Collector<'_>,
        before: Vec<u8>,
        output: &str,
        id: &str,
        time: Option<i64>,
        model: Option<String>,
    ) -> bool {
        let Some((path, base, after)) = observed_diff(c.repo, c.target, output) else {
            return false;
        };
        if before != base || before == after {
            return false;
        }
        if c.push(
            &path,
            Change::Snapshot {
                before,
                after: after.clone(),
            },
            time,
            Some(id),
            model,
        ) {
            self.state = Some(after);
            true
        } else {
            false
        }
    }
}

// Only inspect output from a simple, read-only Git command sequence. Shell
// history is data: never replay its commands to establish attribution.
fn is_diff_observation(command: &str) -> bool {
    let mut has_patch = false;
    for part in command.split(" && ") {
        if !part.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'-' | b'_' | b'/' | b'.' | b'=')
        }) {
            return false;
        }
        if part.starts_with("git diff -- ") {
            has_patch = true;
        } else if part != "git diff --check"
            && !part.starts_with("git status ")
            && !part.starts_with("git check-ignore -v ")
        {
            return false;
        }
    }
    has_patch
}

fn formatter_and_diff(command: &str) -> Option<&str> {
    let (format, rest) = command.split_once(" && ")?;
    is_formatter(format)
        .then_some(rest)
        .filter(|rest| is_diff_observation(rest))
}

// Command names only nominate a candidate. Attribution still requires a
// completed command and a byte-verified before/after observation.
fn is_formatter(command: &str) -> bool {
    if command.contains([';', '|', '\n', '>', '<', '`', '$']) || command.contains("&&") {
        return false;
    }
    let words: Vec<_> = command.split_whitespace().collect();
    if words.is_empty()
        || words
            .iter()
            .any(|word| matches!(*word, "--check" | "--diff" | "--dry-run" | "--no-fmt"))
    {
        return false;
    }
    words.iter().take(4).any(|word| {
        let word = word.rsplit('/').next().unwrap_or(word).to_ascii_lowercase();
        word == "fmt"
            || word == "format"
            || word.ends_with("fmt")
            || word.ends_with("format")
            || matches!(word.as_str(), "prettier" | "black" | "stylua")
    })
}

fn blob_oid(repo: &Repo, bytes: &[u8]) -> Option<String> {
    let mut child = crate::git::command(&repo.root)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(bytes).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

const MAX_OBSERVED_BLOB: usize = 2 * 1024 * 1024;

fn observed_base(repo: &Repo, oid: &str) -> Option<Vec<u8>> {
    let size = crate::git::command(&repo.root)
        .args(["cat-file", "-s", oid])
        .output()
        .ok()?;
    if !size.status.success() {
        return None;
    }
    if std::str::from_utf8(&size.stdout)
        .ok()?
        .trim()
        .parse::<usize>()
        .ok()?
        > MAX_OBSERVED_BLOB
    {
        return None;
    }
    let blob = crate::git::command(&repo.root)
        .args(["cat-file", "blob", oid])
        .output()
        .ok()?;
    blob.status.success().then_some(blob.stdout)
}

// A captured `git diff` supplies the complete resulting blob hash as well as
// the patch. Checking both object IDs rejects partial/truncated tool output.
fn observed_diff(repo: &Repo, target: &Target, output: &str) -> Option<(String, Vec<u8>, Vec<u8>)> {
    let path = std::str::from_utf8(&target.path).ok()?;
    let header = format!("diff --git a/{path} b/{path}");
    let old_header = format!("--- a/{path}");
    let new_header = format!("+++ b/{path}");
    let starts: Vec<_> = output
        .match_indices("diff --git ")
        .filter_map(|(at, _)| (at == 0 || output.as_bytes()[at - 1] == b'\n').then_some(at))
        .collect();
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(output.len());
        let section = &output[start..end];
        if section.lines().next()? != header {
            continue;
        }
        if !section.lines().any(|line| line == old_header)
            || !section.lines().any(|line| line == new_header)
        {
            return None;
        }
        let index = section
            .lines()
            .find_map(|line| line.strip_prefix("index "))?;
        let (old, rest) = index.split_once("..")?;
        let new = rest.split_whitespace().next()?;
        if ![old, new].iter().all(|oid| {
            (7..=64).contains(&oid.len()) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return None;
        }
        let base = observed_base(repo, old)?;
        let after = Change::Unified(section.into()).apply(&base)?;
        if after.len() > MAX_OBSERVED_BLOB {
            return None;
        }
        if !blob_oid(repo, &base)?.starts_with(old) || !blob_oid(repo, &after)?.starts_with(new) {
            return None;
        }
        return Some((path.into(), base, after));
    }
    None
}
