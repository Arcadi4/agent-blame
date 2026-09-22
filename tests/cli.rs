use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

const START: &str = "2020-01-01T00:01:00Z";
const EDIT: &str = "2020-01-01T00:02:00Z";
const END: &str = "2020-01-01T00:03:00Z";
const BEFORE: &str = "same\nold\ntail\n";
const AFTER: &str = "same\nnew\ntail\n";
const PATCH: &str = "@@ -1,3 +1,3 @@\n same\n-old\n+new\n tail\n";

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    stores: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        Self::with_hash("sha1")
    }
    fn with_hash(hash: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("repo");
        let stores = base.join("stores");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&stores).unwrap();
        let f = Self {
            _temp: temp,
            root,
            stores,
        };
        f.git(&["init", &format!("--object-format={hash}")]);
        f.git(&["config", "user.name", "Fixture Author"]);
        f.git(&["config", "user.email", "fixture@example.test"]);
        fs::write(f.root.join("a.txt"), BEFORE).unwrap();
        fs::write(f.root.join("unrelated.txt"), "original\n").unwrap();
        f.commit("2020-01-01T00:00:00Z");
        fs::write(f.root.join("a.txt"), AFTER).unwrap();
        f.commit("2020-01-01T00:10:00Z");
        f
    }
    fn git(&self, args: &[&str]) -> String {
        let o = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap()
    }
    fn commit(&self, date: &str) {
        self.git(&["add", "."]);
        let o = Command::new("git")
            .current_dir(&self.root)
            .args(["-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture"])
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-blame"));
        cmd.current_dir(&self.root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for (env, dir) in [
            ("CODEX_HOME", "codex"),
            ("OMP_CONFIG_DIR", "omp"),
            ("PI_CODING_AGENT_DIR", "pi"),
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("DSH_SESSIONS_DIR", "dsh"),
            ("XDG_DATA_HOME", "xdg"),
        ] {
            cmd.env(env, self.stores.join(dir));
        }
        cmd.output().unwrap()
    }
    fn text(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap()
    }
    fn jsonl(&self, path: &str, rows: &[Value]) -> PathBuf {
        let path = self.stores.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = rows.iter().map(|v| format!("{v}\n")).collect::<String>();
        fs::write(&path, text).unwrap();
        path
    }
    fn native(&self, agent: &str) -> Vec<Value> {
        match agent {
            "codex" => vec![
                json!({"type":"session_meta","payload":{"id":"native-codex","cwd":self.root,"timestamp":START}}),
                json!({"type":"turn_context","timestamp":START,"payload":{"model":"fixture-model"}}),
                json!({"type":"event_msg","timestamp":EDIT,"payload":{"type":"item_completed","item":{"type":"FileChange","id":"applied-edit","status":"completed","changes":{"a.txt":{"type":"update","unified_diff":PATCH}}}}}),
                json!({"type":"event_msg","timestamp":END,"payload":{"type":"future-unrelated-event","arbitrary":true}}),
            ],
            "omp" | "pi" => vec![
                json!({"type":"session","version":3,"id":format!("native-{agent}"),"cwd":self.root,"timestamp":START}),
                json!({"type":"model_change","id":"model","modelId":"fixture-model","timestamp":START}),
                json!({"type":"message","id":"request","parentId":"model","timestamp":EDIT,"message":{"role":"assistant","model":"fixture-model","content":[{"type":"toolCall","id":"call","name":"edit","arguments":{"path":"a.txt","oldText":"not the applied text","newText":"intent"}}]}}),
                json!({"type":"message","id":"result","parentId":"request","timestamp":END,"message":{"role":"toolResult","toolCallId":"call","isError":false,"details":if agent=="omp" {json!({"path":"a.txt","oldText":BEFORE,"newText":AFTER})} else {json!({"patch":PATCH})}}}),
            ],
            "claude" => vec![
                json!({"type":"assistant","sessionId":"native-claude","cwd":self.root,"timestamp":START,"message":{"model":"fixture-model","content":[{"type":"tool_use","id":"call","name":"Edit","input":{"file_path":"a.txt","old_string":"wrong intent","new_string":"wrong"}}]}}),
                json!({"type":"user","timestamp":END,"message":{"content":[{"type":"tool_result","tool_use_id":"call","is_error":false}]},"toolUseResult":{"filePath":"a.txt","structuredPatch":[{"oldStart":1,"oldLines":3,"newStart":1,"newLines":3,"lines":[" same","-old","+new"," tail"]}]}}),
            ],
            "dsh" => vec![
                json!({"type":"session","version":3,"id":"native-dsh","cwd":self.root,"createdAt":1577836860000i64}),
                json!({"type":"request/header","seq":0,"time":1577836860000i64,"data":{"header":{"config":{"model":"fixture-model"}}}}),
                json!({"type":"tool/call","seq":1,"time":1577836920000i64,"data":{"callId":"call","name":"edit","arguments":"{\"file_path\":\"a.txt\"}"}}),
                json!({"type":"tool/result","seq":2,"time":1577836980000i64,"data":{"message":{"role":"user","content":[{"type":"tool-result","toolCallId":"call","isError":false}]},"meta":{"diffs":[{"path":"a.txt","oldText":"old","newText":"new"}]}}}),
            ],
            _ => unreachable!(),
        }
    }
    fn install(&self, agent: &str, rows: &[Value]) -> PathBuf {
        let path = match agent {
            "codex" => "codex/sessions/session.jsonl",
            "omp" => "omp/agent/sessions/session.jsonl",
            "pi" => "pi/sessions/session.jsonl",
            "claude" => "claude/projects/project/session.jsonl",
            "dsh" => "dsh/project/native-dsh/session.v3.jsonl",
            _ => unreachable!(),
        };
        self.jsonl(path, rows)
    }
}

