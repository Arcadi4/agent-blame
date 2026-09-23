use super::{
    Candidate, Source, Stats,
    formatter::{CommandRecord, Formatter},
};
use crate::{
    cli::Agent,
    evidence::{Change, Edit, History},
    git::{Repo, Target},
};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub(super) fn stamp(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    OffsetDateTime::parse(v.as_str()?, &Rfc3339)
        .ok()
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
}
pub(super) fn string<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k)?.as_str())
}
fn reader(path: &Path) -> Result<Box<dyn BufRead>> {
    let file = File::open(path)?;
    if path.extension().is_some_and(|s| s == "zstd") {
        Ok(Box::new(BufReader::new(zstd::stream::read::Decoder::new(
            file,
        )?)))
    } else {
        Ok(Box::new(BufReader::with_capacity(128 * 1024, file)))
    }
}

pub(super) fn generation(path: &Path) -> u64 {
    path.file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| {
            s.split('.')
                .find_map(|p| p.strip_prefix('v').and_then(|n| n.parse().ok()))
        })
        .unwrap_or(0)
}

pub(super) fn header(agent: Agent, path: &Path) -> Result<Option<Candidate>> {
    let mut input = reader(path)?;
    let mut buffer = Vec::new();
    for _ in 0..12 {
        buffer.clear();
        if input.read_until(b'\n', &mut buffer)? == 0 {
            break;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&buffer) else {
            continue;
        };
        let v = if agent == Agent::Codex {
            &v["payload"]
        } else {
            &v
        };
        if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
            let started = stamp(&v["timestamp"]).or_else(|| stamp(&v["createdAt"]));
            let updated = std::fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64);
            let parent = string(v, &["parentSession"]);
            return Ok(Some(Candidate {
                agent,
                id: string(
                    v,
                    if agent == Agent::Claude {
                        &["sessionId", "session_id"]
                    } else {
                        &["sessionId", "session_id", "id"]
                    },
                )
                .map(str::to_owned),
                cwd: cwd.into(),
                started,
                updated,
                parent_file: parent.filter(|s| s.ends_with(".jsonl")).map(PathBuf::from),
                parent_id: string(v, &["forked_from_id", "parentSession"]).map(str::to_owned),
                source: Source::Jsonl(path.to_path_buf()),
            }));
        }
    }
    Ok(None)
}

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(rename = "type", default)]
    kind: &'a str,
    #[serde(default, borrow)]
    payload: Option<&'a RawValue>,
    #[serde(default, borrow)]
    message: Option<&'a RawValue>,
    #[serde(default)]
    timestamp: Value,
    #[serde(default)]
    time: Value,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default, rename = "seedLength")]
    seed_length: Option<u64>,
}

#[derive(Clone)]
struct Call {
    name: String,
    args: Value,
    time: Option<i64>,
    model: Option<String>,
}

pub(super) struct Collector<'a> {
    pub repo: &'a Repo,
    pub cwd: &'a Path,
    pub target: &'a Target,
    pub history: History,
    seen: HashSet<String>,
    pub model: Option<String>,
    calls: HashMap<String, Call>,
    started: Option<i64>,
    inherited: HashSet<String>,
}

impl<'a> Collector<'a> {
    pub fn new(repo: &'a Repo, candidate: &'a Candidate, target: &'a Target) -> Self {
        Self {
            repo,
            cwd: &candidate.cwd,
            target,
            history: History::default(),
            seen: HashSet::new(),
            model: None,
            calls: HashMap::new(),
            started: candidate.started,
            inherited: HashSet::new(),
        }
    }

