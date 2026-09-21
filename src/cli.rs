use clap::{Parser, ValueEnum};
use std::ffi::OsString;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, ValueEnum)]
pub enum Agent {
    Claude,
    Codex,
    Omp,
    Pi,
    Dsh,
    Opencode,
}

impl Agent {
    pub const ALL: [Self; 6] = [
        Self::Claude,
        Self::Codex,
        Self::Omp,
        Self::Pi,
        Self::Dsh,
        Self::Opencode,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Omp => "omp",
            Self::Pi => "pi",
            Self::Dsh => "dsh",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum Style {
    #[default]
    Default,
    Git,
    Porcelain,
}

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Find the coding agent behind committed lines",
    override_usage = "agent-blame [OPTIONS] [<COMMIT>] [--] <FILE>",
    after_help = "Uses system git. Supports Git's -L syntax and automatic whole-file renames.\nOther git blame options, including -M, -C and -w, are rejected.\n\nPorcelain: one record per line, beginning with '<oid> <original> <final> 1'.\nMetadata is 'key value'; text values are JSON strings (including \"unknown\").\nThe final 'source <JSON string>' field closes each record. Times are RFC 3339.\nFor non-UTF-8 bytes, source-bytes/filename-bytes contain lowercase hex instead.\n\nAgent stores honor CODEX_HOME, CLAUDE_CONFIG_DIR, PI_CODING_AGENT_DIR,\nOMP_CONFIG_DIR, DSH_SESSIONS_DIR, DSH_HOME and XDG_DATA_HOME. No persistent index is created."
)]
pub struct Cli {
    /// Analyze only these clients (claude,codex,omp,pi,dsh,opencode)
    #[arg(long, value_delimiter = ',', value_enum)]
    pub agent: Vec<Agent>,
    /// Git author name or email; any selector matches (defaults to git user identity)
    #[arg(long, value_delimiter = ',', conflicts_with = "everyone")]
    pub me: Vec<String>,
    /// Attempt local attribution for every Git author
    #[arg(long)]
    pub everyone: bool,
    /// After backward search fails, accept the earliest later matching edit
    #[arg(long)]
    pub timeless: bool,
    /// Output format
    #[arg(long, value_enum, default_value = "default")]
    pub style: Style,
    /// Restrict blamed lines; passed unchanged to Git (repeatable)
    #[arg(short = 'L', action = clap::ArgAction::Append)]
    pub ranges: Vec<String>,
    /// Print completion script and exit
    #[arg(long, value_enum)]
    pub completions: Option<clap_complete::Shell>,
    /// Print scan counts and timing to stderr
    #[arg(long)]
    pub stats: bool,
    #[arg(value_name = "COMMIT/FILE", num_args = 1..=2, required_unless_present = "completions")]
    pub operands: Vec<OsString>,
}
