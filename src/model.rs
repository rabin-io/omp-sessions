use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

pub const CACHE_VERSION: u32 = 1;
pub const PROMPT_PREVIEW_MAX: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub path: String,
    pub project: String,
    pub title: String,
    pub first_prompt: String,
    pub first_ts: f64,
    pub last_ts: f64,
    pub msg_count: u64,
    pub live: bool,
}

impl Session {
    pub fn new(id: String, path: String, mtime: f64) -> Self {
        Self {
            id,
            path,
            project: String::new(),
            title: String::new(),
            first_prompt: String::new(),
            first_ts: mtime,
            last_ts: mtime,
            msg_count: 0,
            live: false,
        }
    }

    pub fn project_name(&self) -> &str {
        if self.project.is_empty() {
            return "";
        }
        Path::new(&self.project)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.project)
    }

    pub fn searchable_text(&self) -> String {
        let mut out = String::with_capacity(
            self.title.len() + self.first_prompt.len() + self.project.len() + self.id.len() + 10,
        );
        for part in [&self.title, &self.first_prompt, &self.project, &self.id] {
            if !part.is_empty() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(part);
            }
        }
        if self.live {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str("live");
        }
        out
    }

    pub fn display_title(&self) -> String {
        if !self.title.is_empty() {
            return self.title.clone();
        }
        if !self.first_prompt.is_empty() {
            return self.first_prompt.replace('\n', " ");
        }
        "(no title)".to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSessionData {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub first_prompt: String,
    #[serde(default)]
    pub first_ts: f64,
    #[serde(default)]
    pub last_ts: f64,
    #[serde(default)]
    pub msg_count: u64,
}

impl From<&Session> for CachedSessionData {
    fn from(s: &Session) -> Self {
        Self {
            id: s.id.clone(),
            path: s.path.clone(),
            project: s.project.clone(),
            title: s.title.clone(),
            first_prompt: s.first_prompt.clone(),
            first_ts: s.first_ts,
            last_ts: s.last_ts,
            msg_count: s.msg_count,
        }
    }
}

impl From<CachedSessionData> for Session {
    fn from(d: CachedSessionData) -> Self {
        Self {
            id: d.id,
            path: d.path,
            project: d.project,
            title: d.title,
            first_prompt: d.first_prompt,
            first_ts: d.first_ts,
            last_ts: d.last_ts,
            msg_count: d.msg_count,
            live: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Date,
    Project,
    Status,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::Date => Self::Project,
            Self::Project => Self::Status,
            Self::Status => Self::Date,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Project => "project",
            Self::Status => "status",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub home_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub history_db: PathBuf,
    pub transcripts_db: PathBuf,
    pub terminal_sessions_dir: PathBuf,
    pub cache_file: Option<PathBuf>,
    pub proc_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let home_dir = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let omp_dir = home_dir.join(".omp").join("agent");
        let cache_base = env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".cache"));
        Self {
            sessions_dir: omp_dir.join("sessions"),
            history_db: omp_dir.join("history.db"),
            transcripts_db: omp_dir.join("transcripts.db"),
            terminal_sessions_dir: omp_dir.join("terminal-sessions"),
            cache_file: Some(cache_base.join("omp-sessions-cache.json")),
            proc_dir: PathBuf::from("/proc"),
            home_dir,
        }
    }
}

pub fn tokens_match(tokens: &[String], text: &str) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let lower = text.to_lowercase();
    tokens.iter().all(|tok| lower.contains(tok.as_str()))
}

#[cfg(test)]
pub fn multi_word_match(query: &str, text: &str) -> bool {
    let tokens: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    tokens_match(&tokens, text)
}

pub fn iso_time(ts: f64) -> String {
    if !ts.is_finite() || ts <= 0.0 {
        return String::new();
    }
    let secs = ts.floor() as i64;
    Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_default()
}

pub fn parse_iso_timestamp(ts: &str) -> Option<f64> {
    if ts.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return Some(dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_nanos()) / 1e9);
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%:z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(ts, fmt) {
            return Some(dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_nanos()) / 1e9);
        }
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(ts, fmt) {
            if let Some(dt) = Local.from_local_datetime(&ndt).earliest() {
                return Some(dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_nanos()) / 1e9);
            }
        }
    }
    None
}

pub fn shorten_home(path: &str, home_dir: &Path) -> String {
    if path.is_empty() {
        return String::new();
    }
    let p = Path::new(path);
    match p.strip_prefix(home_dir) {
        Ok(rel) => {
            if rel.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rel.display())
            }
        }
        Err(_) => path.to_string(),
    }
}

pub fn short_id(id: &str) -> &str {
    truncate_chars(id, 8)
}

pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

pub fn truncate_display_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let total_w: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total_w <= max_width {
        return s.to_string();
    }
    let budget = max_width.saturating_sub(1);
    let mut out = String::new();
    let mut width = 0usize;
    for ch in s.chars() {
        let ch_w = ch.width().unwrap_or(0);
        if width + ch_w > budget {
            break;
        }
        out.push(ch);
        width += ch_w;
    }
    out.push('…');
    out
}

pub fn cmp_f64_desc(a: f64, b: f64) -> std::cmp::Ordering {
    b.total_cmp(&a)
}
