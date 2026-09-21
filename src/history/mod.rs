mod jsonl;
mod opencode;

use crate::{
    cli::Agent,
    evidence::{self, Attribution, History, Session},
    git::{Repo, Target},
};
use anyhow::Result;
use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
enum Source {
    Jsonl(PathBuf),
    OpenCode { db: PathBuf, version: u8 },
    OpenCodeFiles(PathBuf),
}

#[derive(Clone, Debug)]
struct Candidate {
    agent: Agent,
    id: Option<String>,
    cwd: PathBuf,
    started: Option<i64>,
    updated: Option<i64>,
    parent_file: Option<PathBuf>,
    parent_id: Option<String>,
    source: Source,
}

#[derive(Default)]
pub struct Stats {
    pub candidates: usize,
    pub histories: usize,
    pub records: usize,
    pub bytes: u64,
    pub errors: usize,
}
impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} project candidates, {} histories read, {} records, {:.2} MiB scanned, {} unreadable histories",
            self.candidates,
            self.histories,
            self.records,
            self.bytes as f64 / 1048576.0,
            self.errors
        )
    }
}

pub struct Search<'a> {
    repo: &'a Repo,
    candidates: Vec<Candidate>,
    cache: HashMap<(usize, Vec<u8>, Vec<u8>), History>,
    pub stats: Stats,
}

pub(super) fn database(path: &Path) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(std::time::Duration::from_millis(250))?;
    Ok(db)
}

pub(super) fn env_path(name: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(name).map(PathBuf::from).unwrap_or(default)
}

