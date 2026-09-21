use crate::{
    cli::{Cli, Style},
    evidence::Attribution,
    git::{BlamedLine, Repo, Target},
};
use anyhow::Result;
use owo_colors::OwoColorize;
use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

type Key = (String, Vec<u8>);
type Results = HashMap<Key, HashMap<usize, Attribution>>;

pub fn timestamp(ms: Option<i64>) -> String {
    ms.and_then(|ms| OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok())
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| "unknown".into())
}
fn models(a: Option<&Attribution>) -> String {
    a.filter(|a| !a.models.is_empty())
        .map(|a| a.models.join(","))
        .unwrap_or_else(|| "unknown".into())
}
fn clean(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            // Tabs are meaningful source indentation and are safe to emit in
            // a terminal line. Escape the remaining controls so they cannot
            // split or manipulate terminal output.
            if ch.is_control() && ch != '\t' {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}
fn json(text: &str) -> String {
    serde_json::to_string(text).expect("strings serialize")
}
fn byte_field(out: &mut impl Write, name: &str, value: &[u8]) -> io::Result<()> {
    match std::str::from_utf8(value) {
        Ok(text) => writeln!(out, "{name} {}", json(text)),
        Err(_) => {
            write!(out, "{name}-bytes ")?;
            for byte in value {
                write!(out, "{byte:02x}")?;
            }
            writeln!(out)
        }
    }
}

pub fn render(
    cli: &Cli,
    repo: &Repo,
    lines: &[BlamedLine],
    targets: &HashMap<Key, Target>,
    results: &Results,
) -> Result<()> {
    repo.ensure_clean()?;
    let color = io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").as_deref() != Ok("dumb");
    let mut out = io::BufWriter::new(io::stdout().lock());
    let width = lines
        .iter()
        .map(|l| l.final_line)
        .max()
        .unwrap_or(1)
        .to_string()
        .len();
    if matches!(cli.style, Style::Default) {
        let title = format!("agent-blame  {}", clean(&repo.file.to_string_lossy()));
        if color {
            writeln!(out, "{}\n", title.bold())?;
        } else {
            writeln!(out, "{title}\n")?;
        }
    }
    let mut last_group = None;
    for line in lines {
        let key = (line.oid.clone(), line.path.clone());
        let target = &targets[&key];
        let a = results.get(&key).and_then(|m| m.get(&(line.original - 1)));
        let model = models(a);
        let agent = a.map(|a| a.agent.name()).unwrap_or("unknown");
        let session = a.and_then(|a| a.session.as_deref()).unwrap_or("unknown");
        let time = timestamp(a.and_then(|a| a.time));
        match cli.style {
            Style::Porcelain => {
                writeln!(
                    out,
                    "{} {} {} 1",
                    target.oid, line.original, line.final_line
                )?;
                writeln!(
                    out,
                    "author {}\nauthor-mail {}\nauthor-time {}\nauthor-tz {}\ncommitter-time {}",
                    json(&target.author),
                    json(&target.email),
                    target.author_time,
                    json(&target.author_tz),
                    target.commit_time / 1000
                )?;
                byte_field(&mut out, "filename", &line.path)?;
                writeln!(
                    out,
                    "model-id {}\nagent {}\ntime {}\nsession-id {}",
                    json(&model),
                    json(agent),
                    json(&time),
                    json(session)
                )?;
                byte_field(&mut out, "source", &line.source)?;
            }
            Style::Git => {
                let tz = &target.author_tz;
                let offset = if tz.len() == 5 {
                    let sign = if tz.starts_with('-') { -1 } else { 1 };
                    tz[1..3]
                        .parse::<i8>()
                        .ok()
                        .zip(tz[3..5].parse::<i8>().ok())
                        .and_then(|(h, m)| UtcOffset::from_hms(h * sign, m * sign, 0).ok())
                } else {
                    None
                };
                let date = OffsetDateTime::from_unix_timestamp(target.author_time)
                    .ok()
                    .map(|t| t.to_offset(offset.unwrap_or(UtcOffset::UTC)))
                    .and_then(|t| {
                        t.format(&time::macros::format_description!(
                            "[year]-[month]-[day] [hour]:[minute]:[second]"
                        ))
                        .ok()
                    })
                    .unwrap_or_else(|| "unknown".into());
                writeln!(
                    out,
                    "{} ({} {} {} {:>width$}) [{} {} {} {}] {}",
                    &line.oid[..line.oid.len().min(12)],
                    clean(&target.author),
                    date,
                    tz,
                    line.final_line,
                    clean(&model),
                    agent,
                    time,
                    clean(session),
                    clean(&String::from_utf8_lossy(&line.source))
                )?;
            }
            Style::Default => {
                let group = (
                    line.oid.clone(),
                    model.clone(),
                    agent.to_owned(),
                    session.to_owned(),
                    time.clone(),
                );
                if last_group.as_ref() != Some(&group) {
                    if last_group.is_some() {
                        writeln!(out)?;
                    }
                    let commit = &line.oid[..line.oid.len().min(12)];
                    let identity = format!("{} <{}>", clean(&target.author), clean(&target.email));
                    if color {
                        writeln!(out, "{}  {}", commit.yellow(), identity.dimmed())?;
                    } else {
                        writeln!(out, "{commit}  {identity}")?;
                    }
                    let label = format!(
                        "{agent} · {}  {}  session {}",
                        clean(&model),
                        time,
                        clean(session)
                    );
                    if color && a.is_some() {
                        writeln!(out, "{}", label.cyan())?;
                    } else if color {
                        writeln!(out, "{}", label.dimmed())?;
                    } else {
                        writeln!(out, "{label}")?;
                    }
                    last_group = Some(group);
                }
                let source = String::from_utf8_lossy(&line.source);
                if color {
                    writeln!(
                        out,
                        "{} │ {}",
                        format!("{:>width$}", line.final_line).dimmed(),
                        clean(&source)
                    )?;
                } else {
                    writeln!(out, "{:>width$} │ {}", line.final_line, clean(&source))?;
                }
            }
        }
    }
    out.flush()?;
    Ok(())
}
