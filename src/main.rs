mod cli;
mod evidence;
mod git;
mod history;
mod output;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use std::{collections::HashMap, io, time::Instant};

fn run() -> Result<()> {
    let cli = cli::Cli::parse();
    if let Some(shell) = cli.completions {
        clap_complete::generate(
            shell,
            &mut cli::Cli::command(),
            "agent-blame",
            &mut io::stdout(),
        );
        return Ok(());
    }
    let start = Instant::now();
    let repo = git::Repo::open(&cli.operands)?;
    let lines = repo.blame(&cli.ranges)?;
    let mut selectors = cli.me.clone();
    if !cli.everyone && selectors.is_empty() {
        selectors = repo.identity();
    }
    let mut targets = HashMap::new();
    for line in &lines {
        let key = (line.oid.clone(), line.path.clone());
        if let std::collections::hash_map::Entry::Vacant(e) = targets.entry(key) {
            let target = repo.target(&line.oid, &line.path)?;
            e.insert(target);
        }
    }
    let eligible =
        |t: &git::Target| cli.everyone || selectors.iter().any(|s| s == &t.author || s == &t.email);
    let agents = if cli.agent.is_empty() {
        cli::Agent::ALL.to_vec()
    } else {
        cli.agent.clone()
    };
    let mut histories = history::Search::new(
        &repo,
        if targets.values().any(&eligible) {
            &agents
        } else {
            &[]
        },
    )?;
    let mut results = HashMap::new();
    for (key, target) in &targets {
        let requested: Vec<_> = lines
            .iter()
            .filter(|l| (&l.oid, &l.path) == (&key.0, &key.1))
            .map(|l| l.original - 1)
            .collect();
        let found = if eligible(target) {
            histories.attribute(target, &requested, cli.timeless)?
        } else {
            HashMap::new()
        };
        results.insert(key.clone(), found);
    }
    output::render(&cli, &repo, &lines, &targets, &results)?;
    if cli.stats {
        eprintln!(
            "agent-blame: {} lines, {} originating commit/files; {}; elapsed {:.3}s",
            lines.len(),
            targets.len(),
            histories.stats,
            start.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        if e.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
        {
            return;
        }
        eprintln!("agent-blame: warning: {e:#}");
        std::process::exit(1);
    }
}