    /// Records an edit for the blamed target. Returns whether it was kept:
    /// callers with richer fallback evidence must not short-circuit on `false`.
    pub fn push(
        &mut self,
        path: &str,
        change: Change,
        time: Option<i64>,
        id: Option<&str>,
        model: Option<String>,
    ) -> bool {
        let exact = self
            .repo
            .relative_history_path(self.cwd, path)
            .is_some_and(|r| r == self.target.path || r == self.target.parent_path);
        if !exact {
            // Heuristic alternates (moved checkouts, nested worktree copies)
            // must prove bytes: unproven touches stay dropped, and fragments
            // must at least apply to the blamed parent.
            if matches!(change, Change::Unknown)
                || change.apply(&self.target.before).is_none()
                || !self
                    .repo
                    .relative_history_fallbacks(self.cwd, path)
                    .into_iter()
                    .any(|r| r == self.target.path || r == self.target.parent_path)
            {
                return false;
            }
        }
        if time.zip(self.started).is_some_and(|(t, s)| t < s) {
            return false;
        }
        if let Some(id) = id
            && (self.inherited.contains(id) || !self.seen.insert(format!("{id}\0{path}")))
        {
            return false;
        }
        self.history.edits.push(Edit {
            time,
            model,
            change,
        });
        true
    }

    fn set_model(&mut self, model: Option<&str>) {
        if let Some(model) = model {
            self.model = Some(model.into());
            self.history.models.insert(model.into());
        }
    }

    fn call(&mut self, id: &str, name: &str, args: Value, time: Option<i64>) {
        if matches!(
            name,
            "edit"
                | "write"
                | "apply_patch"
                | "Edit"
                | "Write"
                | "MultiEdit"
                | "NotebookEdit"
                | "str_replace_editor"
                | "ast_edit"
                | "bash"
        ) {
            self.calls.insert(
                id.to_owned(),
                Call {
                    name: name.into(),
                    args,
                    time,
                    model: self.model.clone(),
                },
            );
        }
    }

    pub fn result(
        &mut self,
        name: &str,
        args: &Value,
        result: &Value,
        time: Option<i64>,
        id: Option<&str>,
        model: Option<String>,
    ) {
        if result["staged"].as_bool() == Some(true) || result["applied"].as_bool() == Some(false) {
            return;
        }
        let path = string(
            result,
            &[
                "path",
                "file",
                "filePath",
                "filepath",
                "file_path",
                "notebook_path",
                "resolvedPath",
                "resource",
            ],
        )
        .or_else(|| string(args, &["path", "filePath", "file_path", "notebook_path"]));
        if name == "write"
            && result["operation"] == "create"
            && let (Some(path), Some(after)) = (path, args["content"].as_str())
        {
            self.push(
                path,
                Change::Snapshot {
                    before: vec![],
                    after: after.as_bytes().to_vec(),
                },
                time,
                id,
                model,
            );
            return;
        }
        if let Some(results) = result
            .get("perFileResults")
            .and_then(Value::as_array)
            .or_else(|| result.get("files").and_then(Value::as_array))
        {
            for result in results {
                if let Some(path) = result.as_str() {
                    self.push(path, Change::Unknown, time, id, model.clone());
                } else {
                    self.result(name, args, result, time, id, model.clone());
                }
            }
            return;
        }
        if let Some(changes) = result.get("diffs").and_then(Value::as_array) {
            let mut grouped: HashMap<String, Vec<(String, String)>> = HashMap::new();
            for item in changes {
                let Some(path) = string(item, &["path"]) else {
                    continue;
                };
                if let (Some(old), Some(new)) = (item["oldText"].as_str(), item["newText"].as_str())
                {
                    grouped
                        .entry(path.to_owned())
                        .or_default()
                        .push((old.into(), new.into()));
                } else {
                    self.push(path, Change::Unknown, time, id, model.clone());
                }
            }
            for (path, pairs) in grouped {
                self.push(&path, Change::Fragments(pairs), time, id, model.clone());
            }
            return;
        }
        let Some(path) = path else {
            return;
        };
        // Claude marks edits changed in its permission UI explicitly. Their
        // final bytes belong to an unresolved human/agent mixture.
        if result["userModified"].as_bool() == Some(true) {
            self.push(path, Change::Unknown, time, id, model);
            return;
        }
        let old = string(result, &["oldText", "before", "original_file"]);
        let new = string(result, &["newText", "after", "updated_file"]);
        let change = if let (Some(before), Some(after)) = (old, new) {
            Change::Snapshot {
                before: before.as_bytes().to_vec(),
                after: after.as_bytes().to_vec(),
            }
        } else if let Some(patch) = structured_patch(&result["structuredPatch"]) {
            Change::Unified(patch)
        } else if let Some(patch) = string(result, &["patch", "unified_diff"]) {
            Change::Unified(patch.into())
        } else if let Some(diff) = result.get("diff").and_then(Value::as_str) {
            if diff.contains("@@ ") {
                Change::Unified(diff.into())
            } else {
                numbered_patch(diff)
                    .map(Change::Unified)
                    .unwrap_or(Change::Unknown)
            }
        } else if matches!(name, "Write" | "write") {
            let before = result
                .get("originalFile")
                .or_else(|| result.get("originalContent"));
            let after = result.get("content").and_then(Value::as_str);
            match (before, after) {
                (Some(Value::String(before)), Some(after)) => Change::Snapshot {
                    before: before.as_bytes().to_vec(),
                    after: after.as_bytes().to_vec(),
                },
                (Some(Value::Null), Some(after)) if result["type"] == "create" => {
                    Change::Snapshot {
                        before: vec![],
                        after: after.as_bytes().to_vec(),
                    }
                }
                _ if result["existed"].as_bool() == Some(false)
                    || result["exists"].as_bool() == Some(false)
                    || result["operation"] == "create" =>
                {
                    args["content"]
                        .as_str()
                        .map(|after| Change::Snapshot {
                            before: vec![],
                            after: after.as_bytes().to_vec(),
                        })
                        .unwrap_or(Change::Unknown)
                }
                _ => Change::Unknown,
            }
        } else {
            Change::Unknown
        };
        self.push(path, change, time, id, model);
    }
}