#[test]
fn all_jsonl_adapters_use_applied_results_and_ignore_incomplete_tails() {
    let f = Fixture::new();
    for agent in ["codex", "omp", "pi", "claude", "dsh"] {
        let path = f.install(agent, &f.native(agent));
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"{\"type\":")
            .unwrap();
        let out = f.text(&["--agent", agent, "--style=porcelain", "--", "a.txt"]);
        let records: Vec<_> = out.split("source ").collect();
        assert!(
            records[0].contains("agent \"unknown\""),
            "{agent}: unchanged context claimed"
        );
        assert!(
            records[1].contains(&format!("agent \"{agent}\"")),
            "{agent}: {out}"
        );
        assert!(records[1].contains("model-id \"fixture-model\""));
        assert!(records[1].contains(&format!("session-id \"native-{agent}\"")));
        assert!(records[1].contains(&format!("time \"{END}\"")));
        assert!(records[2].contains("agent \"unknown\""));
    }
}

#[test]
fn writes_require_observed_preimages_and_applied_status() {
    let f = Fixture::new();
    let mut rows = f.native("claude");
    rows[0]["message"]["content"][0]["name"] = json!("Write");
    rows[1]["toolUseResult"] =
        json!({"type":"update","filePath":"a.txt","originalFile":BEFORE,"content":AFTER});
    f.install("claude", &rows);
    assert!(
        f.text(&["--agent=claude", "--style=porcelain", "-L2,2", "a.txt"])
            .contains("agent \"claude\"")
    );
    for extra in [
        json!({"originalFile":null}),
        json!({"userModified":true}),
        json!({"staged":true}),
    ] {
        let mut rows = rows.clone();
        for (key, value) in extra.as_object().unwrap() {
            rows[1]["toolUseResult"][key] = value.clone();
        }
        f.install("claude", &rows);
        assert!(
            f.text(&["--agent=claude", "--style=porcelain", "-L2,2", "a.txt"])
                .contains("agent \"unknown\"")
        );
    }
    rows[0]["message"]["content"][0]["name"] = json!("NotebookEdit");
    rows[1]["toolUseResult"] =
        json!({"notebook_path":"a.txt","original_file":BEFORE,"updated_file":AFTER});
    f.install("claude", &rows);
    assert!(
        f.text(&["--agent=claude", "--style=porcelain", "-L2,2", "a.txt"])
            .contains("agent \"claude\"")
    );
}

