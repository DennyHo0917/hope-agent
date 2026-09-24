//! Import visible Codex local JSONL messages as read-only Hope conversations.
//! No Codex credentials, instructions, reasoning, or tool payloads are read into
//! the session ledger.

use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::SessionDB;

const MAX_FILES: usize = 5_000;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DEPTH: usize = 8;
// The filesystem snapshot is read before the per-record SQLite transaction.
// Serialize explicit import runs so a slower old scan cannot overwrite a
// newer snapshot committed by another request in this process.
static CODEX_IMPORT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexImportReport {
    pub scanned: usize,
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: usize,
    pub unsupported_content: usize,
}

struct ImportedMessage {
    role: &'static str,
    content: String,
    timestamp: String,
}

struct ImportedRecord {
    source_id: String,
    created_at: String,
    updated_at: String,
    title: String,
    messages: Vec<ImportedMessage>,
    hash: String,
    unsupported_content: usize,
}

fn codex_home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            anyhow::bail!("CODEX_HOME must be an absolute path");
        }
        return Ok(path);
    }
    Ok(dirs::home_dir()
        .context("home directory is unavailable")?
        .join(".codex"))
}

fn collect_jsonl_files(
    dir: &Path,
    files: &mut BinaryHeap<Reverse<(SystemTime, PathBuf)>>,
    depth: usize,
    limit: usize,
    report: &mut CodexImportReport,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    if !fs::symlink_metadata(dir)?.file_type().is_dir() {
        anyhow::bail!("Codex session root must be a directory, not a symlink");
    }
    if depth > MAX_DEPTH {
        report.skipped += 1;
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if depth == 0 => return Err(error.into()),
        Err(_) => {
            report.failed += 1;
            return Ok(());
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                report.failed += 1;
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                report.failed += 1;
                continue;
            }
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_jsonl_files(&entry.path(), files, depth + 1, limit, report)?;
        } else if file_type.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        {
            report.scanned += 1;
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH);
            let candidate = Reverse((modified, entry.path()));
            if files.len() < limit {
                files.push(candidate);
            } else {
                report.skipped += 1;
                if files.peek().is_some_and(|oldest| candidate < *oldest) {
                    files.pop();
                    files.push(candidate);
                }
            }
        }
    }
    Ok(())
}

fn read_stable_jsonl(path: &Path, before: &fs::Metadata) -> Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file()
        || before.len() != opened.len()
        || before.modified()? != opened.modified()?
    {
        anyhow::bail!("Codex session changed during import");
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        anyhow::bail!("Codex session exceeds the import limit");
    }
    let after = fs::symlink_metadata(path)?;
    if !after.file_type().is_file()
        || before.len() != after.len()
        || before.modified()? != after.modified()?
        || bytes.len() as u64 != before.len()
    {
        anyhow::bail!("Codex session changed during import");
    }
    Ok(bytes)
}

fn normalized_timestamp(raw: Option<&str>, fallback: &str) -> String {
    raw.filter(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok())
        .unwrap_or(fallback)
        .to_string()
}

/// Codex may serialize its own startup instructions as `role=user` text.
/// Remove only known leading runtime envelopes, preserving any actual prompt
/// that follows them in the same content block.
fn strip_runtime_separator(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix("\r\n") {
        rest
    } else if let Some(rest) = text.strip_prefix('\n') {
        rest
    } else if text.starts_with(' ') && !text.starts_with("  ") {
        &text[1..]
    } else {
        text
    }
}

fn visible_user_text(mut text: &str) -> Option<&str> {
    loop {
        let candidate = text.trim_start();
        let closing_tag = if candidate.starts_with("<recommended_plugins>") {
            Some("</recommended_plugins>")
        } else if candidate.starts_with("<environment_context>") {
            Some("</environment_context>")
        } else if candidate.starts_with("# AGENTS.md instructions for ") {
            if !candidate.contains("<INSTRUCTIONS>") {
                break;
            }
            Some("</INSTRUCTIONS>")
        } else {
            None
        };
        let Some(closing_tag) = closing_tag else {
            break;
        };
        let Some(end) = candidate.find(closing_tag) else {
            break;
        };
        let end = end + closing_tag.len();
        text = strip_runtime_separator(&candidate[end..]);
    }
    (!text.trim().is_empty()).then_some(text)
}

