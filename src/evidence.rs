use crate::{cli::Agent, git::Target};
use bstr::ByteSlice;
use similar::{Algorithm, DiffTag};
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Debug)]
pub enum Change {
    /// Both sides are observed file contents, not a Write argument used as a preimage.
    Snapshot {
        before: Vec<u8>,
        after: Vec<u8>,
    },
    Unified(String),
    /// Applied contextual fragments, whose producer did not retain line coordinates.
    Fragments(Vec<(String, String)>),
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Edit {
    pub time: Option<i64>,
    pub model: Option<String>,
    pub change: Change,
}

#[derive(Clone, Debug, Default)]
pub struct History {
    pub edits: Vec<Edit>,
    pub models: BTreeSet<String>,
    pub updated: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attribution {
    pub agent: Agent,
    pub models: Vec<String>,
    pub time: Option<i64>,
    pub session: Option<String>,
    pub rank: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub old_start: usize,
    pub new_start: usize,
    pub old: Vec<Vec<u8>>,
    pub new: Vec<Vec<u8>>,
}

pub fn lines(data: &[u8]) -> Vec<&[u8]> {
    data.lines_with_terminator().collect()
}

pub fn diff(before: &[u8], after: &[u8]) -> Vec<Hunk> {
    let old = lines(before);
    let new = lines(after);
    similar::capture_diff_slices(Algorithm::Myers, &old, &new)
        .into_iter()
        .filter_map(|op| {
            let (tag, a, b) = op.as_tag_tuple();
            (tag != DiffTag::Equal).then(|| Hunk {
                old_start: a.start,
                new_start: b.start,
                old: old[a].iter().map(|s| s.to_vec()).collect(),
                new: new[b].iter().map(|s| s.to_vec()).collect(),
            })
        })
        .collect()
}

fn range(s: &str) -> Option<(usize, usize)> {
    let (start, count) = s.split_once(',').unwrap_or((s, "1"));
    let start: usize = start.parse().ok()?;
    let count = count.parse().ok()?;
    Some((
        if count == 0 {
            start
        } else {
            start.checked_sub(1)?
        },
        count,
    ))
}

pub fn unified(patch: &str) -> Option<Vec<Hunk>> {
    let mut result = Vec::new();
    let mut iter = patch.as_bytes().split_inclusive(|b| *b == b'\n').peekable();
    while let Some(line) = iter.next() {
        if !line.starts_with(b"@@ ") {
            continue;
        }
        let head = std::str::from_utf8(line).ok()?;
        let fields: Vec<_> = head.split_whitespace().collect();
        if fields.len() < 4 || fields[3] != "@@" {
            return None;
        }
        let (old_start, old_count) = range(fields[1].strip_prefix('-')?)?;
        let (new_start, new_count) = range(fields[2].strip_prefix('+')?)?;
        let mut old = Vec::<Vec<u8>>::new();
        let mut new = Vec::<Vec<u8>>::new();
        let mut last = b' ';
        while let Some(next) = iter.peek() {
            if next.starts_with(b"\\ No newline at end of file") {
                iter.next();
                if last != b'+' && old.last_mut()?.pop()? != b'\n' {
                    return None;
                }
                if last != b'-' && new.last_mut()?.pop()? != b'\n' {
                    return None;
                }
                continue;
            }
            if old.len() == old_count && new.len() == new_count {
                break;
            }
            let data = iter.next()?;
            last = *data.first()?;
            if !matches!(last, b' ' | b'-' | b'+') {
                return None;
            }
            let data = data[1..].to_vec();
            if last != b'+' {
                old.push(data.clone());
            }
            if last != b'-' {
                new.push(data);
            }
            if old.len() > old_count || new.len() > new_count {
                return None;
            }
        }
        if old.len() != old_count || new.len() != new_count {
            return None;
        }
        result.push(Hunk {
            old_start,
            new_start,
            old,
            new,
        });
    }
    (!result.is_empty()).then_some(result)
}

fn apply_hunks(base: &[u8], hunks: &[Hunk]) -> Option<Vec<u8>> {
    let source = lines(base);
    let mut result = Vec::<u8>::new();
    let mut cursor = 0;
    let mut new_cursor = 0;
    for h in hunks {
        if h.old_start < cursor || h.old_start + h.old.len() > source.len() {
            return None;
        }
        if source[h.old_start..h.old_start + h.old.len()]
            .iter()
            .copied()
            .ne(h.old.iter().map(Vec::as_slice))
        {
            return None;
        }
        new_cursor += h.old_start - cursor;
        if new_cursor != h.new_start {
            return None;
        }
        for l in &source[cursor..h.old_start] {
            result.extend_from_slice(l);
        }
        for l in &h.new {
            result.extend_from_slice(l);
        }
        cursor = h.old_start + h.old.len();
        new_cursor += h.new.len();
    }
    for l in &source[cursor..] {
        result.extend_from_slice(l);
    }
    Some(result)
}

impl Change {
    pub fn apply(&self, base: &[u8]) -> Option<Vec<u8>> {
        match self {
            Self::Snapshot { before, after } => (before == base).then(|| after.clone()),
            Self::Unified(patch) => apply_hunks(base, &unified(patch)?),
            Self::Fragments(fragments) => {
                let mut state = base.to_vec();
                for (old, new) in fragments {
                    if old.is_empty() || old == new {
                        return None;
                    }
                    let old_lines: Vec<_> = old.as_bytes().split(|b| *b == b'\n').collect();
                    let source = lines(&state);
                    let positions: Vec<_> = source
                        .windows(old_lines.len())
                        .enumerate()
                        .filter_map(|(i, window)| {
                            window
                                .iter()
                                .zip(&old_lines)
                                .all(|(a, b)| a.strip_suffix(b"\n").unwrap_or(a) == *b)
                                .then_some(i)
                        })
                        .collect();
                    if positions.len() != 1 {
                        return None;
                    }
                    let pos = positions[0];
                    // These producers discard EOF markers. Never infer the final newline.
                    if pos + old_lines.len() >= source.len() {
                        return None;
                    }
                    let mut result = Vec::new();
                    for l in &source[..pos] {
                        result.extend_from_slice(l);
                    }
                    result.extend_from_slice(new.as_bytes());
                    result.push(b'\n');
                    for l in &source[pos + old_lines.len()..] {
                        result.extend_from_slice(l);
                    }
                    state = result;
                }
                Some(state)
            }
            Self::Unknown => None,
        }
    }