#[test]
fn git_contract_author_selection_rename_dirty_and_hashes() {
    let f = Fixture::with_hash("sha256");
    f.install("codex", &f.native("codex"));
    let commit = f.git(&["rev-parse", "HEAD"]).trim().to_owned();
    f.git(&["mv", "a.txt", "renamed-λ.txt"]);
    f.commit("2020-01-01T00:20:00Z");
    fs::write(f.root.join("unrelated.txt"), "dirty unrelated\n").unwrap();
    let out = f.text(&[
        "--agent=codex",
        "--style=porcelain",
        "--me=nobody,fixture@example.test",
        "-L2,+1",
        "--",
        "renamed-λ.txt",
    ]);
    assert!(out.starts_with(&format!("{commit} 2 2 1\n")));
    assert_eq!(commit.len(), 64);
    assert!(out.contains("agent \"codex\""));
    assert!(out.contains("filename \"a.txt\""));
    let filtered = f.text(&[
        "--agent=codex",
        "--style=porcelain",
        "--me=nobody",
        "-L2,2",
        "renamed-λ.txt",
    ]);
    assert!(filtered.contains("author \"Fixture Author\""));
    assert!(filtered.contains("agent \"unknown\""));
    assert!(
        !f.run(&["--me=nobody", "--everyone", "renamed-λ.txt"])
            .status
            .success()
    );
    for flag in ["-M", "-C", "-w", "--reverse"] {
        assert!(!f.run(&[flag, "renamed-λ.txt"]).status.success());
    }
    assert!(
        f.text(&[
            "--everyone",
            "--agent=codex",
            "--style=porcelain",
            "-L2,2",
            "renamed-λ.txt"
        ])
        .contains("agent \"codex\"")
    );
    fs::write(f.root.join("renamed-λ.txt"), "dirty\n").unwrap();
    assert!(!f.run(&["renamed-λ.txt"]).status.success());
    fs::write(f.root.join("untracked"), "x").unwrap();
    assert!(!f.run(&["untracked"]).status.success());
}

#[test]
fn forward_search_is_opt_in_and_chooses_earliest_concrete_edit() {
    let f = Fixture::new();
    for (id, time) in [
        ("later", "2020-01-01T00:40:00Z"),
        ("earliest", "2020-01-01T00:30:00Z"),
    ] {
        let mut rows = f.native("codex");
        rows[0]["payload"]["id"] = json!(id);
        rows[0]["payload"]["timestamp"] = json!(time);
        for row in &mut rows[1..] {
            row["timestamp"] = json!(time);
        }
        f.jsonl(&format!("codex/sessions/{id}.jsonl"), &rows);
    }
    assert!(
        f.text(&["--agent=codex", "--style=porcelain", "-L2,2", "a.txt"])
            .contains("agent \"unknown\"")
    );
    let out = f.text(&[
        "--agent=codex",
        "--style=porcelain",
        "--timeless",
        "-L2,2",
        "a.txt",
    ]);
    assert!(out.contains("session-id \"earliest\""));
}

#[test]
fn fork_inheritance_and_sessions_spanning_commit() {
    let f = Fixture::new();
    let rows = f.native("codex");
    f.install("codex", &rows);
    let mut child = rows.clone();
    child[0]["payload"]["id"] = json!("child");
    child[0]["payload"]["forked_from_id"] = json!("native-codex");
    child[3]["timestamp"] = json!("2020-01-01T00:15:00Z");
    f.jsonl("codex/sessions/child.jsonl", &child);
    assert!(
        f.text(&["--agent=codex", "--style=porcelain", "-L2,2", "a.txt"])
            .contains("session-id \"native-codex\"")
    );
    let mut rows = rows;
    rows[3]["timestamp"] = json!("2020-01-01T00:15:00Z");
    f.install("codex", &rows);
    assert!(
        f.text(&["--agent=codex", "--style=porcelain", "-L2,2", "a.txt"])
            .contains("time \"2020-01-01T00:15:00Z\"")
    );
}