pub(super) fn structured_patch(value: &Value) -> Option<String> {
    let hunks = value.as_array()?;
    let mut result = String::new();
    for h in hunks {
        result += &format!(
            "@@ -{},{} +{},{} @@\n",
            h["oldStart"].as_u64()?,
            h["oldLines"].as_u64()?,
            h["newStart"].as_u64()?,
            h["newLines"].as_u64()?
        );
        for line in h["lines"].as_array()? {
            result += line.as_str()?;
            result.push('\n');
        }
    }
    (!result.is_empty()).then_some(result)
}

fn numbered_patch(diff: &str) -> Option<String> {
    let mut result = String::new();
    let mut old_start = None;
    let mut new_start = None;
    let mut old_count = 0;
    let mut new_count = 0;
    let mut body = String::new();
    let mut delta: isize = 0;
    let flush = |result: &mut String,
                 body: &mut String,
                 a: &mut Option<usize>,
                 b: &mut Option<usize>,
                 ac: &mut usize,
                 bc: &mut usize|
     -> Option<()> {
        if body.is_empty() {
            return Some(());
        }
        result.push_str(&format!(
            "@@ -{},{} +{},{} @@\n{}",
            (*a).or_else(|| b.map(|n| n.saturating_sub(1)))?,
            ac,
            (*b).or_else(|| a.map(|n| n.saturating_sub(1)))?,
            bc,
            body
        ));
        body.clear();
        *a = None;
        *b = None;
        *ac = 0;
        *bc = 0;
        Some(())
    };
    for raw in diff.lines() {
        let sign = raw.as_bytes().first().copied()?;
        if !matches!(sign, b' ' | b'+' | b'-')
            || !raw[1..]
                .trim_start()
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_digit)
        {
            flush(
                &mut result,
                &mut body,
                &mut old_start,
                &mut new_start,
                &mut old_count,
                &mut new_count,
            )?;
            continue;
        }
        let text = raw[1..].trim_start();
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        let number: usize = text[..digits].parse().ok()?;
        let text = text[digits..]
            .strip_prefix('|')
            .or_else(|| text[digits..].strip_prefix(' '))?;
        if sign != b'+' {
            old_start.get_or_insert(number);
            old_count += 1;
        }
        if sign != b'-' {
            new_start.get_or_insert(if sign == b' ' {
                number.checked_add_signed(delta)?
            } else {
                number
            });
            new_count += 1;
        }
        if sign == b'+' {
            delta += 1;
        } else if sign == b'-' {
            delta -= 1;
        }
        body.push(sign as char);
        body.push_str(text);
        body.push('\n');
    }
    flush(
        &mut result,
        &mut body,
        &mut old_start,
        &mut new_start,
        &mut old_count,
        &mut new_count,
    )?;
    (!result.is_empty()).then_some(result)
}

