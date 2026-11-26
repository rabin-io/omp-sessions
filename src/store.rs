use crate::model::{
    CACHE_VERSION, CachedSessionData, Paths, Session, cmp_f64_desc, parse_iso_timestamp, short_id,
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct LoadProgress {
    pub stage: &'static str,
    pub done: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: HashMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    mtime_ns: i64,
    size: u64,
    #[serde(default)]
    subagent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<CachedSessionData>,
}

#[derive(Debug, Default)]
pub struct DeleteOutcome {
    pub deleted: Vec<Session>,
    pub skipped_live: Vec<Session>,
    pub errors: Vec<String>,
}

fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|c| c.load(Ordering::Relaxed))
}

fn canonicalize_existing(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

fn session_path_from_pty(
    paths: &Paths,
    pts_name: &str,
    sessions_prefix: &str,
) -> Option<PathBuf> {
    let pty_file = paths.terminal_sessions_dir.join(pts_name);
    let content = fs::read_to_string(pty_file).ok()?;
    let candidate = content.lines().nth(1)?.trim();
    if !candidate.ends_with(".jsonl") {
        return None;
    }
    let real = canonicalize_existing(Path::new(candidate))?;
    let real_str = real.to_string_lossy();
    real_str.starts_with(sessions_prefix).then_some(real)
}

pub fn open_session_files(paths: &Paths, cancel: Option<&AtomicBool>) -> HashSet<PathBuf> {
    let mut open_files = HashSet::new();
    let Ok(sessions_root) = fs::canonicalize(&paths.sessions_dir) else {
        return open_files;
    };
    let mut sessions_prefix = sessions_root.to_string_lossy().into_owned();
    if !sessions_prefix.ends_with(std::path::MAIN_SEPARATOR) {
        sessions_prefix.push(std::path::MAIN_SEPARATOR);
    }

    let Ok(proc_entries) = fs::read_dir(&paths.proc_dir) else {
        return open_files;
    };

    let has_term_sessions = paths.terminal_sessions_dir.is_dir();

    for entry in proc_entries.flatten() {
        if is_cancelled(cancel) {
            return open_files;
        }
        let pid_os = entry.file_name();
        let Some(pid_str) = pid_os.to_str() else {
            continue;
        };
        if pid_str.is_empty() || !pid_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }

        let pid_path = entry.path();
        let fd_dir = pid_path.join("fd");
        let Ok(fds) = fs::read_dir(&fd_dir) else {
            continue;
        };

        let mut is_omp = false;
        if has_term_sessions {
            if let Ok(comm) = fs::read_to_string(pid_path.join("comm")) {
                if comm.trim() == "omp" {
                    if let Ok(cmdline) = fs::read(pid_path.join("cmdline")) {
                        is_omp = !cmdline
                            .windows(b"__omp_worker_".len())
                            .any(|w| w == b"__omp_worker_");
                    }
                }
            }
        }

        let mut pty_checked = false;
        for fd_entry in fds.flatten() {
            let Ok(target) = fs::read_link(fd_entry.path()) else {
                continue;
            };
            let target_str = target.to_string_lossy();
            if target_str.ends_with(".jsonl") {
                if let Some(real) = canonicalize_existing(&target) {
                    if real.to_string_lossy().starts_with(&sessions_prefix) {
                        open_files.insert(real);
                    }
                }
            } else if is_omp && !pty_checked && target_str.starts_with("/dev/pts/") {
                pty_checked = true;
                if let Some(pts_num) = target_str.strip_prefix("/dev/pts/") {
                    if !pts_num.is_empty() && pts_num.bytes().all(|b| b.is_ascii_digit()) {
                        let pts_name = format!("pts-{pts_num}");
                        if let Some(session_real) =
                            session_path_from_pty(paths, &pts_name, &sessions_prefix)
                        {
                            open_files.insert(session_real);
                        }
                    }
                }
            }
        }
    }

    open_files
}