    fn hunks(&self, base: &[u8]) -> Option<Vec<Hunk>> {
        match self {
            Self::Snapshot { before, after } => Some(diff(before, after)),
            Self::Unified(patch) => Some(
                unified(patch)?
                    .into_iter()
                    .flat_map(|h| {
                        let before = h.old.concat();
                        let after = h.new.concat();
                        diff(&before, &after).into_iter().map(move |mut d| {
                            d.old_start += h.old_start;
                            d.new_start += h.new_start;
                            d
                        })
                    })
                    .collect(),
            ),
            Self::Fragments(_) => Some(diff(base, &self.apply(base)?)),
            Self::Unknown => None,
        }
    }
}

pub struct Session<'a> {
    pub agent: Agent,
    pub id: Option<&'a str>,
    pub started: Option<i64>,
    pub updated: Option<i64>,
}

#[derive(Default)]
pub struct Findings {
    pub matched: HashMap<usize, Attribution>,
    pub blocked: HashMap<usize, i64>,
}

pub fn attribute(
    target: &Target,
    requested: &[usize],
    session: &Session<'_>,
    history: &History,
    forward: bool,
) -> Findings {
    let mut result = Findings::default();
    if target.merge {
        return result;
    }
    let started = session.started;
    let updated = history.updated.or(session.updated);
    if !forward && !started.is_some_and(|t| t <= target.commit_time) {
        return result;
    }
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let eligible = |e: &&Edit| match e.time {
        Some(t) => {
            if forward {
                t > target.commit_time && t <= now
            } else {
                t <= target.commit_time && target.parent_time.is_none_or(|p| t > p)
            }
        }
        None => {
            if forward {
                started.is_some_and(|t| t > target.commit_time && t <= now)
            } else {
                updated.is_some_and(|t| t <= target.commit_time)
                    && target
                        .parent_time
                        .is_none_or(|p| started.is_some_and(|t| t > p))
            }
        }
    };
    let edits: Vec<_> = history.edits.iter().filter(eligible).collect();
    let committed = diff(&target.before, &target.after);
    let changed: BTreeSet<_> = committed
        .iter()
        .flat_map(|h| h.new_start..h.new_start + h.new.len())
        .collect();

    // A contiguous, applied history that reconstructs the exact commit is sufficient
    // at session granularity. Missing preimages or unexplained state transitions break it.
    let mut state = target.before.clone();
    let mut owners = vec![None; lines(&state).len()];
    let mut complete = !edits.is_empty();
    for (index, edit) in edits.iter().enumerate() {
        let Some(next) = edit.change.apply(&state) else {
            complete = false;
            break;
        };
        let old = lines(&state);
        let new = lines(&next);
        let mut next_owners = Vec::new();
        for op in similar::capture_diff_slices(Algorithm::Myers, &old, &new) {
            let (tag, a, b) = op.as_tag_tuple();
            if tag == DiffTag::Equal {
                next_owners.extend_from_slice(&owners[a]);
            } else {
                next_owners.extend(std::iter::repeat_n(Some(index), b.len()));
            }
        }
        owners = next_owners;
        state = next;
        if forward && state == target.after {
            break;
        }
    }
    if complete && state == target.after {
        let models: Vec<_> = edits
            .iter()
            .map(|e| e.model.clone().unwrap_or_else(|| "unknown".into()))
            .chain(history.models.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for &line in requested {
            if !changed.contains(&line) {
                continue;
            }
            if let Some(Some(owner)) = owners.get(line) {
                let rank = edits[*owner]
                    .time
                    .or(if forward { started } else { updated });
                if let Some(rank) = rank {
                    result.matched.insert(
                        line,
                        Attribution {
                            agent: session.agent,
                            models: models.clone(),
                            time: updated,
                            session: session.id.map(str::to_owned),
                            rank,
                        },
                    );
                }
            }
        }
        return result;
    }

    let mut ordered = edits;
    ordered.sort_by_key(|e| e.time.or(updated));
    if !forward {
        ordered.reverse();
    }
    for edit in ordered {
        let Some(rank) = edit.time.or(if forward { started } else { updated }) else {
            continue;
        };
        let Some(hunks) = edit.change.hunks(&target.before) else {
            if !forward {
                for &line in requested {
                    if !result.matched.contains_key(&line) {
                        result.blocked.entry(line).or_insert(rank);
                    }
                }
            }
            continue;
        };
        for hunk in hunks {
            // Compare the actual changed block, including both coordinates and bytes.
            // Context/touched ranges and a matching added string alone are not evidence.
            let matches = committed.contains(&hunk);
            for &line in requested {
                if result.matched.contains_key(&line) || result.blocked.contains_key(&line) {
                    continue;
                }
                if (hunk.new_start..hunk.new_start + hunk.new.len()).contains(&line) {
                    if matches {
                        result.matched.insert(
                            line,
                            Attribution {
                                agent: session.agent,
                                models: edit.model.clone().into_iter().collect(),
                                time: edit.time,
                                session: session.id.map(str::to_owned),
                                rank,
                            },
                        );
                    } else if !forward {
                        result.blocked.insert(line, rank);
                    }
                } else if !forward && hunk.old.len() != hunk.new.len() && line >= hunk.new_start {
                    result.blocked.insert(line, rank);
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(before: &str, after: &str) -> Target {
        Target {
            oid: "a".repeat(64),
            path: b"a.rs".to_vec(),
            parent_path: b"a.rs".to_vec(),
            before: before.into(),
            after: after.into(),
            author: "me".into(),
            email: "me@x".into(),
            author_time: 0,
            author_tz: "+0000".into(),
            commit_time: 100,
            parent_time: None,
            merge: false,
        }
    }
    fn run(t: &Target, changes: Vec<Change>) -> HashMap<usize, Attribution> {
        attribute(
            t,
            &[0, 1, 2, 3],
            &Session {
                agent: Agent::Omp,
                id: Some("native-id"),
                started: Some(10),
                updated: Some(90),
            },
            &History {
                edits: changes
                    .into_iter()
                    .map(|change| Edit {
                        time: Some(50),
                        model: Some("model".into()),
                        change,
                    })
                    .collect(),
                ..History::default()
            },
            false,
        )
        .matched
    }
    #[test]
    fn whole_write_does_not_claim_unchanged_lines() {
        let t = target("same\nold\n", "same\nnew\n");
        let result = run(
            &t,
            vec![Change::Snapshot {
                before: t.before.clone(),
                after: t.after.clone(),
            }],
        );
        assert_eq!(result.keys().copied().collect::<Vec<_>>(), vec![1]);
    }
    #[test]
    fn manual_edit_after_agent_stays_unknown() {
        let t = target("old\n", "manual\n");
        assert!(
            run(
                &t,
                vec![Change::Snapshot {
                    before: b"old\n".to_vec(),
                    after: b"agent\n".to_vec()
                }]
            )
            .is_empty()
        );
    }
    #[test]
    fn patch_context_and_identical_replacements_are_not_changes() {
        let patch = "@@ -1,3 +1,3 @@\n same\n-old\n+new\n tail\n";
        let t = target("same\nold\ntail\n", "same\nnew\ntail\n");
        let r = run(&t, vec![Change::Unified(patch.into())]);
        assert_eq!(r.keys().copied().collect::<Vec<_>>(), vec![1]);
    }
    #[test]
    fn eof_and_crlf_are_exact() {
        let p = Change::Unified(
            "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n"
                .into(),
        );
        assert_eq!(p.apply(b"old"), Some(b"new".to_vec()));
        assert_eq!(p.apply(b"old\n"), None);
        assert!(
            run(
                &target("old\r\n", "new\r\n"),
                vec![Change::Unified("@@ -1 +1 @@\n-old\n+new\n".into())]
            )
            .is_empty()
        );
    }
    #[test]
    fn successive_edits_reconstruct_session() {
        let t = target("a\nb\n", "c\nd\n");
        let r = run(
            &t,
            vec![
                Change::Unified("@@ -1 +1 @@\n-a\n+c\n".into()),
                Change::Unified("@@ -2 +2 @@\n-b\n+d\n".into()),
            ],
        );
        assert_eq!(r.len(), 2);
        assert_eq!(r[&0].time, Some(90));
    }
    #[test]
    fn merge_and_ambiguous_fragments_fail_closed() {
        let mut t = target("a\n", "b\n");
        t.merge = true;
        assert!(
            run(
                &t,
                vec![Change::Snapshot {
                    before: t.before.clone(),
                    after: t.after.clone()
                }]
            )
            .is_empty()
        );
        assert!(
            Change::Fragments(vec![("x".into(), "y".into())])
                .apply(b"x\nx\ntail\n")
                .is_none()
        );
    }
    #[test]
    fn unexplained_restore_does_not_resurrect_an_older_match() {
        let t = target("old\n", "new\n");
        assert!(
            run(
                &t,
                vec![
                    Change::Snapshot {
                        before: b"old\n".to_vec(),
                        after: b"new\n".to_vec()
                    },
                    Change::Snapshot {
                        before: b"new\n".to_vec(),
                        after: b"third\n".to_vec()
                    }
                ]
            )
            .is_empty()
        );
        assert!(
            run(
                &t,
                vec![
                    Change::Snapshot {
                        before: t.before.clone(),
                        after: t.after.clone()
                    },
                    Change::Unknown
                ]
            )
            .is_empty()
        );
    }

    #[test]
    fn tool_fallback_keeps_only_the_responsible_model_and_time() {
        let t = target("old\nsame\n", "new\nsame\n");
        let history = History {
            edits: vec![Edit {
                time: Some(50),
                model: Some("editing-model".into()),
                change: Change::Snapshot {
                    before: b"old\nelsewhere\n".to_vec(),
                    after: b"new\nelsewhere\n".to_vec(),
                },
            }],
            models: BTreeSet::from(["other-model".into()]),
            updated: Some(90),
        };
        let session = Session {
            agent: Agent::Pi,
            id: Some("s"),
            started: Some(10),
            updated: Some(90),
        };
        let found = attribute(&t, &[0], &session, &history, false).matched;
        assert_eq!(found[&0].models, vec!["editing-model"]);
        assert_eq!(found[&0].time, Some(50));
        let mut t = t;
        t.parent_time = Some(60);
        assert!(
            attribute(&t, &[0], &session, &history, false)
                .matched
                .is_empty()
        );
    }

    #[test]
    fn timeless_uses_the_first_matching_state_even_if_later_repeated() {
        let t = target("old\n", "new\n");
        let edits = [
            (110, "old\n", "new\n"),
            (120, "new\n", "old\n"),
            (130, "old\n", "new\n"),
        ]
        .into_iter()
        .map(|(time, before, after)| Edit {
            time: Some(time),
            model: None,
            change: Change::Snapshot {
                before: before.into(),
                after: after.into(),
            },
        })
        .collect();
        let h = History {
            edits,
            updated: Some(140),
            ..History::default()
        };
        let s = Session {
            agent: Agent::Omp,
            id: Some("s"),
            started: Some(105),
            updated: Some(140),
        };
        assert_eq!(attribute(&t, &[0], &s, &h, true).matched[&0].rank, 110);
    }
}
