use crate::app::{App, ConfirmDelete, Focus, Severity};
use crate::model::{Paths, Session, SortMode, multi_word_match, parse_iso_timestamp, shorten_home};
use crate::store::{delete_sessions, load_sessions, open_session_files, parse_transcript};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rusqlite::Connection;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl_key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn make_paths(root: &Path) -> Paths {
    let omp = root.join(".omp/agent");
    fs::create_dir_all(omp.join("sessions")).unwrap();
    fs::create_dir_all(omp.join("terminal-sessions")).unwrap();
    fs::create_dir_all(root.join("proc")).unwrap();
    Paths {
        home_dir: root.to_path_buf(),
        sessions_dir: omp.join("sessions"),
        history_db: omp.join("history.db"),
        transcripts_db: omp.join("transcripts.db"),
        terminal_sessions_dir: omp.join("terminal-sessions"),
        cache_file: Some(root.join("cache/omp-sessions-cache.json")),
        proc_dir: root.join("proc"),
    }
}

fn make_session(id: &str, project: &str, title: &str, last_ts: f64, live: bool) -> Session {
    Session {
        id: id.to_string(),
        path: format!("/tmp/{id}.jsonl"),
        project: project.to_string(),
        title: title.to_string(),
        first_prompt: format!("prompt for {id}"),
        first_ts: last_ts - 60.0,
        last_ts,
        msg_count: 3,
        live,
    }
}

fn create_history_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            prompt TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL DEFAULT 0,
            cwd TEXT,
            session_id TEXT,
            use_count INTEGER NOT NULL DEFAULT 1
        );
        CREATE VIRTUAL TABLE history_fts USING fts5(prompt, content='history', content_rowid='id');
        CREATE TRIGGER history_ai AFTER INSERT ON history BEGIN
            INSERT INTO history_fts(rowid, prompt) VALUES (new.id, new.prompt);
        END;
        CREATE TRIGGER history_ad AFTER DELETE ON history BEGIN
            INSERT INTO history_fts(history_fts, rowid, prompt) VALUES ('delete', old.id, old.prompt);
        END;
        CREATE TABLE session_titles (
            session_id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            updated_at INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE session_recaps (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            cwd TEXT NOT NULL,
            recap TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )
    .unwrap();
}

fn create_transcripts_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE files (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            session_id TEXT NOT NULL,
            cwd TEXT NOT NULL,
            indexed INTEGER NOT NULL,
            messages INTEGER NOT NULL
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY,
            file INTEGER NOT NULL,
            role TEXT NOT NULL,
            at INTEGER NOT NULL,
            text TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE messages_fts USING fts5(text, content='messages', content_rowid='id');
        CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
        END;
        CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
        END;
        "#,
    )
    .unwrap();
}

#[test]
fn parse_transcript_extracts_metadata_and_skips_subagents() {
    let tmp = TempDir::new().unwrap();
    let main_path = tmp.path().join("2026-10-04_abc12345.jsonl");
    fs::write(
        &main_path,
        concat!(
            "{\"type\":\"session\",\"cwd\":\"/home/u/proj\"}\n",
            "not valid json at all\n",
            "{\"type\":\"title\",\"title\":\"Initial Title\"}\n",
            "{\"type\":\"message\",\"timestamp\":\"2026-10-04T19:42:27.500Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello\"},{\"type\":\"image\"},{\"type\":\"text\",\"text\":\"world\"}]}}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\"reply\"}}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"second msg\"}}\n",
            "{\"type\":\"title_change\",\"title\":\"Updated Title\"}\n",
        ),
    )
    .unwrap();

    let s = parse_transcript(&main_path, "abc12345", 2000.0).expect("should parse");
    assert_eq!(s.id, "abc12345");
    assert_eq!(s.project, "/home/u/proj");
    assert_eq!(s.title, "Updated Title");
    assert_eq!(s.first_prompt, "hello world");
    assert_eq!(s.msg_count, 2);
    assert!((s.first_ts - parse_iso_timestamp("2026-10-04T19:42:27.500Z").unwrap()).abs() < 1e-3);
    assert_eq!(s.last_ts, 2000.0);

    let sub_path = tmp.path().join("sub.jsonl");
    fs::write(
        &sub_path,
        "{\"type\":\"session\",\"parentSession\":\"abc12345\",\"cwd\":\"/home/u/proj\"}\n",
    )
    .unwrap();
    assert!(parse_transcript(&sub_path, "sub", 1000.0).is_none());
}