pub(super) fn load(
    repo: &Repo,
    candidate: &Candidate,
    path: &Path,
    target: &Target,
    stats: &mut Stats,
) -> Result<History> {
    let mut c = Collector::new(repo, candidate, target);
    if let Some(parent) = &candidate.parent_file
        && parent != path
    {
        if let Ok(mut input) = reader(parent) {
            let mut line = Vec::new();
            while input.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
                stats.records += 1;
                stats.bytes += line.len() as u64;
                if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                    for pointer in [
                        "/id",
                        "/uuid",
                        "/payload/id",
                        "/payload/call_id",
                        "/payload/item/id",
                    ] {
                        if let Some(id) = v.pointer(pointer).and_then(Value::as_str) {
                            c.inherited.insert(id.into());
                        }
                    }
                }
                line.clear();
            }
        } else {
            return Ok(History::default());
        }
    }
    let mut input = reader(path)?;
    let mut buffer = Vec::new();
    let mut branches = HashMap::<String, Option<String>>::new();
    let mut modern_codex = Vec::new();
    let mut formatter = Formatter::new(target);
    let mut seed_length = 0;
    loop {
        buffer.clear();
        match input.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
            _ => {}
        }
        stats.records += 1;
        stats.bytes += buffer.len() as u64;
        let envelope = match serde_json::from_slice::<Envelope<'_>>(&buffer) {
            Ok(v) => v,
            Err(_) if !buffer.ends_with(b"\n") => break,
            Err(e) => return Err(e.into()),
        };
        let time = stamp(&envelope.timestamp).or_else(|| stamp(&envelope.time));
        c.history.updated = c.history.updated.max(time);
        if candidate.agent == Agent::Dsh {
            if envelope.kind == "session" {
                seed_length = envelope.seed_length.unwrap_or(0);
            }
            if envelope.seq.is_some_and(|seq| seq < seed_length) {
                continue;
            }
        }
        match candidate.agent {
            Agent::Codex => {
                let Some(raw) = envelope.payload else {
                    continue;
                };
                if !matches!(
                    envelope.kind,
                    "event_msg" | "turn_context" | "response_item"
                ) {
                    continue;
                }
                if envelope.kind == "turn_context" {
                    let v: Value = serde_json::from_str(raw.get())?;
                    c.set_model(v["model"].as_str());
                    continue;
                }
                if ![
                    "FileChange",
                    "file_change",
                    "patch_apply_end",
                    "CommandExecution",
                    "function_call",
                    "custom_tool_call",
                ]
                .iter()
                .any(|s| memchr::memmem::find(raw.get().as_bytes(), s.as_bytes()).is_some())
                {
                    continue;
                }
                let v: Value = serde_json::from_str(raw.get())?;
                if envelope.kind == "event_msg" {
                    let (item, event_time) = if v["type"] == "item_completed" {
                        (&v["item"], stamp(&v["started_at_ms"]).or(time))
                    } else {
                        (&v, time)
                    };
                    if matches!(
                        item["type"].as_str(),
                        Some("FileChange" | "file_change" | "patch_apply_end")
                    ) {
                        let success =
                            matches!(item["status"].as_str(), Some("completed" | "succeeded"))
                                || item["success"].as_bool() == Some(true);
                        if !success {
                            continue;
                        }
                        if let Some(changes) = item["changes"].as_object() {
                            for (path, change) in changes {
                                let id = string(item, &["id", "call_id"]);
                                let change = match string(change, &["unified_diff", "diff"]) {
                                    Some(diff) => Change::Unified(diff.into()),
                                    // Add entries carry the created file's full
                                    // bytes; that is exact evidence, not intent.
                                    _ if string(change, &["type"]) == Some("add") => {
                                        match string(change, &["content"]) {
                                            Some(content) => Change::Snapshot {
                                                before: vec![],
                                                after: content.as_bytes().to_vec(),
                                            },
                                            None => Change::Unknown,
                                        }
                                    }
                                    _ => Change::Unknown,
                                };
                                if c.push(path, change.clone(), event_time, id, c.model.clone()) {
                                    formatter.edit(&change);
                                }
                            }
                        }
                    }
                    if item["type"] == "CommandExecution" {
                        let command = item["command"]
                            .as_array()
                            .and_then(|parts| parts.last())
                            .and_then(Value::as_str);
                        if let Some(command) = command {
                            let model = c.model.clone();
                            formatter.command(
                                &mut c,
                                CommandRecord {
                                    command,
                                    output: item["stdout"].as_str().unwrap_or(""),
                                    completed: item["status"] == "completed"
                                        && item["exit_code"].as_i64() == Some(0),
                                    cwd: item["cwd"].as_str(),
                                    id: item["id"].as_str(),
                                    time: event_time,
                                    model,
                                },
                            );
                        }
                    }
                    continue;
                }
                match v["type"].as_str() {
                    Some("function_call" | "custom_tool_call")
                        if matches!(
                            v["name"].as_str(),
                            Some("apply_patch" | "functions.apply_patch")
                        ) =>
                    {
                        let mut args = v
                            .get("input")
                            .cloned()
                            .unwrap_or_else(|| v["arguments"].clone());
                        if let Some(encoded) = args.as_str().filter(|s| s.starts_with('{'))
                            && let Ok(decoded) = serde_json::from_str::<Value>(encoded)
                        {
                            args = decoded
                                .get("patch")
                                .or_else(|| decoded.get("input"))
                                .cloned()
                                .unwrap_or(decoded);
                        }
                        if let Some(id) = v["call_id"].as_str() {
                            c.call(id, "apply_patch", args, time);
                        }
                    }
                    Some("function_call_output" | "custom_tool_call_output") => {
                        let Some(id) = v["call_id"].as_str() else {
                            continue;
                        };
                        let Some(call) = c.calls.remove(id) else {
                            continue;
                        };
                        let text = content_text(&v["output"]);
                        if text.contains("Success. Updated the following files:")
                            && let Some(patch) = call.args.as_str()
                        {
                            modern_codex.push((
                                id.to_owned(),
                                call.time,
                                call.model,
                                patch.to_owned(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            Agent::Omp | Agent::Pi => {
                #[derive(Deserialize)]
                struct Branch<'a> {
                    id: Option<&'a str>,
                    #[serde(rename = "parentId")]
                    parent: Option<&'a str>,
                }
                let branch: Branch = serde_json::from_slice(&buffer)?;
                if let Some(parent) = branch.parent {
                    c.model = branches.get(parent).cloned().flatten();
                }
                if let Some(id) = branch.id {
                    branches.insert(id.into(), c.model.clone());
                }
                if envelope.kind == "model_change" {
                    let v: Value = serde_json::from_slice(&buffer)?;
                    c.set_model(v["modelId"].as_str());
                    if let Some(id) = v["id"].as_str() {
                        branches.insert(id.into(), c.model.clone());
                    }
                    continue;
                }
                if envelope.kind != "message" {
                    continue;
                }
                let Some(raw) = envelope.message else {
                    continue;
                };
                #[derive(Deserialize)]
                struct Role<'a> {
                    role: &'a str,
                    model: Option<&'a str>,
                }
                let role: Role = serde_json::from_str(raw.get())?;
                if !matches!(role.role, "assistant" | "toolResult") {
                    continue;
                }
                let v: Value = serde_json::from_slice(&buffer)?;
                c.set_model(role.model);
                if let Some(id) = v["id"].as_str() {
                    branches.insert(id.into(), c.model.clone());
                }
                let m = &v["message"];
                let when = stamp(&m["timestamp"]).or(time);
                if role.role == "assistant" {
                    if let Some(content) = m["content"].as_array() {
                        for part in content {
                            if part["type"] == "toolCall"
                                && let (Some(id), Some(name)) =
                                    (part["id"].as_str(), part["name"].as_str())
                            {
                                c.call(id, name, part["arguments"].clone(), when);
                            }
                        }
                    }
                } else {
                    if m["isError"].as_bool() != Some(false) {
                        continue;
                    }
                    let Some(id) = m["toolCallId"].as_str() else {
                        continue;
                    };
                    if c.inherited.contains(v["id"].as_str().unwrap_or("")) {
                        continue;
                    }
                    let Some(call) = c.calls.remove(id) else {
                        continue;
                    };
                    let repaired =
                        content_text(&m["content"]).contains("an automatic syntax repair (");
                    let previous = c.history.edits.len();
                    if call.name == "bash" {
                        let args = &call.args;
                        if let Some(command) = args["command"].as_str() {
                            formatter.command(
                                &mut c,
                                CommandRecord {
                                    command,
                                    output: &content_text(&m["content"]),
                                    completed: true,
                                    cwd: args["cwd"].as_str(),
                                    id: Some(id),
                                    time: call.time.or(when),
                                    model: call.model.clone(),
                                },
                            );
                        }
                    }
                    c.result(
                        &call.name,
                        &call.args,
                        &m["details"],
                        call.time.or(when),
                        Some(id),
                        call.model,
                    );
                    for edit in &c.history.edits[previous..] {
                        formatter.edit(&edit.change);
                    }
                    if repaired {
                        for edit in &mut c.history.edits[previous..] {
                            edit.change = Change::Unknown;
                        }
                    }
                }
            }
            Agent::Claude => {
                if !matches!(envelope.kind, "assistant" | "user") {
                    continue;
                }
                let v: Value = serde_json::from_slice(&buffer)?;
                let m = &v["message"];
                c.set_model(m["model"].as_str());
                if let Some(content) = m["content"].as_array() {
                    for part in content {
                        if part["type"] == "tool_use"
                            && let (Some(id), Some(name)) =
                                (part["id"].as_str(), part["name"].as_str())
                        {
                            c.call(id, name, part["input"].clone(), time);
                        }
                        if part["type"] == "tool_result" {
                            let Some(id) = part["tool_use_id"].as_str() else {
                                continue;
                            };
                            let Some(call) = c.calls.remove(id) else {
                                continue;
                            };
                            if part["is_error"].as_bool() == Some(true) {
                                continue;
                            }
                            let result = &v["toolUseResult"];
                            if result.is_object() {
                                c.result(
                                    &call.name,
                                    &call.args,
                                    result,
                                    call.time.or(time),
                                    Some(id),
                                    call.model,
                                );
                            }
                        }
                    }
                }
            }
            Agent::Dsh => {
                if envelope.kind == "session/end-seed" {
                    c.history.edits.clear();
                    c.calls.clear();
                    continue;
                }
                if !matches!(
                    envelope.kind,
                    "request/header" | "llm/config" | "tool/call" | "tool/result"
                ) {
                    continue;
                }
                let v: Value = serde_json::from_slice(&buffer)?;
                let data = &v["data"];
                if envelope.kind == "request/header" {
                    c.set_model(data.pointer("/header/config/model").and_then(Value::as_str));
                    continue;
                }
                if envelope.kind == "llm/config" {
                    c.set_model(data["model"].as_str());
                    continue;
                }
                if envelope.kind == "tool/call" {
                    if let (Some(id), Some(name), Some(args)) = (
                        data["callId"].as_str(),
                        data["name"].as_str(),
                        data["arguments"].as_str(),
                    ) && let Ok(args) = serde_json::from_str(args)
                    {
                        c.call(id, name, args, time);
                    }
                } else {
                    let message = &data["message"];
                    let parts = message["content"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    for part in parts {
                        let Some(id) = string(part, &["toolCallId", "callId", "tool_call_id"])
                        else {
                            continue;
                        };
                        let Some(call) = c.calls.remove(id) else {
                            continue;
                        };
                        if part["isError"].as_bool() == Some(true)
                            || data.get("error").is_some_and(|v| !v.is_null())
                        {
                            continue;
                        }
                        c.result(
                            &call.name,
                            &call.args,
                            &data["meta"],
                            call.time.or(time),
                            Some(id),
                            call.model,
                        );
                    }
                }
            }
            Agent::Opencode => unreachable!(),
        }
    }
    // New Codex clients emit the applied FileChange, including calls dispatched
    // through code mode. Only old clients need the confirmed native patch fallback.
    if c.history.edits.is_empty() {
        for (id, time, model, patch) in modern_codex {
            for (path, change) in codex_patch(&patch) {
                c.push(&path, change, time, Some(&id), model.clone());
            }
        }
    }
    c.history.edits.sort_by_key(|e| e.time);
    Ok(c.history)
}

fn content_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_owned();
    }
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

pub(super) fn codex_patch(patch: &str) -> Vec<(String, Change)> {
    let mut results = Vec::new();
    let mut path = None;
    let mut pairs = Vec::new();
    let mut old = String::new();
    let mut new = String::new();
    let mut reliable = true;
    let flush_pair = |old: &mut String, new: &mut String, pairs: &mut Vec<(String, String)>| {
        if !old.is_empty() || !new.is_empty() {
            pairs.push((
                std::mem::take(old).trim_end_matches('\n').into(),
                std::mem::take(new).trim_end_matches('\n').into(),
            ));
        }
    };
    for line in patch.lines() {
        if line.starts_with("*** ") && !line.starts_with("*** End of File") {
            flush_pair(&mut old, &mut new, &mut pairs);
            if let Some(path) = path.take() {
                results.push((
                    path,
                    if reliable {
                        Change::Fragments(std::mem::take(&mut pairs))
                    } else {
                        pairs.clear();
                        Change::Unknown
                    },
                ));
            }
            if let Some(name) = line.strip_prefix("*** Update File: ") {
                path = Some(name.into());
                reliable = true;
            } else if let Some(name) = line
                .strip_prefix("*** Add File: ")
                .or_else(|| line.strip_prefix("*** Delete File: "))
            {
                path = Some(name.into());
                reliable = false;
            }
        } else if line.starts_with("@@") {
            flush_pair(&mut old, &mut new, &mut pairs);
        } else if line == "*** End of File" {
            reliable = false;
        } else if path.is_some() && !line.is_empty() {
            let sign = line.as_bytes()[0];
            if !matches!(sign, b' ' | b'+' | b'-') {
                reliable = false;
                continue;
            }
            if sign != b'+' {
                old.push_str(&line[1..]);
                old.push('\n');
            }
            if sign != b'-' {
                new.push_str(&line[1..]);
                new.push('\n');
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn applied_numbered_diff_preserves_coordinates() {
        let patch = numbered_patch(" 1|same\n-2|old\n+2|new\n 3|tail").unwrap();
        assert_eq!(
            Change::Unified(patch).apply(b"same\nold\ntail\n"),
            Some(b"same\nnew\ntail\n".to_vec())
        );
    }
    #[test]
    fn structured_results_preserve_eof_markers() {
        let value = serde_json::json!([{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-old","+new","\\ No newline at end of file"]}]);
        assert_eq!(
            Change::Unified(structured_patch(&value).unwrap()).apply(b"old\n"),
            Some(b"new".to_vec())
        );
    }
}
