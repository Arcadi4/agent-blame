use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};

#[derive(Debug)]
pub struct Repo {
    pub root: PathBuf,
    pub file: PathBuf,
    revision: OsString,
    pub worktrees: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct BlamedLine {
    pub oid: String,
    pub original: usize,
    pub final_line: usize,
    pub path: Vec<u8>,
    pub source: Vec<u8>,
}

#[derive(Debug)]
pub struct Target {
    pub oid: String,
    pub path: Vec<u8>,
    pub parent_path: Vec<u8>,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
    pub author: String,
    pub email: String,
    pub author_time: i64,
    pub author_tz: String,
    pub commit_time: i64,
    pub parent_time: Option<i64>,
    pub merge: bool,
}

pub fn os(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(bytes).as_ref())
    }
}

pub fn bytes(s: &OsStr) -> &[u8] {
    s.as_encoded_bytes()
}

pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(part),
        }
    }
    out
}

pub fn command(root: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat");
    c.args([
        "--literal-pathspecs",
        "-c",
        "core.quotePath=true",
        "-c",
        "color.ui=false",
    ]);
    c
}

fn checked(output: Output) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "git: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

impl Repo {
    pub fn open(operands: &[OsString]) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let out = command(&cwd)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("system git is required")?;
        if !out.status.success() {
            bail!(
                "use outside a Git worktree is not supported yet; it may be available in a future release"
            );
        }
        let root = PathBuf::from(os(out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout)));
        let file = PathBuf::from(operands.last().context("a file is required")?);
        let absolute = normalize(&if file.is_absolute() {
            file
        } else {
            cwd.join(file)
        });
        let file = absolute.strip_prefix(&root).map(Path::to_path_buf).map_err(|_| {
            anyhow::anyhow!(
                "target file '{}' belongs to a different Git worktree than the current directory ('{}'); run agent-blame from that repository",
                absolute.display(),
                root.display()
            )
        })?;
        ensure!(!file.as_os_str().is_empty(), "target must be a file");
        let mut rev = if operands.len() == 2 {
            operands[0].clone()
        } else {
            OsString::from("HEAD")
        };
        rev.push("^{commit}");
        let resolved = checked(
            command(&root)
                .args([
                    OsStr::new("rev-parse"),
                    OsStr::new("--verify"),
                    OsStr::new("--end-of-options"),
                    &rev,
                ])
                .output()?,
        )?;
        let revision = os(resolved.trim_ascii());
        let mut repo = Self {
            root: root.clone(),
            file,
            revision,
            worktrees: vec![root],
        };
        if let Ok(out) = command(&repo.root)
            .args(["worktree", "list", "--porcelain", "-z"])
            .output()
        {
            for part in out.stdout.split(|b| *b == 0) {
                if let Some(path) = part.strip_prefix(b"worktree ") {
                    repo.worktrees.push(PathBuf::from(os(path)));
                }
            }
        }
        repo.worktrees.sort();
        repo.worktrees.dedup();
        repo.ensure_clean()?;
        Ok(repo)
    }

    pub fn ensure_clean(&self) -> Result<()> {
        let tracked = command(&self.root)
            .args(["ls-files", "--error-unmatch", "--"])
            .arg(&self.file)
            .output()?;
        ensure!(
            tracked.status.success(),
            "target file is untracked or ignored; only committed tracked files are supported"
        );
        let clean = command(&self.root)
            .args([
                "diff",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD",
                "--",
            ])
            .arg(&self.file)
            .status()?;
        ensure!(
            clean.success(),
            "target file has staged or working-tree changes; commit or restore this file before analysis"
        );
        let mode = checked(
            command(&self.root)
                .args(["ls-files", "--stage", "--"])
                .arg(&self.file)
                .output()?,
        )?;
        ensure!(
            mode.starts_with(b"100644 ") || mode.starts_with(b"100755 "),
            "only regular tracked files are supported"
        );
        // Git diff honors assume-unchanged/skip-worktree. Verify this one file's
        // filtered bytes as well, without changing the user's index flags.
        let content = checked(
            command(&self.root)
                .arg("hash-object")
                .arg("--path")
                .arg(&self.file)
                .arg("--")
                .arg(&self.file)
                .output()?,
        )?;
        let indexed = mode
            .split(|b| *b == b' ')
            .nth(1)
            .context("missing index object")?;
        ensure!(
            content.trim_ascii() == indexed,
            "target file has working-tree changes; commit or restore this file before analysis"
        );
        Ok(())
    }

    pub fn identity(&self) -> Vec<String> {
        ["user.name", "user.email"]
            .into_iter()
            .filter_map(|key| {
                let out = command(&self.root)
                    .args(["config", "--get", key])
                    .output()
                    .ok()?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(out.stdout.trim_ascii()).into_owned())
                    .filter(|s| !s.is_empty())
            })
            .collect()
    }

    pub fn blame(&self, ranges: &[String]) -> Result<Vec<BlamedLine>> {
        let mut c = command(&self.root);
        c.args([
            "-c",
            "blame.ignoreRevsFile=",
            "blame",
            "--line-porcelain",
            "--no-progress",
            "--no-textconv",
        ]);
        for range in ranges {
            c.arg("-L").arg(range);
        }
        c.arg(&self.revision).arg("--").arg(&self.file);
        parse_blame(&checked(c.output()?)?)
    }

    pub fn target(&self, oid: &str, path: &[u8]) -> Result<Target> {
        let metadata = checked(
            command(&self.root)
                .args([
                    "show",
                    "-s",
                    "--format=%P%x00%an%x00%ae%x00%at%x00%ai%x00%ct",
                    oid,
                    "--",
                ])
                .output()?,
        )?;
        let fields: Vec<_> = metadata.trim_ascii_end().split(|b| *b == 0).collect();
        ensure!(fields.len() == 6, "unexpected Git commit metadata");
        let parents: Vec<_> = fields[0]
            .split(|b| *b == b' ')
            .filter(|s| !s.is_empty())
            .collect();
        let after = self.blob(oid, path)?;
        let mut parent_path = path.to_vec();
        let parent_time = if parents.len() == 1 {
            let output = checked(
                command(&self.root)
                    .args([
                        "show",
                        "-s",
                        "--format=%ct",
                        &String::from_utf8_lossy(parents[0]),
                        "--",
                    ])
                    .output()?,
            )?;
            Some(
                String::from_utf8_lossy(output.trim_ascii())
                    .parse::<i64>()?
                    .saturating_mul(1000)
                    .saturating_add(999),
            )
        } else {
            None
        };
        let before = if parents.len() == 1 {
            let parent = String::from_utf8_lossy(parents[0]);
            let changes = checked(
                command(&self.root)
                    .args([
                        "diff-tree",
                        "--no-commit-id",
                        "-r",
                        "-M",
                        "--name-status",
                        "-z",
                        &parent,
                        oid,
                        "--",
                    ])
                    .output()?,
            )?;
            let mut parts = changes.split(|b| *b == 0);
            let mut added = false;
            while let Some(status) = parts.next().filter(|s| !s.is_empty()) {
                let old = parts.next().context("truncated Git diff-tree")?;
                if status.starts_with(b"R") || status.starts_with(b"C") {
                    let new = parts.next().context("truncated Git rename")?;
                    if new == path {
                        parent_path = old.to_vec();
                    }
                } else if status == b"A" && old == path {
                    added = true;
                }
            }
            if added {
                vec![]
            } else {
                self.blob(&parent, &parent_path)?
            }
        } else {
            vec![]
        };
        Ok(Target {
            oid: oid.to_owned(),
            path: path.to_vec(),
            parent_path,
            before,
            after,
            author: String::from_utf8_lossy(fields[1]).into_owned(),
            email: String::from_utf8_lossy(fields[2]).into_owned(),
            author_time: String::from_utf8_lossy(fields[3]).parse()?,
            author_tz: String::from_utf8_lossy(fields[4])
                .split_whitespace()
                .last()
                .unwrap_or("+0000")
                .to_owned(),
            commit_time: String::from_utf8_lossy(fields[5])
                .parse::<i64>()?
                .saturating_mul(1000),
            parent_time,
            merge: parents.len() > 1,
        })
    }

    fn blob(&self, oid: &str, path: &[u8]) -> Result<Vec<u8>> {
        let mut object = OsString::from(format!("{oid}:"));
        object.push(os(path));
        checked(
            command(&self.root)
                .args([OsStr::new("cat-file"), OsStr::new("blob"), &object])
                .output()?,
        )
    }

    pub fn relative_history_path(&self, cwd: &Path, path: &str) -> Option<Vec<u8>> {
        let raw = Path::new(path.strip_prefix("file://").unwrap_or(path));
        let absolute = normalize(&if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            cwd.join(raw)
        });
        self.worktrees
            .iter()
            .filter_map(|root| absolute.strip_prefix(root).ok())
            .min_by_key(|p| p.components().count())
            .map(|p| bytes(p.as_os_str()).to_vec())
    }

    pub fn matches_project(&self, cwd: &Path) -> bool {
        let cwd = normalize(cwd);
        self.worktrees
            .iter()
            .any(|root| cwd.starts_with(root) || root.starts_with(&cwd))
    }
}