#[test]
fn load_sessions_uses_cache_and_fills_db_titles() {
    let tmp = TempDir::new().unwrap();
    let paths = make_paths(tmp.path());
    let proj_dir = paths.sessions_dir.join("-proj");
    fs::create_dir_all(&proj_dir).unwrap();

    let s1 = proj_dir.join("2026-11-01_11111111.jsonl");
    fs::write(
        &s1,
        "{\"type\":\"session\",\"cwd\":\"/work/alpha\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"fix bug\"}}\n",
    )
    .unwrap();

    let sub = proj_dir.join("2026-11-01_22222222.jsonl");
    fs::write(
        &sub,
        "{\"type\":\"session\",\"parentSession\":\"11111111\",\"cwd\":\"/work/alpha\"}\n",
    )
    .unwrap();

    create_history_db(&paths.history_db);
    let conn = Connection::open(&paths.history_db).unwrap();
    conn.execute(
        "INSERT INTO session_titles (session_id, title) VALUES (?1, ?2)",
        ["11111111", "DB Filled Title"],
    )
    .unwrap();

    let loaded = load_sessions(&paths, |_| {}, None);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id, "11111111");
    assert_eq!(loaded[0].title, "DB Filled Title");

    // Verify cache JSON format compatibility (version 1 + subagent marker without data).
    let cache_text = fs::read_to_string(paths.cache_file.as_ref().unwrap()).unwrap();
    let cache_json: serde_json::Value = serde_json::from_str(&cache_text).unwrap();
    assert_eq!(cache_json["version"], 1);
    let sub_entry = &cache_json["entries"][sub.to_str().unwrap()];
    assert_eq!(sub_entry["subagent"], true);
    assert!(sub_entry.get("data").is_none());

    // Warm reload should return the same session.
    let reloaded = load_sessions(&paths, |_| {}, None);
    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded[0].title, "DB Filled Title");
}

#[test]
fn open_session_files_detects_direct_fd_and_pty_mapping() {
    let tmp = TempDir::new().unwrap();
    let paths = make_paths(tmp.path());
    let proj_dir = paths.sessions_dir.join("-proj");
    fs::create_dir_all(&proj_dir).unwrap();

    let direct_jsonl = proj_dir.join("2026-11-01_direct11.jsonl");
    let pty_jsonl = proj_dir.join("2026-11-01_pty22222.jsonl");
    let worker_jsonl = proj_dir.join("2026-11-01_worker33.jsonl");
    fs::write(&direct_jsonl, "{}\n").unwrap();
    fs::write(&pty_jsonl, "{}\n").unwrap();
    fs::write(&worker_jsonl, "{}\n").unwrap();

    // PID 100: direct fd symlink to jsonl
    let p100_fd = paths.proc_dir.join("100/fd");
    fs::create_dir_all(&p100_fd).unwrap();
    symlink(&direct_jsonl, p100_fd.join("3")).unwrap();

    // PID 200: omp main process on /dev/pts/42
    let p200_fd = paths.proc_dir.join("200/fd");
    fs::create_dir_all(&p200_fd).unwrap();
    fs::write(paths.proc_dir.join("200/comm"), "omp\n").unwrap();
    fs::write(paths.proc_dir.join("200/cmdline"), b"omp\0--resume\0").unwrap();
    symlink("/dev/pts/42", p200_fd.join("0")).unwrap();
    fs::write(
        paths.terminal_sessions_dir.join("pts-42"),
        format!("pid=200\n{}\n", pty_jsonl.display()),
    )
    .unwrap();

    // PID 300: __omp_worker_ on /dev/pts/43 (must NOT be counted via PTY)
    let p300_fd = paths.proc_dir.join("300/fd");
    fs::create_dir_all(&p300_fd).unwrap();
    fs::write(paths.proc_dir.join("300/comm"), "omp\n").unwrap();
    fs::write(paths.proc_dir.join("300/cmdline"), b"omp\0__omp_worker_1\0").unwrap();
    symlink("/dev/pts/43", p300_fd.join("0")).unwrap();
    fs::write(
        paths.terminal_sessions_dir.join("pts-43"),
        format!("pid=300\n{}\n", worker_jsonl.display()),
    )
    .unwrap();

    let live = open_session_files(&paths, None);
    assert!(live.contains(&fs::canonicalize(&direct_jsonl).unwrap()));
    assert!(live.contains(&fs::canonicalize(&pty_jsonl).unwrap()));
    assert!(!live.contains(&fs::canonicalize(&worker_jsonl).unwrap()));
}

