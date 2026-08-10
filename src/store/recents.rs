//! Recents: the list of session configurations the user has created
//! in the past, so the new-session modal can offer quick re-creation
//! without the user having to re-enter every field.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::error::Result;
use crate::events::{
    ClaudeOptions, ClaudeSessionMode, CodexOptions, KimiOptions, OpencodeOptions, QwenOptions,
    SessionSpec, SpecOptions,
};

use super::{map_sql_err, Store};

/// One row out of the `recents` table, ready to display in the modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recent {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub agent: String,
    pub args: String,
    pub claude: ClaudeOptions,
    pub codex: CodexOptions,
    pub kimi: KimiOptions,
    pub opencode: OpencodeOptions,
    pub qwen: QwenOptions,
    pub last_used_at: i64,
    pub use_count: i64,
}

impl Recent {
    /// Convert back into a `SessionSpec` suitable for pre-filling the
    /// new-session modal. `name` is copied verbatim; the user can
    /// edit it (and the collision resolver will rename on submit if
    /// the live tmux server already has a session with that name).
    pub fn to_spec(&self) -> SessionSpec {
        SessionSpec {
            name: self.name.clone(),
            path: self.path.clone(),
            agent: self.agent.clone(),
            args: self.args.clone(),
            options: SpecOptions {
                claude: self.claude.clone(),
                codex: self.codex.clone(),
                kimi: self.kimi.clone(),
                opencode: self.opencode.clone(),
                qwen: self.qwen.clone(),
            },
            container_id: None,
            resume: false,
            // Recents never re-create a worktree — recreating from a
            // recent opens the (already existing) path directly.
            worktree: None,
        }
    }
}

impl Store {
    /// Insert a recent, or if (name, path, agent) already exists,
    /// bump `last_used_at` + `use_count` and refresh the other fields
    /// (so option toggles from the latest create are preserved).
    pub fn upsert_recent(&self, spec: &SessionSpec) -> Result<()> {
        let now = now_millis();
        let session_mode = claude_mode_to_str(spec.options.claude.session_mode);
        let skip_perms = spec.options.claude.skip_permissions as i64;
        let codex_mode = claude_mode_to_str(spec.options.codex.session_mode);
        let yolo = spec.options.codex.yolo as i64;
        let kimi_mode = claude_mode_to_str(spec.options.kimi.session_mode);
        let kimi_yolo = spec.options.kimi.yolo as i64;
        let opencode_mode = claude_mode_to_str(spec.options.opencode.session_mode);
        let opencode_auto = spec.options.opencode.auto as i64;
        let qwen_mode = claude_mode_to_str(spec.options.qwen.session_mode);
        let qwen_yolo = spec.options.qwen.yolo as i64;

        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            r#"
            INSERT INTO recents (
                name, path, agent, args,
                claude_session_mode, claude_skip_permissions, codex_yolo,
                kimi_session_mode, kimi_yolo,
                codex_session_mode,
                opencode_session_mode, opencode_auto,
                qwen_session_mode, qwen_yolo,
                last_used_at, use_count
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, 1)
            ON CONFLICT(name, path, agent) DO UPDATE SET
                args                    = excluded.args,
                claude_session_mode     = excluded.claude_session_mode,
                claude_skip_permissions = excluded.claude_skip_permissions,
                codex_yolo              = excluded.codex_yolo,
                kimi_session_mode       = excluded.kimi_session_mode,
                kimi_yolo               = excluded.kimi_yolo,
                codex_session_mode      = excluded.codex_session_mode,
                opencode_session_mode   = excluded.opencode_session_mode,
                opencode_auto           = excluded.opencode_auto,
                qwen_session_mode       = excluded.qwen_session_mode,
                qwen_yolo               = excluded.qwen_yolo,
                last_used_at            = excluded.last_used_at,
                use_count               = use_count + 1
            "#,
            params![
                spec.name,
                spec.path,
                spec.agent,
                spec.args,
                session_mode,
                skip_perms,
                yolo,
                kimi_mode,
                kimi_yolo,
                codex_mode,
                opencode_mode,
                opencode_auto,
                qwen_mode,
                qwen_yolo,
                now,
            ],
        )
        .map_err(map_sql_err)?;
        Ok(())
    }

    /// Return up to `limit` recents, most-recently-used first.
    pub fn list_recents(&self, limit: usize) -> Result<Vec<Recent>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn
            .prepare(
                r#"
                SELECT
                    id, name, path, agent, args,
                    claude_session_mode, claude_skip_permissions, codex_yolo,
                    kimi_session_mode, kimi_yolo,
                    codex_session_mode,
                    opencode_session_mode, opencode_auto,
                    qwen_session_mode, qwen_yolo,
                    last_used_at, use_count
                FROM recents
                ORDER BY last_used_at DESC
                LIMIT ?1
                "#,
            )
            .map_err(map_sql_err)?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let session_mode_str: String = row.get(5)?;
                let claude_skip: i64 = row.get(6)?;
                let codex_yolo: i64 = row.get(7)?;
                let kimi_mode_str: String = row.get(8)?;
                let kimi_yolo: i64 = row.get(9)?;
                let codex_mode_str: String = row.get(10)?;
                let opencode_mode_str: String = row.get(11)?;
                let opencode_auto: i64 = row.get(12)?;
                let qwen_mode_str: String = row.get(13)?;
                let qwen_yolo: i64 = row.get(14)?;
                Ok(Recent {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    agent: row.get(3)?,
                    args: row.get(4)?,
                    claude: ClaudeOptions {
                        session_mode: claude_mode_from_str(&session_mode_str),
                        skip_permissions: claude_skip != 0,
                    },
                    codex: CodexOptions {
                        session_mode: claude_mode_from_str(&codex_mode_str),
                        yolo: codex_yolo != 0,
                    },
                    kimi: KimiOptions {
                        session_mode: claude_mode_from_str(&kimi_mode_str),
                        yolo: kimi_yolo != 0,
                    },
                    opencode: OpencodeOptions {
                        session_mode: claude_mode_from_str(&opencode_mode_str),
                        auto: opencode_auto != 0,
                    },
                    qwen: QwenOptions {
                        session_mode: claude_mode_from_str(&qwen_mode_str),
                        yolo: qwen_yolo != 0,
                    },
                    last_used_at: row.get(15)?,
                    use_count: row.get(16)?,
                })
            })
            .map_err(map_sql_err)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(map_sql_err)?);
        }
        Ok(out)
    }

    /// Delete a recent row by its primary key. Used by the `d` key
    /// in the RecentsModal to remove stale entries.
    pub fn delete_recent(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute("DELETE FROM recents WHERE id = ?1", params![id])
            .map_err(map_sql_err)?;
        Ok(())
    }

    /// Return `DISTINCT path`s sorted by most-recently-used, up to
    /// `limit`. Used by path tab-completion in the modal (Phase 3c
    /// polish).
    #[allow(dead_code)]
    pub fn list_recent_paths(&self, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn
            .prepare(
                r#"
                SELECT path, MAX(last_used_at) AS mru
                FROM recents
                GROUP BY path
                ORDER BY mru DESC
                LIMIT ?1
                "#,
            )
            .map_err(map_sql_err)?;
        let rows = stmt
            .query_map(params![limit as i64], |row| row.get::<_, String>(0))
            .map_err(map_sql_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(map_sql_err)?);
        }
        Ok(out)
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn claude_mode_to_str(mode: ClaudeSessionMode) -> &'static str {
    match mode {
        ClaudeSessionMode::New => "New",
        ClaudeSessionMode::Continue => "Continue",
        ClaudeSessionMode::Resume => "Resume",
    }
}