fn discover_session_files(sessions_dir: &Path) -> Vec<PathBuf> {
    let Ok(project_dirs) = fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for proj_entry in project_dirs.flatten() {
        let proj_path = proj_entry.path();
        if !proj_path.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&proj_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if !name.starts_with('.') && name.ends_with(".jsonl") {
                let p = entry.path();
                if p.is_file() {
                    files.push(p);
                }
            }
        }
    }
    files
}

fn load_cache(cache_file: Option<&Path>) -> HashMap<String, CacheEntry> {
    let Some(path) = cache_file else {
        return HashMap::new();
    };
    let Ok(bytes) = fs::read(path) else {
        return HashMap::new();
    };
    let Ok(parsed) = serde_json::from_slice::<CacheFile>(&bytes) else {
        return HashMap::new();
    };
    if parsed.version != CACHE_VERSION {
        return HashMap::new();
    }
    parsed.entries
}

fn save_cache(cache_file: Option<&Path>, entries: HashMap<String, CacheEntry>) {
    let Some(path) = cache_file else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_path = parent.join(format!(
        ".omp-sessions-cache-{}-{}.tmp",
        std::process::id(),
        nanos
    ));
    let payload = CacheFile {
        version: CACHE_VERSION,
        entries,
    };
    let write_res = (|| -> std::io::Result<()> {
        let mut f = File::create(&tmp_path)?;
        serde_json::to_writer(&mut f, &payload)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        f.flush()?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
}

fn is_interesting_line(line: &str) -> bool {
    let has_key = line.contains("\"type\"") || line.contains("\"role\"");
    if !has_key {
        return false;
    }
    line.contains("\"session\"")
        || line.contains("\"title\"")
        || line.contains("\"title_change\"")
        || line.contains("\"user\"")
}

fn is_truthy_json(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn extract_user_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(items)) => {
            let mut parts: Vec<&str> = Vec::new();
            for item in items {
                if let Some(obj) = item.as_object() {
                    if obj.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = obj.get("text").and_then(Value::as_str) {
                            parts.push(text);
                        }
                    }
                }
            }
            parts.join(" ").trim().to_string()
        }
        _ => String::new(),
    }
}