#[test]
fn delete_sessions_cleans_files_sibling_dir_and_fts5_dbs_and_skips_live() {
    let tmp = TempDir::new().unwrap();
    let paths = make_paths(tmp.path());
    let proj_dir = paths.sessions_dir.join("-proj");
    fs::create_dir_all(&proj_dir).unwrap();

    let dead_jsonl = proj_dir.join("2026-11-01_dead0001.jsonl");
    let dead_lock = proj_dir.join(".2026-11-01_dead0001.jsonl.lock.os");
    let dead_sibling = proj_dir.join("2026-11-01_dead0001");
    fs::write(&dead_jsonl, "{}\n").unwrap();
    fs::write(&dead_lock, "lock").unwrap();
    fs::create_dir_all(&dead_sibling).unwrap();
    fs::write(dead_sibling.join("artifact.txt"), "hi").unwrap();

    let live_jsonl = proj_dir.join("2026-11-01_live0002.jsonl");
    fs::write(&live_jsonl, "{}\n").unwrap();
    let p100_fd = paths.proc_dir.join("100/fd");
    fs::create_dir_all(&p100_fd).unwrap();
    symlink(&live_jsonl, p100_fd.join("4")).unwrap();

    create_history_db(&paths.history_db);
    {
        let conn = Connection::open(&paths.history_db).unwrap();
        conn.execute(
            "INSERT INTO history (prompt, cwd, session_id) VALUES ('p1', '/a', 'dead0001')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_titles (session_id, title) VALUES ('dead0001', 'T1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_recaps (session_id, cwd, recap) VALUES ('dead0001', '/a', 'R1')",
            [],
        )
        .unwrap();
    }

    create_transcripts_db(&paths.transcripts_db);
    {
        let conn = Connection::open(&paths.transcripts_db).unwrap();
        conn.execute(
            "INSERT INTO files (id, path, session_id, cwd, indexed, messages) VALUES (1, '/p', 'dead0001', '/a', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, file, role, at, text) VALUES (1, 1, 'user', 100, 'hello')",
            [],
        )
        .unwrap();
    }

    let mut s_dead = make_session("dead0001", "/a", "Dead", 1000.0, false);
    s_dead.path = dead_jsonl.to_string_lossy().into_owned();
    let mut s_live = make_session("live0002", "/a", "Live", 1001.0, false);
    s_live.path = live_jsonl.to_string_lossy().into_owned();

    let outcome = delete_sessions(&paths, &[s_dead, s_live]);
    assert_eq!(outcome.deleted.len(), 1);
    assert_eq!(outcome.deleted[0].id, "dead0001");
    assert_eq!(outcome.skipped_live.len(), 1);
    assert_eq!(outcome.skipped_live[0].id, "live0002");
    assert!(outcome.errors.is_empty(), "errors: {:?}", outcome.errors);

    assert!(!dead_jsonl.exists());
    assert!(!dead_lock.exists());
    assert!(!dead_sibling.exists());
    assert!(live_jsonl.exists());

    let hconn = Connection::open(&paths.history_db).unwrap();
    let h_count: i64 = hconn
        .query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))
        .unwrap();
    assert_eq!(h_count, 0);

    let tconn = Connection::open(&paths.transcripts_db).unwrap();
    let m_count: i64 = tconn
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    let f_count: i64 = tconn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(m_count, 0);
    assert_eq!(f_count, 0);
}