fn opencode_database(f: &Fixture, version: u8) {
    let dir = f.stores.join("xdg/opencode");
    fs::create_dir_all(&dir).unwrap();
    let db = rusqlite::Connection::open(dir.join("opencode.db")).unwrap();
    db.execute_batch("CREATE TABLE project(id TEXT,worktree TEXT);")
        .unwrap();
    db.execute(
        "INSERT INTO project VALUES ('project',?1)",
        [f.root.to_str().unwrap()],
    )
    .unwrap();
    let table = if version == 2 {
        "session_v2"
    } else {
        "session"
    };
    db.execute_batch(&format!("CREATE TABLE {table}(id TEXT,project_id TEXT,directory TEXT,time_created INTEGER,time_updated INTEGER,parent_id TEXT);")).unwrap();
    db.execute(&format!("INSERT INTO {table} VALUES ('native-opencode','project',?1,1577836860000,1577836980000,NULL)"),[f.root.to_str().unwrap()]).unwrap();
    let tool = json!({"type":"tool","id":"call","tool":"edit","name":"edit","state":{"status":"completed","input":{"filePath":"a.txt"},"time":{"start":1577836920000i64},"metadata":{"filediff":{"file":"a.txt","before":BEFORE,"after":AFTER}}}});
    if version == 2 {
        db.execute_batch("CREATE TABLE session_message(id TEXT,session_id TEXT,type TEXT,seq INTEGER,time_created INTEGER,data TEXT);").unwrap();
        let message = json!({"model":{"id":"fixture-model"},"content":[{"type":"reasoning","text":"unrelated"},tool]});
        db.execute("INSERT INTO session_message VALUES ('msg','native-opencode','assistant',0,1577836920000,?1)",[message.to_string()]).unwrap();
    } else {
        db.execute_batch("CREATE TABLE message(id TEXT,data TEXT); CREATE TABLE part(id TEXT,session_id TEXT,message_id TEXT,time_created INTEGER,data TEXT);").unwrap();
        db.execute(
            "INSERT INTO message VALUES ('msg',?1)",
            [json!({"modelID":"fixture-model"}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO part VALUES ('part','native-opencode','msg',1577836920000,?1)",
            [tool.to_string()],
        )
        .unwrap();
    }
}

#[test]
fn opencode_v1_and_v2_applied_tool_records() {
    for version in [1, 2] {
        let f = Fixture::new();
        opencode_database(&f, version);
        let out = f.text(&["--agent=opencode", "--style=porcelain", "-L2,2", "a.txt"]);
        assert!(out.contains("agent \"opencode\""), "v{version}: {out}");
        assert!(out.contains("model-id \"fixture-model\""));
        assert!(out.contains("session-id \"native-opencode\""));
    }
}

#[test]
fn confirmed_creation_can_precede_an_unrelated_parent_commit() {
    for version in [1, 2] {
        let f = Fixture::new();
        fs::write(f.root.join("created.txt"), AFTER).unwrap();
        // Like a batch of atomic commits: the file was created before the
        // preceding commit, which did not contain it, at the same Git second.
        f.commit("2020-01-01T00:10:00Z");
        opencode_database(&f, version);
        let db = rusqlite::Connection::open(f.stores.join("xdg/opencode/opencode.db")).unwrap();
        for exists in [false, true] {
            let tool = json!({"type":"tool","id":"create","tool":"write","name":"write","state":{"status":"completed","input":{"filePath":"created.txt","content":AFTER},"time":{"start":1577836920000i64},"metadata":{"filepath":"created.txt","exists":exists}}});
            if version == 2 {
                let message = json!({"model":{"id":"fixture-model"},"content":[tool]});
                db.execute("UPDATE session_message SET data=?1", [message.to_string()])
                    .unwrap();
            } else {
                db.execute("UPDATE part SET data=?1", [tool.to_string()])
                    .unwrap();
            }
            let out = f.text(&["--agent=opencode", "--style=porcelain", "created.txt"]);
            let agent = if exists { "unknown" } else { "opencode" };
            assert_eq!(
                out.matches(&format!("agent \"{agent}\"\n")).count(),
                3,
                "v{version}, exists={exists}: {out}"
            );
        }
    }
}

#[test]
fn edit_window_follows_the_preimage_file_across_unrelated_commits_and_renames() {
    for renamed in [false, true] {
        let f = Fixture::new();
        fs::write(f.root.join("a.txt"), BEFORE).unwrap();
        f.commit("2020-01-01T00:11:00Z");
        fs::write(f.root.join("unrelated.txt"), "other change\n").unwrap();
        f.commit("2020-01-01T00:15:00Z");
        fs::write(f.root.join("a.txt"), AFTER).unwrap();
        let path = if renamed {
            fs::rename(f.root.join("a.txt"), f.root.join("renamed.txt")).unwrap();
            "renamed.txt"
        } else {
            "a.txt"
        };
        f.commit("2020-01-01T00:20:00Z");
        let mut rows = f.native("codex");
        rows[0]["payload"]["timestamp"] = json!("2020-01-01T00:11:30Z");
        rows[1]["timestamp"] = json!("2020-01-01T00:11:30Z");
        rows[2]["timestamp"] = json!("2020-01-01T00:12:00Z");
        rows[3]["timestamp"] = json!("2020-01-01T00:13:00Z");
        f.install("codex", &rows);
        let out = f.text(&["--agent=codex", "--style=porcelain", "-L2,2", path]);
        assert!(out.contains("agent \"codex\""), "renamed={renamed}: {out}");

        // The same bytes were also edited before the intervening restore.
        // That older evidence must not explain the new occurrence.
        f.install("codex", &f.native("codex"));
        let out = f.text(&["--agent=codex", "--style=porcelain", "-L2,2", path]);
        assert!(out.contains("agent \"unknown\""), "stale edit: {out}");
    }
}

#[test]
fn recreation_does_not_reuse_evidence_from_before_a_delete_or_emptying() {
    for deleted in [false, true] {
        let f = Fixture::new();
        if deleted {
            fs::remove_file(f.root.join("a.txt")).unwrap();
        } else {
            fs::write(f.root.join("a.txt"), "").unwrap();
        }
        f.commit("2020-01-01T00:11:00Z");
        fs::write(f.root.join("unrelated.txt"), "other change\n").unwrap();
        f.commit("2020-01-01T00:15:00Z");
        fs::write(f.root.join("a.txt"), AFTER).unwrap();
        f.commit("2020-01-01T00:20:00Z");
        let mut rows = f.native("codex");
        rows[2]["payload"]["item"]["changes"]["a.txt"] = json!({"type":"add","content":AFTER});
        f.install("codex", &rows);
        let out = f.text(&["--agent=codex", "--style=porcelain", "a.txt"]);
        assert!(
            !out.contains("agent \"codex\""),
            "stale creation, deleted={deleted}: {out}"
        );

        rows[2]["timestamp"] = json!("2020-01-01T00:12:00Z");
        rows[3]["timestamp"] = json!("2020-01-01T00:13:00Z");
        f.install("codex", &rows);
        let out = f.text(&["--agent=codex", "--style=porcelain", "a.txt"]);
        assert_eq!(
            out.matches("agent \"codex\"\n").count(),
            3,
            "fresh creation: {out}"
        );
    }
}

#[test]
fn dsh_compressed_generations_and_seed_prefix() {
    let f = Fixture::new();
    let mut rows = f.native("dsh");
    let path = f.install("dsh", &rows);
    let data = fs::read(&path).unwrap();
    let mut compressed = zstd::stream::encode_all(
        &data[..data.iter().position(|b| *b == b'\n').unwrap() + 1],
        1,
    )
    .unwrap();
    compressed.extend(
        zstd::stream::encode_all(
            &data[data.iter().position(|b| *b == b'\n').unwrap() + 1..],
            1,
        )
        .unwrap(),
    );
    fs::write(path.with_file_name("session.v4.jsonl.zstd"), compressed).unwrap();
    assert!(
        f.text(&["--agent=dsh", "--style=porcelain", "-L2,2", "a.txt"])
            .contains("agent \"dsh\"")
    );
    rows.push(json!({"type":"session/end-seed","seq":3,"time":1577836980001i64,"data":{"inherited":true}}));
    let data = rows.iter().map(|r| format!("{r}\n")).collect::<String>();
    fs::write(
        path.with_file_name("session.v4.jsonl.zstd"),
        zstd::stream::encode_all(data.as_bytes(), 1).unwrap(),
    )
    .unwrap();
    assert!(
        f.text(&["--agent=dsh", "--style=porcelain", "-L2,2", "a.txt"])
            .contains("agent \"unknown\"")
    );
}

#[test]
fn output_preserves_unusual_source_bytes_and_non_repository_warning() {
    let f = Fixture::new();
    fs::write(f.root.join("a.txt"), b"\xff\t\x1b\r\nlast").unwrap();
    f.commit("2020-01-01T00:20:00Z");
    let out = f.text(&["--agent=codex", "--style=porcelain", "a.txt"]);
    assert!(out.contains("source-bytes ff091b0d\n"));
    assert!(out.contains("source \"last\""));
    let o = Command::new(env!("CARGO_BIN_EXE_agent-blame"))
        .current_dir(Path::new(&f.stores))
        .arg("anything")
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("future release"));
}