pub fn unquote_path(input: &[u8]) -> Result<Vec<u8>> {
    if !input.starts_with(b"\"") {
        return Ok(input.to_vec());
    }
    ensure!(input.ends_with(b"\""), "unterminated Git path");
    let mut out = Vec::new();
    let mut i = 1;
    while i + 1 < input.len() {
        let c = input[i];
        i += 1;
        if c != b'\\' {
            out.push(c);
            continue;
        }
        let c = input[i];
        i += 1;
        let escaped = match c {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'b' => 8,
            b'f' => 12,
            b'v' => 11,
            b'a' => 7,
            b'\\' | b'"' => c,
            b'0'..=b'7' => {
                ensure!(
                    i + 1 < input.len() - 1
                        && input[i..i + 2].iter().all(|b| (b'0'..=b'7').contains(b)),
                    "invalid Git octal path escape"
                );
                let value = (u16::from(c - b'0') * 64)
                    + (u16::from(input[i] - b'0') * 8)
                    + u16::from(input[i + 1] - b'0');
                i += 2;
                u8::try_from(value).context("invalid Git path byte")?
            }
            _ => bail!("invalid Git path escape"),
        };
        out.push(escaped);
    }
    Ok(out)
}

pub fn parse_blame(data: &[u8]) -> Result<Vec<BlamedLine>> {
    let mut records = Vec::new();
    let mut lines = data.split(|b| *b == b'\n');
    while let Some(header) = lines.next().filter(|s| !s.is_empty()) {
        let header = std::str::from_utf8(header)?;
        let parts: Vec<_> = header.split_whitespace().collect();
        ensure!(
            parts.len() >= 3 && parts[0].bytes().all(|b| b.is_ascii_hexdigit()),
            "unexpected Git blame header"
        );
        let original: usize = parts[1].parse()?;
        let final_line = parts[2].parse()?;
        ensure!(original > 0, "Git returned an invalid line number");
        let mut path = None;
        let mut source = None;
        for line in lines.by_ref() {
            if let Some(text) = line.strip_prefix(b"\t") {
                source = Some(text.to_vec());
                break;
            }
            if let Some(name) = line.strip_prefix(b"filename ") {
                path = Some(unquote_path(name)?);
            }
        }
        records.push(BlamedLine {
            oid: parts[0].to_owned(),
            original,
            final_line,
            path: path.context("missing blame filename")?,
            source: source.context("missing blame source")?,
        });
    }
    Ok(records)
}