fn parse_record(bytes: &[u8]) -> Result<ImportedRecord> {
    let text = std::str::from_utf8(bytes).context("JSONL is not UTF-8")?;
    let mut source_id = None;
    let mut seen_metadata = false;
    let mut created_at = None;
    let mut messages = Vec::new();
    let mut unsupported_content = 0;
    let mut last_timestamp = None;
    for line in text.lines() {
        if line.len() > MAX_LINE_BYTES {
            anyhow::bail!("JSONL line exceeds the import limit");
        }
        if line.trim().is_empty() {
            continue;
        }
        let entry: Value = serde_json::from_str(line).context("invalid JSONL line")?;
        let payload = &entry["payload"];
        if entry["type"] == "session_meta" {
            if seen_metadata {
                anyhow::bail!("duplicate session metadata");
            }
            seen_metadata = true;
            let id = payload["id"].as_str().filter(|id| {
                !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
            });
            source_id = id.map(str::to_string);
            created_at = Some(normalized_timestamp(
                entry["timestamp"].as_str(),
                "1970-01-01T00:00:00Z",
            ));
            continue;
        }
        if entry["type"] != "response_item" || payload["type"] != "message" {
            continue;
        }
        let role = match payload["role"].as_str() {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        let Some(content) = payload["content"].as_array() else {
            continue;
        };
        let mut visible = Vec::new();
        for part in content {
            match part["type"].as_str() {
                Some("input_text" | "output_text") => {
                    if let Some(text) = part["text"].as_str() {
                        let text = if role == "user" {
                            visible_user_text(text)
                        } else {
                            (!text.is_empty()).then_some(text)
                        };
                        if let Some(text) = text {
                            visible.push(text);
                        }
                    }
                }
                Some(_) => unsupported_content += 1,
                None => {}
            }
        }
        if visible.is_empty() {
            continue;
        }
        let fallback = last_timestamp
            .as_deref()
            .or(created_at.as_deref())
            .unwrap_or("1970-01-01T00:00:00Z");
        let timestamp = normalized_timestamp(entry["timestamp"].as_str(), fallback);
        last_timestamp = Some(timestamp.clone());
        messages.push(ImportedMessage {
            role,
            content: visible.join("\n\n"),
            timestamp,
        });
    }
    let source_id = source_id.context("missing Codex session ID")?;
    let created_at = created_at.unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
    if messages.is_empty() {
        anyhow::bail!("no visible messages");
    }
    let title = messages
        .iter()
        .find(|message| message.role == "user")
        .map(|message| {
            message
                .content
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("")
        })
        .unwrap_or("Codex conversation")
        .trim()
        .chars()
        .take(80)
        .collect::<String>();
    let updated_at = last_timestamp.unwrap_or_else(|| created_at.clone());
    Ok(ImportedRecord {
        source_id,
        created_at,
        updated_at,
        title: if title.is_empty() {
            "Codex conversation".into()
        } else {
            title
        },
        messages,
        hash: blake3::hash(bytes).to_hex().to_string(),
        unsupported_content,
    })
}

impl SessionDB {
    /// A dedicated ledger controls imported-session write policy. `origin_json`
    /// remains display-only and is never consulted for authorization.
    pub fn is_codex_imported_session(&self, session_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|error| anyhow::anyhow!("Lock error: {error}"))?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_import_sources WHERE session_id=?1)",
            params![session_id],
            |row| row.get::<_, bool>(0),
        )?)
    }

    fn import_codex_record(&self, record: ImportedRecord) -> Result<&'static str> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|error| anyhow::anyhow!("Lock error: {error}"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT session_id, content_hash FROM session_import_sources
             WHERE provider='codex' AND source_id=?1",
                params![record.source_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if existing
            .as_ref()
            .is_some_and(|(_, hash)| hash == &record.hash)
        {
            return Ok("unchanged");
        }
        let now = chrono::Utc::now().to_rfc3339();
        let session_id = existing
            .as_ref()
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let origin = serde_json::to_string(&super::SessionOrigin {
            kind: "codex".into(),
            id: record.source_id.clone(),
            label: "Codex".into(),
        })?;
        if existing.is_some() {
            tx.execute(
                "DELETE FROM messages WHERE session_id=?1",
                params![session_id],
            )?;
            tx.execute(
                "UPDATE sessions SET title=CASE WHEN title_source='manual' THEN title ELSE ?2 END,
                 updated_at=?3, origin_json=?4, context_json=NULL, last_read_message_id=0
                 WHERE id=?1",
                params![session_id, record.title, record.updated_at, origin],
            )?;
        } else {
            tx.execute(
                "INSERT INTO sessions (id, title, title_source, agent_id, created_at,
                 updated_at, origin_json, runtime_defaults_initialized, kind)
                 VALUES (?1, ?2, 'first_message', 'ha-main', ?3, ?4, ?5, 1, 'regular')",
                params![
                    session_id,
                    record.title,
                    record.created_at,
                    record.updated_at,
                    origin
                ],
            )?;
            tx.execute(
                "INSERT INTO session_import_sources
                 (provider, source_id, session_id, content_hash, imported_at)
                 VALUES ('codex', ?1, ?2, ?3, ?4)",
                params![record.source_id, session_id, record.hash, now],
            )?;
        }
        let mut last_id = 0;
        for message in record.messages {
            tx.execute(
                "INSERT INTO messages (session_id, role, content, timestamp, source)
                 VALUES (?1, ?2, ?3, ?4, 'codex_import')",
                params![session_id, message.role, message.content, message.timestamp],
            )?;
            last_id = tx.last_insert_rowid();
        }
        tx.execute(
            "UPDATE sessions SET last_read_message_id=?2 WHERE id=?1",
            params![session_id, last_id],
        )?;
        if existing.is_some() {
            tx.execute(
                "UPDATE session_import_sources SET content_hash=?2, imported_at=?3
                 WHERE provider='codex' AND source_id=?1",
                params![record.source_id, record.hash, now],
            )?;
        }
        tx.commit()?;
        Ok(if existing.is_some() {
            "updated"
        } else {
            "created"
        })
    }

    pub fn import_local_codex_sessions(&self) -> Result<CodexImportReport> {
        let _import_guard = CODEX_IMPORT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = codex_home()?;
        let mut files = BinaryHeap::new();
        let mut report = CodexImportReport::default();
        collect_jsonl_files(
            &root.join("sessions"),
            &mut files,
            0,
            MAX_FILES,
            &mut report,
        )?;
        collect_jsonl_files(
            &root.join("archived_sessions"),
            &mut files,
            0,
            MAX_FILES,
            &mut report,
        )?;
        // A thread can appear in both active and archived roots. Prefer the
        // newest snapshot before the per-run source-id deduplication.
        let mut files: Vec<_> = files
            .into_vec()
            .into_iter()
            .map(|Reverse(item)| item)
            .collect();
        files.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        let mut seen_ids = HashSet::new();
        let mut read_bytes = 0u64;
        for (_, path) in files {
            let outcome = (|| -> Result<Option<ImportedRecord>> {
                let before = fs::symlink_metadata(&path)?;
                if !before.file_type().is_file()
                    || before.len() > MAX_FILE_BYTES
                    || read_bytes.saturating_add(before.len()) > MAX_TOTAL_BYTES
                {
                    return Ok(None);
                }
                read_bytes += before.len();
                let bytes = read_stable_jsonl(&path, &before)?;
                parse_record(&bytes).map(Some)
            })();
            match outcome {
                Ok(Some(record)) if seen_ids.insert(record.source_id.clone()) => {
                    report.unsupported_content += record.unsupported_content;
                    match self.import_codex_record(record) {
                        Ok("created") => report.created += 1,
                        Ok("updated") => report.updated += 1,
                        Ok(_) => report.unchanged += 1,
                        Err(_) => report.failed += 1,
                    }
                }
                Ok(Some(_)) | Ok(None) => report.skipped += 1,
                Err(_) => report.failed += 1,
            }
        }
        crate::app_info!(
            "session",
            "codex_import",
            "Codex import completed: scanned={} created={} updated={} unchanged={} skipped={} failed={} unsupported_content={}",
            report.scanned,
            report.created,
            report.updated,
            report.unchanged,
            report.skipped,
            report.failed,
            report.unsupported_content
        );
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::SessionDB;
    use super::{collect_jsonl_files, parse_record, visible_user_text, CodexImportReport};
    use crate::session::NewMessage;
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn imports_only_visible_user_and_assistant_text() {
        let jsonl = concat!(
            "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"payload\":{\"id\":\"source-1\",\"base_instructions\":\"secret\"}}\n",
            "{\"type\":\"response_item\",\"timestamp\":\"2026-01-01T00:00:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"developer\",\"content\":[{\"type\":\"input_text\",\"text\":\"hidden\"}]}}\n",
            "{\"type\":\"response_item\",\"timestamp\":\"2026-01-01T00:00:02Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"你好\"},{\"type\":\"input_image\",\"image_url\":\"secret\"}]}}\n",
            "{\"type\":\"response_item\",\"timestamp\":\"2026-01-01T00:00:03Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"回答\"}]}}\n"
        );
        let record = parse_record(jsonl.as_bytes()).unwrap();
        assert_eq!(record.source_id, "source-1");
        assert_eq!(record.title, "你好");
        assert_eq!(record.messages.len(), 2);
        assert_eq!(record.messages[0].content, "你好");
        assert_eq!(record.messages[1].content, "回答");
        assert_eq!(record.unsupported_content, 1);
    }

    #[test]
    fn strips_generated_user_context_without_losing_the_prompt() {
        let lines = [
            serde_json::json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":"source-2"}}),
            serde_json::json!({"type":"response_item","timestamp":"2026-01-01T00:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>hidden</environment_context>"}]}}),
            serde_json::json!({"type":"response_item","timestamp":"2026-01-01T00:00:02Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<recommended_plugins>hidden</recommended_plugins>\n# AGENTS.md instructions for /workspace\n<INSTRUCTIONS>hidden</INSTRUCTIONS>\n<environment_context>hidden</environment_context>\nActual request"}]}}),
            serde_json::json!({"type":"response_item","timestamp":"2026-01-01T00:00:03Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Answer"}]}}),
        ];
        let jsonl = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let record = parse_record(jsonl.as_bytes()).unwrap();
        assert_eq!(record.title, "Actual request");
        assert_eq!(record.messages.len(), 2);
        assert_eq!(record.messages[0].content, "Actual request");
        assert_eq!(record.messages[1].content, "Answer");
    }

    #[test]
    fn incomplete_envelope_like_user_text_is_preserved() {
        for text in [
            "<environment_context>my literal prompt",
            "<recommended_plugins>my literal prompt",
            "# AGENTS.md instructions for my own document",
            "# AGENTS.md instructions for my own document\n<INSTRUCTIONS>unfinished",
            "<environment_context>complete</environment_context>\n<recommended_plugins>unfinished",
        ] {
            let expected = if text.starts_with("<environment_context>complete") {
                "<recommended_plugins>unfinished"
            } else {
                text
            };
            assert_eq!(visible_user_text(text), Some(expected));
        }
    }

    #[test]
    fn ordinary_user_prompt_preserves_leading_whitespace() {
        let prompt = "\n    code block\n    second line";
        assert_eq!(visible_user_text(prompt), Some(prompt));
        assert_eq!(
            visible_user_text("    <environment_context>unfinished"),
            Some("    <environment_context>unfinished")
        );
        let lines = [
            serde_json::json!({"type":"session_meta","payload":{"id":"whitespace-source"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":prompt}]}}),
        ];
        let jsonl = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let record = parse_record(jsonl.as_bytes()).unwrap();
        assert_eq!(record.messages[0].content, prompt);
    }

    #[test]
    fn prompt_after_runtime_envelopes_preserves_indentation() {
        let prompt = "    code block\n    second line";
        for text in [
            format!("<environment_context>hidden</environment_context>\n{prompt}"),
            format!("<environment_context>hidden</environment_context>\r\n{prompt}"),
            format!("<recommended_plugins>hidden</recommended_plugins>\n<environment_context>hidden</environment_context>\n{prompt}"),
        ] {
            assert_eq!(visible_user_text(&text), Some(prompt));
        }
        assert_eq!(
            visible_user_text("<environment_context>hidden</environment_context>\n\n    code"),
            Some("\n    code")
        );
    }

    #[test]
    fn invalid_timestamps_use_valid_metadata_or_prior_message() {
        let lines = [
            serde_json::json!({"type":"session_meta","timestamp":"invalid","payload":{"id":"invalid-time-source"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Question"}]}}),
            serde_json::json!({"type":"response_item","timestamp":"also-invalid","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Answer"}]}}),
        ];
        let jsonl = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let record = parse_record(jsonl.as_bytes()).unwrap();
        assert_eq!(record.created_at, "1970-01-01T00:00:00Z");
        assert_eq!(record.updated_at, "1970-01-01T00:00:00Z");
        assert!(record
            .messages
            .iter()
            .all(|message| message.timestamp == "1970-01-01T00:00:00Z"));

        let lines = [
            serde_json::json!({"type":"session_meta","timestamp":"2025-01-01T00:00:00Z","payload":{"id":"missing-last-time"}}),
            serde_json::json!({"type":"response_item","timestamp":"2026-01-01T00:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Question"}]}}),
            serde_json::json!({"type":"response_item","timestamp":"invalid","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Answer"}]}}),
        ];
        let jsonl = lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let record = parse_record(jsonl.as_bytes()).unwrap();
        assert_eq!(record.messages[1].timestamp, "2026-01-01T00:00:00Z");
        assert_eq!(record.updated_at, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn duplicate_metadata_is_rejected_even_when_first_id_is_invalid() {
        for first_id in [serde_json::Value::Null, serde_json::json!("")] {
            let lines = [
                serde_json::json!({"type":"session_meta","payload":{"id":first_id}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"unrelated"}]}}),
                serde_json::json!({"type":"session_meta","payload":{"id":"valid-id"}}),
            ];
            let jsonl = lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(parse_record(jsonl.as_bytes()).is_err());
        }
    }

    #[test]
    fn file_limit_skips_excess_without_discarding_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        for (name, seconds) in [("a.jsonl", 1), ("b.jsonl", 3), ("c.jsonl", 2)] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"{}").unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
                .unwrap();
        }
        let mut files = BinaryHeap::<Reverse<_>>::new();
        let mut report = CodexImportReport::default();
        collect_jsonl_files(dir.path(), &mut files, 0, 2, &mut report).unwrap();
        assert_eq!(report.scanned, 3);
        assert_eq!(report.skipped, 1);
        assert_eq!(files.len(), 2);
        let selected: Vec<_> = files
            .into_iter()
            .map(|Reverse((_, path))| path.file_name().unwrap().to_owned())
            .collect();
        assert!(!selected.contains(&"a.jsonl".into()));
    }

    #[test]
    fn reimport_keeps_session_id_and_blocks_live_messages() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDB::open_ephemeral_for_test(&dir.path().join("sessions.db")).unwrap();
        db.with_conn_for_test(|conn| {
            conn.execute_batch(
                "CREATE TABLE channel_conversations (
                    session_id TEXT PRIMARY KEY,
                    channel_id TEXT,
                    account_id TEXT,
                    chat_id TEXT,
                    chat_type TEXT,
                    sender_name TEXT
                );",
            )?;
            Ok(())
        })
        .unwrap();
        let first = concat!(
            "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"payload\":{\"id\":\"source-1\"}}\n",
            "{\"type\":\"response_item\",\"timestamp\":\"2026-01-01T00:00:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Question\"}]}}\n"
        );
        assert_eq!(
            db.import_codex_record(parse_record(first.as_bytes()).unwrap())
                .unwrap(),
            "created"
        );
        let session_id: String = db
            .with_conn_for_test(|conn| {
                Ok(
                    conn.query_row("SELECT session_id FROM session_import_sources", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .unwrap();
        assert!(db.is_codex_imported_session(&session_id).unwrap());
        assert!(db
            .list_sessions(None)
            .unwrap()
            .iter()
            .any(|s| s.id == session_id));
        assert!(db
            .list_sessions_for_model(None, false, 20)
            .unwrap()
            .iter()
            .all(|s| s.id != session_id));
        assert!(db
            .search_messages("Question", None, None, None, 10)
            .unwrap()
            .iter()
            .any(|hit| hit.session_id == session_id));
        assert!(db
            .search_message_content_for_model("Question", None, None, None, 10)
            .unwrap()
            .is_empty());
        assert!(db.fork_session(&session_id, None).is_err());
        assert!(db.create_side_chat(&session_id).is_err());
        assert!(db
            .append_message(&session_id, &NewMessage::user("new turn"))
            .is_err());
        assert_eq!(
            db.import_codex_record(parse_record(first.as_bytes()).unwrap())
                .unwrap(),
            "unchanged"
        );

        let changed = format!("{first}{}", "{\"type\":\"response_item\",\"timestamp\":\"2026-01-01T00:00:02Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Answer\"}]}}\n");
        assert_eq!(
            db.import_codex_record(parse_record(changed.as_bytes()).unwrap())
                .unwrap(),
            "updated"
        );
        let (imported_id, message_count): (String, i64) = db
            .with_conn_for_test(|conn| {
                let imported_id: String =
                    conn.query_row("SELECT session_id FROM session_import_sources", [], |row| {
                        row.get(0)
                    })?;
                let message_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM messages WHERE session_id=?1",
                    rusqlite::params![imported_id],
                    |row| row.get(0),
                )?;
                Ok((imported_id, message_count))
            })
            .unwrap();
        assert_eq!(imported_id, session_id);
        assert_eq!(message_count, 2);
    }
}