fn claude_mode_from_str(s: &str) -> ClaudeSessionMode {
    match s {
        "Continue" => ClaudeSessionMode::Continue,
        "Resume" => ClaudeSessionMode::Resume,
        _ => ClaudeSessionMode::New,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, path: &str, agent: &str) -> SessionSpec {
        SessionSpec {
            name: name.to_string(),
            path: path.to_string(),
            agent: agent.to_string(),
            args: String::new(),
            options: SpecOptions::default(),
            container_id: None,
            resume: false,
            worktree: None,
        }
    }

    #[test]
    fn upsert_then_list_returns_the_row() {
        let s = Store::in_memory().unwrap();
        s.upsert_recent(&spec("work", "/tmp", "claude")).unwrap();
        let got = s.list_recents(10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "work");
        assert_eq!(got[0].path, "/tmp");
        assert_eq!(got[0].agent, "claude");
        assert_eq!(got[0].use_count, 1);
    }

    #[test]
    fn upsert_same_key_bumps_use_count() {
        let s = Store::in_memory().unwrap();
        s.upsert_recent(&spec("work", "/tmp", "claude")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.upsert_recent(&spec("work", "/tmp", "claude")).unwrap();
        let got = s.list_recents(10).unwrap();
        assert_eq!(got.len(), 1, "same key should not insert a second row");
        assert_eq!(got[0].use_count, 2);
    }

    #[test]
    fn different_names_are_separate_recents() {
        let s = Store::in_memory().unwrap();
        s.upsert_recent(&spec("work", "/tmp", "claude")).unwrap();
        s.upsert_recent(&spec("play", "/tmp", "claude")).unwrap();
        let got = s.list_recents(10).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn list_respects_last_used_ordering() {
        let s = Store::in_memory().unwrap();
        s.upsert_recent(&spec("older", "/tmp", "claude")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.upsert_recent(&spec("newer", "/tmp", "claude")).unwrap();
        let got = s.list_recents(10).unwrap();
        assert_eq!(got[0].name, "newer");
        assert_eq!(got[1].name, "older");
    }

    #[test]
    fn list_respects_limit() {
        let s = Store::in_memory().unwrap();
        for i in 0..5 {
            s.upsert_recent(&spec(&format!("n{}", i), "/tmp", "claude"))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let got = s.list_recents(3).unwrap();
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn recent_roundtrip_claude_options() {
        let s = Store::in_memory().unwrap();
        let mut sp = spec("api", "/srv", "claude");
        sp.options.claude.skip_permissions = true;
        sp.options.claude.session_mode = ClaudeSessionMode::Resume;
        s.upsert_recent(&sp).unwrap();
        let got = &s.list_recents(1).unwrap()[0];
        assert!(got.claude.skip_permissions);
        assert_eq!(got.claude.session_mode, ClaudeSessionMode::Resume);
    }

    #[test]
    fn recent_roundtrip_codex_options() {
        let s = Store::in_memory().unwrap();
        let mut sp = spec("ops", "/srv", "codex");
        sp.options.codex.yolo = true;
        sp.options.codex.session_mode = ClaudeSessionMode::Resume;
        s.upsert_recent(&sp).unwrap();
        let got = &s.list_recents(1).unwrap()[0];
        assert!(got.codex.yolo);
        assert_eq!(got.codex.session_mode, ClaudeSessionMode::Resume);
    }

    #[test]
    fn recent_roundtrip_kimi_options() {
        let s = Store::in_memory().unwrap();
        let mut sp = spec("moon", "/srv", "kimi");
        sp.options.kimi.yolo = true;
        sp.options.kimi.session_mode = ClaudeSessionMode::Continue;
        s.upsert_recent(&sp).unwrap();
        let got = &s.list_recents(1).unwrap()[0];
        assert!(got.kimi.yolo);
        assert_eq!(got.kimi.session_mode, ClaudeSessionMode::Continue);
    }

    #[test]
    fn recent_roundtrip_opencode_options() {
        let s = Store::in_memory().unwrap();
        let mut sp = spec("oc", "/srv", "opencode");
        sp.options.opencode.auto = true;
        sp.options.opencode.session_mode = ClaudeSessionMode::Continue;
        s.upsert_recent(&sp).unwrap();
        let got = &s.list_recents(1).unwrap()[0];
        assert!(got.opencode.auto);
        assert_eq!(got.opencode.session_mode, ClaudeSessionMode::Continue);
    }

    #[test]
    fn recent_roundtrip_qwen_options() {
        let s = Store::in_memory().unwrap();
        let mut sp = spec("qc", "/srv", "qwen");
        sp.options.qwen.yolo = true;
        sp.options.qwen.session_mode = ClaudeSessionMode::Resume;
        s.upsert_recent(&sp).unwrap();
        let got = &s.list_recents(1).unwrap()[0];
        assert!(got.qwen.yolo);
        assert_eq!(got.qwen.session_mode, ClaudeSessionMode::Resume);
    }

    #[test]
    fn delete_recent_removes_row() {
        let s = Store::in_memory().unwrap();
        s.upsert_recent(&spec("keep", "/tmp", "claude")).unwrap();
        s.upsert_recent(&spec("drop", "/tmp", "claude")).unwrap();
        let before = s.list_recents(10).unwrap();
        assert_eq!(before.len(), 2);
        let drop_id = before.iter().find(|r| r.name == "drop").unwrap().id;
        s.delete_recent(drop_id).unwrap();
        let after = s.list_recents(10).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].name, "keep");
    }

    #[test]
    fn delete_nonexistent_recent_is_noop() {
        let s = Store::in_memory().unwrap();
        s.upsert_recent(&spec("keep", "/tmp", "claude")).unwrap();
        // Delete an id that doesn't exist — should silently succeed.
        s.delete_recent(99999).unwrap();
        let after = s.list_recents(10).unwrap();
        assert_eq!(after.len(), 1);
    }

    #[test]
    fn list_recent_paths_deduplicates() {
        let s = Store::in_memory().unwrap();
        s.upsert_recent(&spec("a", "/x", "claude")).unwrap();
        s.upsert_recent(&spec("b", "/x", "claude")).unwrap();
        s.upsert_recent(&spec("c", "/y", "terminal")).unwrap();
        let paths = s.list_recent_paths(10).unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"/x".to_string()));
        assert!(paths.contains(&"/y".to_string()));
    }

    #[test]
    fn to_spec_carries_all_fields() {
        let r = Recent {
            id: 1,
            name: "x".into(),
            path: "/a".into(),
            agent: "claude".into(),
            args: "--foo".into(),
            claude: ClaudeOptions {
                session_mode: ClaudeSessionMode::Continue,
                skip_permissions: true,
            },
            codex: CodexOptions::default(),
            kimi: KimiOptions::default(),
            opencode: OpencodeOptions::default(),
            qwen: QwenOptions::default(),
            last_used_at: 123,
            use_count: 7,
        };
        let s = r.to_spec();
        assert_eq!(s.name, "x");
        assert_eq!(s.args, "--foo");
        assert!(s.options.claude.skip_permissions);
    }
}