#[test]
fn app_filter_sort_selection_resume_and_delete_flow() {
    let tmp = TempDir::new().unwrap();
    let paths = make_paths(tmp.path());
    let mut app = App::new(paths.clone());

    let s1 = make_session("aaaa1111", "/home/u/zeta", "Fix auth", 300.0, false);
    let s2 = make_session("bbbb2222", "/home/u/alpha", "Build TUI", 200.0, true);
    let s3 = make_session("cccc3333", "/home/u/beta", "Auth refactor", 100.0, false);
    app.set_sessions_for_test(vec![s1, s2, s3]);

    // Default sort: Date desc (300, 200, 100)
    assert_eq!(app.filtered[0].id, "aaaa1111");
    assert_eq!(app.sort_mode, SortMode::Date);

    // Cycle sort -> Project (alpha, beta, zeta)
    app.handle_key(key(KeyCode::Char('s')));
    assert_eq!(app.sort_mode, SortMode::Project);
    assert_eq!(app.filtered[0].id, "bbbb2222");
    assert_eq!(app.filtered[1].id, "cccc3333");
    assert_eq!(app.filtered[2].id, "aaaa1111");

    // Cycle sort -> Status (live first)
    app.handle_key(key(KeyCode::Char('s')));
    assert_eq!(app.sort_mode, SortMode::Status);
    assert_eq!(app.filtered[0].id, "bbbb2222");
    assert!(app.filtered[0].live);

    // Filter via `/` for "auth"
    app.handle_key(key(KeyCode::Char('/')));
    assert_eq!(app.focus, Focus::Filter);
    for ch in "auth".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    assert_eq!(app.filtered.len(), 2);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.focus, Focus::Table);

    // Space selects row 0 and advances cursor
    app.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(app.selected.len(), 1);
    assert_eq!(app.table_state.selected(), Some(1));

    // Esc clears filter first, then second Esc clears selection
    app.handle_key(key(KeyCode::Esc));
    assert!(app.filter_query.is_empty());
    assert_eq!(app.filtered.len(), 3);
    assert_eq!(app.selected.len(), 1);
    app.handle_key(key(KeyCode::Esc));
    assert!(app.selected.is_empty());

    // Cannot delete live session (row 0 in Status sort is bbbb2222 which is live)
    app.handle_key(key(KeyCode::Char('d')));
    assert_eq!(app.confirm_delete, ConfirmDelete::None);
    assert_eq!(app.notifications.last().unwrap().severity, Severity::Warning);

    // Move down to non-live session, press d once -> pending confirm; moving cursor resets it
    app.handle_key(key(KeyCode::Down));
    let id = app.current_session().unwrap().id.clone();
    app.handle_key(key(KeyCode::Char('d')));
    assert_eq!(app.confirm_delete, ConfirmDelete::Single(id));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.confirm_delete, ConfirmDelete::None);

    // Restore (`f`) sets --restore command and quits
    app.handle_key(key(KeyCode::Char('f')));
    assert!(app.should_quit);
    let action = app.resume_action.as_ref().unwrap();
    assert_eq!(action.command_args()[0], "omp");
    assert_eq!(action.command_args()[1], "--restore");
}

#[test]
fn ui_renders_narrow_wide_and_loading_states() {
    let tmp = TempDir::new().unwrap();
    let paths = make_paths(tmp.path());
    let mut app = App::new(paths);
    app.set_sessions_for_test(vec![
        make_session("aaaa1111-xxxx", "/home/u/proj", "Hello 🚀 world", 1_760_000_000.0, true),
        make_session("bbbb2222-yyyy", "/home/u/other", "", 1_760_000_100.0, false),
    ]);
    app.selected.insert("aaaa1111-xxxx".to_string());

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        use unicode_width::UnicodeWidthStr;
        let mut out = String::new();
        let mut skip = 0usize;
        for cell in buf.content() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let sym = cell.symbol();
            out.push_str(sym);
            let w = sym.width();
            if w > 1 {
                skip = w - 1;
            }
        }
        out
    }

    // Narrow terminal (no ID column)
    let mut term_narrow = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term_narrow.draw(|f| ui::render(f, &mut app)).unwrap();
    let narrow_text = buffer_text(term_narrow.backend().buffer());
    assert!(narrow_text.contains("OMP Sessions"));
    assert!(narrow_text.contains("Hello 🚀 world"));
    assert!(!narrow_text.contains("aaaa1111"));

    // Wide terminal (ID column visible)
    let mut term_wide = Terminal::new(TestBackend::new(140, 30)).unwrap();
    term_wide.draw(|f| ui::render(f, &mut app)).unwrap();
    let wide_text = buffer_text(term_wide.backend().buffer());
    assert!(wide_text.contains("aaaa1111"));

    // Loading state + filter editing with Ctrl+W
    app.focus = Focus::Filter;
    app.handle_key(key(KeyCode::Char('a')));
    app.handle_key(key(KeyCode::Char(' ')));
    app.handle_key(key(KeyCode::Char('b')));
    app.handle_key(ctrl_key('w'));
    assert_eq!(app.filter_query, "a ");
    app.loading = true;
    term_wide.draw(|f| ui::render(f, &mut app)).unwrap();
}

#[test]
fn helpers_match_python_semantics() {
    assert!(multi_word_match("foo bar", "Bar baz FOO"));
    assert!(!multi_word_match("foo qux", "Bar baz FOO"));
    assert_eq!(shorten_home("/home/alice", Path::new("/home/alice")), "~");
    assert_eq!(
        shorten_home("/home/alice/src/app", Path::new("/home/alice")),
        "~/src/app"
    );
    assert_eq!(
        shorten_home("/etc/hosts", Path::new("/home/alice")),
        "/etc/hosts"
    );
    let _ = PathBuf::from("/tmp");
}