pub fn parse_transcript(jsonl: &Path, sid: &str, mtime: f64) -> Option<Session> {
    let file = File::open(jsonl).ok()?;
    let mut reader = BufReader::with_capacity(32 * 1024, file);
    let mut session = Session::new(sid.to_string(), jsonl.to_string_lossy().into_owned(), mtime);
    let mut raw_buf = Vec::with_capacity(4096);

    loop {
        raw_buf.clear();
        match reader.read_until(b'\n', &mut raw_buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
        let line = String::from_utf8_lossy(&raw_buf);
        if !is_interesting_line(&line) {
            continue;
        }
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(ttype) = obj.get("type").and_then(Value::as_str) else {
            continue;
        };
        match ttype {
            "session" => {
                if obj.get("parentSession").is_some_and(is_truthy_json) {
                    return None;
                }
                if let Some(cwd) = obj.get("cwd").and_then(Value::as_str) {
                    session.project = cwd.to_string();
                }
            }
            "title" => {
                if session.title.is_empty() {
                    if let Some(title) = obj.get("title").and_then(Value::as_str) {
                        session.title = title.to_string();
                    }
                }
            }
            "title_change" => {
                if let Some(title) = obj.get("title").and_then(Value::as_str) {
                    if !title.is_empty() {
                        session.title = title.to_string();
                    }
                }
            }
            "message" => {
                let Some(msg) = obj.get("message").and_then(Value::as_object) else {
                    continue;
                };
                if msg.get("role").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                let text = extract_user_text(msg.get("content"));
                if text.is_empty() {
                    continue;
                }
                session.msg_count += 1;
                if session.first_prompt.is_empty() {
                    session.first_prompt = text;
                    if let Some(ts_str) = obj.get("timestamp").and_then(Value::as_str) {
                        if let Some(parsed_ts) = parse_iso_timestamp(ts_str) {
                            session.first_ts = parsed_ts;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Some(session)
}

fn fill_db_titles(history_db: &Path, sessions: &mut HashMap<String, Session>) {
    if !history_db.exists() {
        return;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(conn) = Connection::open_with_flags(history_db, flags) else {
        return;
    };
    let Ok(mut stmt) = conn.prepare("SELECT session_id, title FROM session_titles") else {
        return;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return;
    };
    for row in rows.flatten() {
        let (sid, title) = row;
        if title.is_empty() {
            continue;
        }
        if let Some(sess) = sessions.get_mut(&sid) {
            if sess.title.is_empty() {
                sess.title = title;
            }
        }
    }
}

pub fn load_sessions<F>(
    paths: &Paths,
    mut on_progress: F,
    cancel: Option<&AtomicBool>,
) -> Vec<Session>
where
    F: FnMut(LoadProgress),
{
    if is_cancelled(cancel) {
        return Vec::new();
    }
    on_progress(LoadProgress {
        stage: "Discovering sessions",
        done: 0,
        total: 0,
    });

    let files = discover_session_files(&paths.sessions_dir);
    let total = files.len();
    if is_cancelled(cancel) {
        return Vec::new();
    }

    let old_cache = load_cache(paths.cache_file.as_deref());
    let mut new_cache: HashMap<String, CacheEntry> = HashMap::with_capacity(total);
    let mut cache_dirty = false;
    let mut sessions: HashMap<String, Session> = HashMap::new();

    if total > 0 {
        on_progress(LoadProgress {
            stage: "Reading transcripts",
            done: 0,
            total,
        });
    }

    for (idx, jsonl) in files.iter().enumerate() {
        if is_cancelled(cancel) {
            return Vec::new();
        }
        let done = idx + 1;
        let Ok(meta) = fs::metadata(jsonl) else {
            on_progress(LoadProgress {
                stage: "Reading transcripts",
                done,
                total,
            });
            continue;
        };
        let mtime_sec = meta.mtime();
        let mtime_nsec = meta.mtime_nsec();
        let mtime_ns = mtime_sec
            .saturating_mul(1_000_000_000)
            .saturating_add(mtime_nsec);
        let mtime_f64 = mtime_sec as f64 + (mtime_nsec as f64 / 1e9);
        let size = meta.size();

        let stem = jsonl
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let sid = stem
            .rsplit_once('_')
            .map(|(_, tail)| tail)
            .unwrap_or(stem)
            .to_string();

        if let Some(existing) = sessions.get(&sid) {
            if mtime_f64 <= existing.last_ts {
                on_progress(LoadProgress {
                    stage: "Reading transcripts",
                    done,
                    total,
                });
                continue;
            }
        }

        let key = jsonl.to_string_lossy().into_owned();
        let mut session: Option<Session> = None;
        let mut is_subagent = false;

        if let Some(cached) = old_cache.get(&key) {
            if cached.mtime_ns == mtime_ns && cached.size == size {
                if cached.subagent {
                    is_subagent = true;
                    new_cache.insert(key.clone(), cached.clone());
                } else if let Some(data) = &cached.data {
                    session = Some(Session::from(data.clone()));
                    new_cache.insert(key.clone(), cached.clone());
                }
            }
        }

        if session.is_none() && !is_subagent {
            cache_dirty = true;
            session = parse_transcript(jsonl, &sid, mtime_f64);
            match &session {
                None => {
                    new_cache.insert(
                        key,
                        CacheEntry {
                            mtime_ns,
                            size,
                            subagent: true,
                            data: None,
                        },
                    );
                }
                Some(s) => {
                    new_cache.insert(
                        key,
                        CacheEntry {
                            mtime_ns,
                            size,
                            subagent: false,
                            data: Some(CachedSessionData::from(s)),
                        },
                    );
                }
            }
        }

        if let Some(s) = session {
            sessions.insert(sid, s);
        }

        on_progress(LoadProgress {
            stage: "Reading transcripts",
            done,
            total,
        });
    }

    if is_cancelled(cancel) {
        return Vec::new();
    }

    if cache_dirty || new_cache.len() != old_cache.len() {
        save_cache(paths.cache_file.as_deref(), new_cache);
    }

    if paths.history_db.exists() {
        on_progress(LoadProgress {
            stage: "Loading titles",
            done: total,
            total,
        });
        fill_db_titles(&paths.history_db, &mut sessions);
    }

    if is_cancelled(cancel) {
        return Vec::new();
    }

    on_progress(LoadProgress {
        stage: "Checking live sessions",
        done: total,
        total,
    });
    let live = open_session_files(paths, cancel);
    if is_cancelled(cancel) {
        return Vec::new();
    }
    for s in sessions.values_mut() {
        if let Some(real) = canonicalize_existing(Path::new(&s.path)) {
            if live.contains(&real) {
                s.live = true;
            }
        }
    }

    let mut result: Vec<Session> = sessions.into_values().collect();
    result.sort_by(|a, b| cmp_f64_desc(a.last_ts, b.last_ts));
    result
}

pub fn delete_sessions(paths: &Paths, targets: &[Session]) -> DeleteOutcome {
    let live_files = open_session_files(paths, None);
    let mut outcome = DeleteOutcome::default();

    for target in targets {
        let mut s = target.clone();
        let is_live = s.live
            || canonicalize_existing(Path::new(&s.path))
                .is_some_and(|real| live_files.contains(&real));
        if is_live {
            s.live = true;
            outcome.skipped_live.push(s);
            continue;
        }

        let jsonl = Path::new(&s.path);
        match fs::remove_file(jsonl) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                outcome.errors.push(format!("{}: {e}", short_id(&s.id)));
                continue;
            }
        }

        if let Some(fname) = jsonl.file_name().and_then(|n| n.to_str()) {
            let lock_file = jsonl.with_file_name(format!(".{fname}.lock.os"));
            let _ = fs::remove_file(lock_file);
        }

        let sibling = jsonl.with_extension("");
        if sibling.is_dir() {
            let _ = fs::remove_dir_all(sibling);
        }
        outcome.deleted.push(s);
    }

    if outcome.deleted.is_empty() {
        return outcome;
    }

    if paths.history_db.exists() {
        let res = (|| -> rusqlite::Result<()> {
            let mut conn = Connection::open(&paths.history_db)?;
            conn.busy_timeout(Duration::from_secs(5))?;
            let tx = conn.transaction()?;
            {
                let mut del_history = tx.prepare("DELETE FROM history WHERE session_id = ?1")?;
                let mut del_titles =
                    tx.prepare("DELETE FROM session_titles WHERE session_id = ?1")?;
                let mut del_recaps =
                    tx.prepare("DELETE FROM session_recaps WHERE session_id = ?1")?;
                for s in &outcome.deleted {
                    del_history.execute([&s.id])?;
                    del_titles.execute([&s.id])?;
                    del_recaps.execute([&s.id])?;
                }
            }
            tx.commit()
        })();
        if let Err(e) = res {
            outcome.errors.push(format!("history.db: {e}"));
        }
    }

    if paths.transcripts_db.exists() {
        let res = (|| -> rusqlite::Result<()> {
            let mut conn = Connection::open(&paths.transcripts_db)?;
            conn.busy_timeout(Duration::from_secs(5))?;
            let tx = conn.transaction()?;
            {
                let mut del_msgs = tx.prepare(
                    "DELETE FROM messages WHERE file IN (SELECT id FROM files WHERE session_id = ?1)",
                )?;
                let mut del_files = tx.prepare("DELETE FROM files WHERE session_id = ?1")?;
                for s in &outcome.deleted {
                    del_msgs.execute([&s.id])?;
                    del_files.execute([&s.id])?;
                }
            }
            tx.commit()
        })();
        if let Err(e) = res {
            outcome.errors.push(format!("transcripts.db: {e}"));
        }
    }

    outcome
}