impl<'a> Search<'a> {
    pub fn new(repo: &'a Repo, agents: &[Agent]) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let mut candidates = Vec::new();
        let mut indexed = HashSet::new();
        let mut roots = Vec::new();
        for &agent in agents {
            match agent {
                Agent::Codex => {
                    let root = env_path("CODEX_HOME", home.join(".codex"));
                    if let Ok(entries) = std::fs::read_dir(&root) {
                        let mut dbs: Vec<_> = entries
                            .flatten()
                            .map(|e| e.path())
                            .filter(|p| {
                                p.file_name().is_some_and(|s| {
                                    s.to_string_lossy().starts_with("state_")
                                        && s.to_string_lossy().ends_with(".sqlite")
                                })
                            })
                            .collect();
                        dbs.sort();
                        if let Some(path) = dbs.last()
                            && let Ok(db) = database(path)
                            && let Ok(mut stmt) = db.prepare(
                                "SELECT id, rollout_path, cwd, created_at, updated_at FROM threads",
                            )
                            && let Ok(rows) = stmt.query_map([], |r| {
                                Ok((
                                    r.get::<_, String>(0)?,
                                    r.get::<_, String>(1)?,
                                    r.get::<_, String>(2)?,
                                    r.get::<_, i64>(3)?,
                                    r.get::<_, i64>(4)?,
                                ))
                            })
                        {
                            for row in rows.flatten() {
                                let (id, path, cwd, start, end) = row;
                                let path = PathBuf::from(path);
                                indexed.insert(path.clone());
                                if repo.matches_project(Path::new(&cwd)) && path.is_file() {
                                    let header = jsonl::header(agent, &path).ok().flatten();
                                    candidates.push(Candidate {
                                        agent,
                                        id: Some(id),
                                        cwd: cwd.into(),
                                        started: Some(start.saturating_mul(1000)),
                                        updated: Some(end.saturating_mul(1000) + 999),
                                        parent_file: None,
                                        parent_id: header.and_then(|h| h.parent_id),
                                        source: Source::Jsonl(path),
                                    });
                                }
                            }
                        }
                    }
                    roots.extend([
                        (agent, root.join("sessions")),
                        (agent, root.join("archived_sessions")),
                    ]);
                }
                Agent::Claude => roots.push((
                    agent,
                    env_path("CLAUDE_CONFIG_DIR", home.join(".claude")).join("projects"),
                )),
                Agent::Omp => {
                    let root = env_path("OMP_CONFIG_DIR", home.join(".omp"));
                    roots.extend([
                        (agent, root.join("agent/sessions")),
                        (agent, root.join("sessions")),
                        (agent, root.join("agent/custom-session-files")),
                    ]);
                }
                Agent::Pi => roots.push((
                    agent,
                    env_path("PI_CODING_AGENT_DIR", home.join(".pi/agent")).join("sessions"),
                )),
                Agent::Dsh => roots.push((
                    agent,
                    env_path(
                        "DSH_SESSIONS_DIR",
                        env_path("DSH_HOME", home.join(".dsh")).join("sessions"),
                    ),
                )),
                Agent::Opencode => {
                    let root =
                        env_path("XDG_DATA_HOME", home.join(".local/share")).join("opencode");
                    opencode::discover(repo, &root, &mut candidates);
                }
            }
        }
        let mut files = Vec::new();
        for (agent, root) in roots {
            let walker = ignore::WalkBuilder::new(root)
                .hidden(false)
                .ignore(false)
                .git_ignore(false)
                .git_global(false)
                .git_exclude(false)
                .follow_links(false)
                .build();
            for entry in walker
                .flatten()
                .filter(|e| e.file_type().is_some_and(|f| f.is_file()))
            {
                let path = entry.into_path();
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if (name.ends_with(".jsonl") || name.ends_with(".jsonl.zstd"))
                    && !indexed.contains(&path)
                {
                    files.push((agent, path));
                }
            }
        }
        let headers: Vec<_> = files
            .par_iter()
            .filter_map(|(agent, path)| jsonl::header(*agent, path).ok().flatten())
            .filter(|c| repo.matches_project(&c.cwd))
            .collect();
        candidates.extend(headers);
        let parents: HashMap<_, _> = candidates
            .iter()
            .filter_map(|c| match (&c.id, &c.source) {
                (Some(id), Source::Jsonl(path)) => Some(((c.agent, id.clone()), path.clone())),
                _ => None,
            })
            .collect();
        for c in &mut candidates {
            if c.parent_file.is_none() {
                c.parent_file = c
                    .parent_id
                    .as_ref()
                    .and_then(|id| parents.get(&(c.agent, id.clone())))
                    .cloned();
            }
        }
        // Prefer the highest DSH generation. It contains the same logical history;
        // old generations left behind by migrations must not win fallback search.
        let mut generations = HashMap::<PathBuf, PathBuf>::new();
        for c in &candidates {
            if c.agent == Agent::Dsh
                && let Source::Jsonl(path) = &c.source
            {
                let parent = path.parent().unwrap_or(path).to_path_buf();
                let existing = generations.entry(parent).or_insert_with(|| path.clone());
                if jsonl::generation(path) > jsonl::generation(existing) {
                    *existing = path.clone();
                }
            }
        }
        candidates.retain(|c| match &c.source {
            Source::Jsonl(path) if c.agent == Agent::Dsh => generations
                .get(path.parent().unwrap_or(path))
                .is_some_and(|p| p == path),
            _ => true,
        });
        candidates.sort_by(|a, b| {
            b.updated
                .cmp(&a.updated)
                .then_with(|| b.started.cmp(&a.started))
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut seen = HashSet::new();
        candidates.retain(|c| {
            c.id.as_ref()
                .is_none_or(|id| seen.insert((c.agent, id.clone())))
        });
        let stats = Stats {
            candidates: candidates.len(),
            ..Stats::default()
        };
        Ok(Self {
            repo,
            candidates,
            cache: HashMap::new(),
            stats,
        })
    }

    pub fn attribute(
        &mut self,
        target: &Target,
        requested: &[usize],
        timeless: bool,
    ) -> Result<HashMap<usize, Attribution>> {
        if target.merge {
            return Ok(HashMap::new());
        }
        let mut found = self.direction(target, requested, false)?;
        if timeless {
            let missing: Vec<_> = requested
                .iter()
                .copied()
                .filter(|n| !found.contains_key(n))
                .collect();
            if !missing.is_empty() {
                found.extend(self.direction(target, &missing, true)?);
            }
        }
        Ok(found)
    }

    fn direction(
        &mut self,
        target: &Target,
        requested: &[usize],
        forward: bool,
    ) -> Result<HashMap<usize, Attribution>> {
        let mut order: Vec<_> = (0..self.candidates.len()).collect();
        if forward {
            order.sort_by_key(|&i| (self.candidates[i].started, self.candidates[i].id.clone()));
        }
        let mut found: HashMap<usize, Attribution> = HashMap::new();
        let mut blocked: HashMap<usize, i64> = HashMap::new();
        for index in order {
            let candidate = &self.candidates[index];
            if forward {
                if candidate.updated.is_some_and(|t| t <= target.commit_time) {
                    continue;
                }
            } else if !candidate.started.is_some_and(|t| t <= target.commit_time) {
                continue;
            }
            if !forward
                && candidate
                    .updated
                    .zip(target.parent_time)
                    .is_some_and(|(end, parent)| end <= parent)
            {
                continue;
            }
            if requested.iter().all(|n| found.contains_key(n)) {
                let bounded = found.values().all(|a| {
                    if forward {
                        candidate.started.is_some_and(|t| t >= a.rank)
                    } else {
                        candidate.updated.is_some_and(|t| t <= a.rank)
                    }
                });
                if bounded {
                    break;
                }
            }
            let key = (index, target.path.clone(), target.parent_path.clone());
            if !self.cache.contains_key(&key) {
                self.stats.histories += 1;
                let history = match &candidate.source {
                    Source::Jsonl(path) => {
                        jsonl::load(self.repo, candidate, path, target, &mut self.stats)
                    }
                    _ => opencode::load(self.repo, candidate, target, &mut self.stats),
                };
                match history {
                    Ok(h) => {
                        self.cache.insert(key.clone(), h);
                    }
                    Err(_) => {
                        self.stats.errors += 1;
                        self.cache.insert(key.clone(), History::default());
                    }
                }
            }
            let history = &self.cache[&key];
            let session = Session {
                agent: candidate.agent,
                id: candidate.id.as_deref(),
                started: candidate.started,
                updated: history.updated,
            };
            let decisions = evidence::attribute(target, requested, &session, history, forward);
            for (line, time) in decisions.blocked {
                if !forward && found.get(&line).is_none_or(|hit| time >= hit.rank) {
                    found.remove(&line);
                    blocked
                        .entry(line)
                        .and_modify(|old| *old = (*old).max(time))
                        .or_insert(time);
                }
            }
            for (line, hit) in decisions.matched {
                if blocked.get(&line).is_some_and(|t| *t >= hit.rank) {
                    continue;
                }
                let replace = found.get(&line).is_none_or(|old| {
                    if forward {
                        hit.rank < old.rank
                    } else {
                        hit.rank > old.rank
                    }
                });
                if replace {
                    found.insert(line, hit);
                }
            }
        }
        Ok(found)
    }
}
