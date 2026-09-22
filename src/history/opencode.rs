use super::{
    Candidate, Source, Stats, database,
    jsonl::{Collector, codex_patch, stamp, string},
};
use crate::{
    cli::Agent,
    evidence::{Change, History},
    git::{Repo, Target},
};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

pub(super) fn discover(repo: &Repo, root: &Path, candidates: &mut Vec<Candidate>) {
    let path = root.join("opencode.db");
    if let Ok(db) = database(&path) {
        let mut projects = Vec::new();
        if let Ok(mut query) = db.prepare("SELECT id, worktree FROM project")
            && let Ok(rows) =
                query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for (id, worktree) in rows.flatten() {
                if repo.matches_project(Path::new(&worktree)) {
                    projects.push(id);
                }
            }
        }
        for version in [2, 1] {
            let table = if version == 2 {
                "session_v2"
            } else {
                "session"
            };
            let sql = format!(
                "SELECT id,directory,time_created,time_updated,parent_id FROM {table} WHERE project_id=?1"
            );
            if let Ok(mut query) = db.prepare(&sql) {
                for project in &projects {
                    if let Ok(rows) = query.query_map([project], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, Option<String>>(4)?,
                        ))
                    }) {
                        for (id, cwd, start, end, parent) in rows.flatten() {
                            if repo.matches_project(Path::new(&cwd)) {
                                candidates.push(Candidate {
                                    agent: Agent::Opencode,
                                    id: Some(id),
                                    cwd: cwd.into(),
                                    started: Some(start),
                                    updated: Some(end),
                                    parent_file: None,
                                    parent_id: parent,
                                    source: Source::OpenCode {
                                        db: path.clone(),
                                        version,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    // Pre-SQLite v1 stores project/session/message/part objects as separate JSON files.
    let storage = root.join("storage");
    let walk = ignore::WalkBuilder::new(storage.join("session"))
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .build();
    for entry in walk
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
    {
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let (Some(id), Some(cwd)) = (v["id"].as_str(), v["directory"].as_str()) else {
            continue;
        };
        if !repo.matches_project(Path::new(cwd)) {
            continue;
        }
        candidates.push(Candidate {
            agent: Agent::Opencode,
            id: Some(id.into()),
            cwd: cwd.into(),
            started: stamp(&v["time"]["created"]),
            updated: stamp(&v["time"]["updated"]),
            parent_file: None,
            parent_id: v["parentID"].as_str().map(str::to_owned),
            source: Source::OpenCodeFiles(storage.clone()),
        });
    }
}

pub(super) fn load(
    repo: &Repo,
    candidate: &Candidate,
    target: &Target,
    stats: &mut Stats,
) -> Result<History> {
    let Some(id) = candidate.id.as_deref() else {
        return Ok(History::default());
    };
    let mut c = Collector::new(repo, candidate, target);
    c.history.updated = candidate.updated;
    match &candidate.source {
        Source::OpenCode { db, version } => {
            let db = database(db)?;
            if *version == 2 {
                // json_each selects tools inside SQLite. The Rust side never parses
                // assistant reasoning, encrypted provider state, or user attachments.
                let mut query=db.prepare("SELECT m.id,m.time_created,json_extract(m.data,'$.model.id'), t.value FROM session_message m, json_each(m.data,'$.content') t WHERE m.session_id=?1 AND m.type='assistant' AND json_extract(t.value,'$.type')='tool' ORDER BY m.seq, CAST(t.key AS INTEGER)")?;
                let rows = query.query_map([id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?;
                for row in rows {
                    let (mid, when, model, raw) = row?;
                    stats.records += 1;
                    stats.bytes += raw.len() as u64;
                    let tool: Value = serde_json::from_str(&raw)?;
                    if let Some(m) = &model {
                        c.history.models.insert(m.clone());
                    }
                    let native_id =
                        format!("{mid}:{}", string(&tool, &["id"]).unwrap_or("unknown"));
                    parse_tool(&mut c, &tool, model, Some(when), Some(&native_id));
                }
            } else {
                let mut query=db.prepare("SELECT p.id,p.time_created,json_extract(m.data,'$.modelID'),p.data FROM part p LEFT JOIN message m ON m.id=p.message_id WHERE p.session_id=?1 AND json_extract(p.data,'$.type')='tool' ORDER BY p.time_created,p.id")?;
                let rows = query.query_map([id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?;
                for row in rows {
                    let (id, when, model, raw) = row?;
                    stats.records += 1;
                    stats.bytes += raw.len() as u64;
                    let tool: Value = serde_json::from_str(&raw)?;
                    if let Some(m) = &model {
                        c.history.models.insert(m.clone());
                    }
                    parse_tool(&mut c, &tool, model, Some(when), Some(&id));
                }
            }
        }
        Source::OpenCodeFiles(storage) => {
            let mut messages = Vec::new();
            if let Ok(dir) = std::fs::read_dir(storage.join("message").join(id)) {
                for e in dir.flatten() {
                    if let Ok(data) = std::fs::read(e.path()) {
                        let v: Value = serde_json::from_slice(&data)?;
                        stats.bytes += data.len() as u64;
                        stats.records += 1;
                        messages.push(v);
                    }
                }
            }
            messages.sort_by_key(|v| stamp(&v["time"]["created"]));
            for message in messages {
                let Some(mid) = message["id"].as_str() else {
                    continue;
                };
                let model = message["modelID"].as_str().map(str::to_owned);
                let mut parts = Vec::new();
                if let Ok(dir) = std::fs::read_dir(storage.join("part").join(mid)) {
                    for e in dir.flatten() {
                        if let Ok(data) = std::fs::read(e.path()) {
                            let v: Value = serde_json::from_slice(&data)?;
                            stats.bytes += data.len() as u64;
                            stats.records += 1;
                            if v["type"] == "tool" {
                                parts.push(v);
                            }
                        }
                    }
                }
                parts.sort_by_key(|v| stamp(&v["state"]["time"]["start"]));
                for part in parts {
                    parse_tool(
                        &mut c,
                        &part,
                        model.clone(),
                        stamp(&message["time"]["created"]),
                        part["id"].as_str(),
                    );
                }
            }
        }
        _ => unreachable!(),
    }
    c.history.edits.sort_by_key(|e| e.time);
    Ok(c.history)
}

fn parse_tool(
    c: &mut Collector<'_>,
    tool: &Value,
    model: Option<String>,
    fallback_time: Option<i64>,
    id: Option<&str>,
) {
    let state = &tool["state"];
    if state["status"] != "completed" {
        return;
    }
    let Some(name) = string(tool, &["name", "tool"]) else {
        return;
    };
    if !matches!(name, "edit" | "write" | "apply_patch" | "multiedit") {
        return;
    }
    let time = stamp(&tool["time"]["created"])
        .or_else(|| stamp(&state["time"]["start"]))
        .or(fallback_time);
    let input = &state["input"];
    let result = if state["structured"].is_object() {
        &state["structured"]
    } else if state["metadata"].is_object() {
        &state["metadata"]
    } else {
        &state["result"]
    };
    // Per-file results below already describe the same call; input pairs and
    // native patches only add evidence when those are absent.
    let snapshot = result
        .get("files")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
        || result.get("filediff").is_some();
    if name == "edit"
        && !snapshot
        && let (Some(old), Some(new)) = (input["oldString"].as_str(), input["newString"].as_str())
    {
        let old = old.trim_end_matches('\n');
        let new = new.trim_end_matches('\n');
        if !old.is_empty()
            && !new.is_empty()
            && old != new
            && let Some(path) = string(input, &["filePath", "path", "file"])
            && c.push(
                path,
                Change::Fragments(vec![(old.into(), new.into())]),
                time,
                id,
                model.clone(),
            )
        {
            return;
        }
    }
    // apply_patch carries the native patch in its input; the completed
    // status confirms it. Parse per-file fragments like Codex patches.
    // Unproven parses fall through to per-file results below.
    if name == "apply_patch"
        && !snapshot
        && let Some(patch) = input["patchText"].as_str()
        && !patch.is_empty()
    {
        let mut proven = false;
        for (path, change) in codex_patch(patch) {
            let unknown = matches!(change, Change::Unknown);
            proven |= c.push(&path, change, time, id, model.clone()) && !unknown;
        }
        if proven {
            return;
        }
    }
    if let Some(files) = result.get("files").and_then(Value::as_array) {
        for file in files {
            let Some(path) = string(file, &["filePath", "path", "file"]) else {
                continue;
            };
            if file.get("patch").and_then(Value::as_str).is_some()
                || file.get("diff").and_then(Value::as_str).is_some()
                || file.get("unified_diff").and_then(Value::as_str).is_some()
            {
                c.result(name, &state["input"], file, time, id, model.clone());
            } else {
                c.push(
                    path,
                    crate::evidence::Change::Unknown,
                    time,
                    id,
                    model.clone(),
                );
            }
        }
        return;
    }
    if let Some(filediff) = result.get("filediff") {
        c.result(name, &state["input"], filediff, time, id, model);
    } else {
        c.result(name, &state["input"], result, time, id, model);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn v2_schema_can_select_tools_without_deserializing_reasoning() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE session_message(id TEXT,session_id TEXT,type TEXT,seq INTEGER,time_created INTEGER,data TEXT);").unwrap();
        let data = serde_json::json!({"model":{"id":"m"},"content":[{"type":"reasoning","text":"irrelevant"},{"type":"tool","name":"edit","state":{"status":"completed"}}]});
        db.execute(
            "INSERT INTO session_message VALUES ('msg','ses','assistant',1,1,?1)",
            [data.to_string()],
        )
        .unwrap();
        let result:String=db.query_row("SELECT t.value FROM session_message m,json_each(m.data,'$.content') t WHERE json_extract(t.value,'$.type')='tool'",[],|r|r.get(0)).unwrap();
        assert!(!result.contains("irrelevant"));
        assert!(result.contains("completed"));
    }
}
