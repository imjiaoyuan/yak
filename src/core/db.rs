//! logs.db: the full final schema of the reference implementation
//! (migrations m001–m027 applied), the legacy read helpers, ULID ids and
//! the two timestamp formats the store relies on.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

use crate::core::config;

pub struct Db {
    conn: Connection,
}

/// Migration names exactly as the reference records them, so the
/// reference tool opening our database sees every migration as applied and
/// skips replaying them.
const APPLIED_MIGRATIONS: &[&str] = &[
    "m001_initial",
    "m002_id_primary_key",
    "m003_chat_id_foreign_key",
    "m004_column_order",
    "m004_drop_provider",
    "m005_debug",
    "m006_new_logs_table",
    "m007_finish_logs_table",
    "m008_reply_to_id_foreign_key",
    "m008_fix_column_order_in_logs",
    "m009_delete_logs_table_if_empty",
    "m010_create_new_log_tables",
    "m011_fts_for_responses",
    "m012_attachments_tables",
    "m013_usage",
    "m014_schemas",
    "m015_fragments_tables",
    "m016_fragments_table_pks",
    "m017_tools_tables",
    "m017_tools_plugin",
    "m018_tool_instances",
    "m019_resolved_model",
    "m020_tool_results_attachments",
    "m021_tool_results_exception",
    "m022_response_reasoning",
    "m023_message_store",
    "m024_tool_instance_references",
    "m025_turn_tools_instance_backfill",
    "m026_message_tree_view",
    "m027_turns_response_json",
];

/// Turns store `response.datetime_utc()` — datetime.isoformat():
/// "2026-08-16T08:41:02.123456+00:00", microseconds omitted when zero.
pub fn now_turn_datetime() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    format!(
        "{}+00:00",
        format_utc(d.as_secs() as i64, d.subsec_micros(), 'T')
    )
}

/// Threads store `str(datetime.now(timezone.utc))` — space separator.
pub fn now_thread_datetime() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    format!(
        "{}+00:00",
        format_utc(d.as_secs() as i64, d.subsec_micros(), ' ')
    )
}

/// Compact timestamp for the session list: HH:MM when it is today, MM-DD
/// otherwise (both inputs are ISO-ish strings, so dates compare lexically).
pub fn short_time(now: &str, then: &str) -> String {
    let today = now.get(..10).unwrap_or("");
    if then.get(..10) == Some(today) {
        then.get(11..16).unwrap_or("").to_string()
    } else {
        then.get(5..10).unwrap_or("").replace('-', "/")
    }
}

fn format_utc(secs: i64, micros: u32, sep: char) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil-from-days algorithm (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    if micros == 0 {
        format!("{y:04}-{mo:02}-{d:02}{sep}{h:02}:{m:02}:{s:02}")
    } else {
        format!("{y:04}-{mo:02}-{d:02}{sep}{h:02}:{m:02}:{s:02}.{micros:06}")
    }
}

// ULID

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

static LAST_ULID: std::sync::Mutex<Option<(u64, u128)>> = std::sync::Mutex::new(None);

pub fn ulid() -> String {
    let now_ms = now_ms();
    let mut last = LAST_ULID.lock().unwrap();
    let mut rand_bits = {
        let mut buf = [0u8; 10];
        getrandom_fill(&mut buf);
        let mut v: u128 = 0;
        for b in buf {
            v = (v << 8) | b as u128;
        }
        v
    };
    // keep ids strictly monotonic within a process: same ms must beat the last value
    if let Some((t, prev)) = *last
        && now_ms == t
        && rand_bits <= prev
    {
        rand_bits = prev.wrapping_add(1) & ((1u128 << 80) - 1);
    }
    *last = Some((now_ms, rand_bits));
    encode_ulid(now_ms, rand_bits)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn encode_ulid(ts: u64, rand: u128) -> String {
    let mut chars = [0u8; 26];
    let mut t = ts as u128;
    for slot in chars.iter_mut().take(10).rev() {
        *slot = CROCKFORD[(t & 31) as usize];
        t >>= 5;
    }
    let mut r = rand;
    for slot in chars.iter_mut().skip(10).rev() {
        *slot = CROCKFORD[(r & 31) as usize];
        r >>= 5;
    }
    String::from_utf8(chars.to_vec()).unwrap()
}

fn getrandom_fill(buf: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom")
            && f.read_exact(buf).is_ok()
        {
            return;
        }
    }
    // non-unix (and a failed /dev/urandom): RandomState is OS-seeded per
    // process and differs per call — no crate needed for ten random bytes
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut filled = 0usize;
    while filled < buf.len() {
        let v = RandomState::new().build_hasher().finish().to_le_bytes();
        let take = (buf.len() - filled).min(v.len());
        buf[filled..filled + take].copy_from_slice(&v[..take]);
        filled += take;
    }
}

// open + schema

impl Db {
    pub fn open() -> rusqlite::Result<Db> {
        let path = config::logs_db_path();
        config::ensure_dir_exists(&path);
        let conn = Connection::open(&path)?;
        Db::finish_open(conn)
    }

    pub fn open_path(path: &std::path::Path) -> rusqlite::Result<Db> {
        config::ensure_dir_exists(path);
        let conn = Connection::open(path)?;
        Db::finish_open(conn)
    }

    #[cfg(test)]
    pub fn open_memory() -> rusqlite::Result<Db> {
        let conn = Connection::open_in_memory()?;
        Db::finish_open(conn)
    }

    fn finish_open(conn: Connection) -> rusqlite::Result<Db> {
        // the original uses SQLite defaults (rollback journal) — no WAL here
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        // schema + FTS rebuild run once per database (user_version gate):
        // rebuilding the external-content FTS indexes rewrites the whole
        // index, far too costly to repeat on every open
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            self.conn.execute_batch(SCHEMA_SQL)?;
            self.conn.execute_batch(MESSAGE_TREE_SQL)?;
            self.conn.execute(
                "INSERT INTO responses_fts(responses_fts) VALUES('rebuild')",
                [],
            )?;
            self.conn.execute(
                "INSERT INTO turn_search_fts(turn_search_fts) VALUES('rebuild')",
                [],
            )?;
            self.conn.pragma_update(None, "user_version", 1)?;
            // prewritten migration rows, once per database (not per open)
            for name in APPLIED_MIGRATIONS {
                self.conn.execute(
                    "INSERT OR IGNORE INTO _llm_migrations (name, applied_at) VALUES (?1, ?2)",
                    params![name, now_thread_datetime()],
                )?;
            }
        }
        Ok(())
    }

    /// Store-layer access. Command modules should go through `logstore`
    /// functions instead of writing SQL.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Online backup (`llm logs backup PATH`).
    pub fn backup_to(&self, path: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("VACUUM INTO ?1", params![path])
            .map(|_| ())
    }
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS _llm_migrations (
  name TEXT PRIMARY KEY,
  applied_at TEXT
);

CREATE TABLE IF NOT EXISTS conversations (
  id TEXT PRIMARY KEY,
  name TEXT,
  model TEXT
);

CREATE TABLE IF NOT EXISTS schemas (
  id TEXT PRIMARY KEY,
  content TEXT
);

CREATE TABLE IF NOT EXISTS responses (
  id TEXT PRIMARY KEY,
  model TEXT,
  prompt TEXT,
  system TEXT,
  prompt_json TEXT,
  options_json TEXT,
  response TEXT,
  response_json TEXT,
  conversation_id TEXT REFERENCES conversations(id),
  duration_ms INTEGER,
  datetime_utc TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  token_details TEXT,
  schema_id TEXT REFERENCES schemas(id),
  resolved_model TEXT,
  reasoning TEXT
);

CREATE TABLE IF NOT EXISTS attachments (
  id TEXT PRIMARY KEY,
  type TEXT,
  path TEXT,
  url TEXT,
  content BLOB
);

CREATE TABLE IF NOT EXISTS prompt_attachments (
  response_id TEXT REFERENCES responses(id),
  attachment_id TEXT REFERENCES attachments(id),
  "order" INTEGER,
  PRIMARY KEY (response_id, attachment_id)
);

CREATE TABLE IF NOT EXISTS usage (
  responses_id TEXT PRIMARY KEY REFERENCES responses(id),
  input_tokens INTEGER,
  output_tokens INTEGER,
  token_details TEXT
);

CREATE TABLE IF NOT EXISTS fragments (
  id INTEGER PRIMARY KEY,
  hash TEXT,
  content TEXT,
  datetime_utc TEXT,
  source TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS fragments_hash ON fragments(hash);

CREATE TABLE IF NOT EXISTS fragment_aliases (
  alias TEXT PRIMARY KEY,
  fragment_id INTEGER REFERENCES fragments(id)
);

CREATE TABLE IF NOT EXISTS prompt_fragments (
  response_id TEXT REFERENCES responses(id),
  fragment_id INTEGER REFERENCES fragments(id),
  "order" INTEGER,
  PRIMARY KEY (response_id, fragment_id, "order")
);

CREATE TABLE IF NOT EXISTS system_fragments (
  response_id TEXT REFERENCES responses(id),
  fragment_id INTEGER REFERENCES fragments(id),
  "order" INTEGER,
  PRIMARY KEY (response_id, fragment_id, "order")
);

CREATE TABLE IF NOT EXISTS tools (
  id INTEGER PRIMARY KEY,
  hash TEXT,
  name TEXT,
  description TEXT,
  input_schema TEXT,
  plugin TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS tools_hash ON tools(hash);

CREATE TABLE IF NOT EXISTS tool_responses (
  tool_id INTEGER REFERENCES tools(id),
  response_id TEXT REFERENCES responses(id),
  PRIMARY KEY (tool_id, response_id)
);

CREATE TABLE IF NOT EXISTS tool_instances (
  id INTEGER PRIMARY KEY,
  plugin TEXT,
  name TEXT,
  arguments TEXT
);

CREATE TABLE IF NOT EXISTS tool_calls (
  id INTEGER PRIMARY KEY,
  response_id TEXT REFERENCES responses(id),
  tool_id INTEGER REFERENCES tools(id),
  name TEXT,
  arguments TEXT,
  tool_call_id TEXT
);

CREATE TABLE IF NOT EXISTS tool_results (
  id INTEGER PRIMARY KEY,
  response_id TEXT REFERENCES responses(id),
  tool_id INTEGER REFERENCES tools(id),
  name TEXT,
  output TEXT,
  tool_call_id TEXT,
  instance_id INTEGER REFERENCES tool_instances(id),
  exception TEXT
);

CREATE TABLE IF NOT EXISTS tool_results_attachments (
  tool_result_id INTEGER REFERENCES tool_results(id),
  attachment_id TEXT REFERENCES attachments(id),
  "order" INTEGER,
  PRIMARY KEY (tool_result_id, attachment_id, "order")
);

CREATE TABLE IF NOT EXISTS messages (
  hash TEXT PRIMARY KEY,
  parent_hash TEXT REFERENCES messages(hash),
  role TEXT,
  provider_metadata TEXT
);
CREATE INDEX IF NOT EXISTS messages_parent ON messages(parent_hash);

CREATE TABLE IF NOT EXISTS parts (
  id INTEGER PRIMARY KEY,
  message_hash TEXT REFERENCES messages(hash),
  position INTEGER,
  type TEXT,
  tool_name TEXT,
  text TEXT,
  payload TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS parts_message_position ON parts(message_hash, position);

CREATE TABLE IF NOT EXISTS part_attachments (
  part_id INTEGER REFERENCES parts(id),
  attachment_id TEXT REFERENCES attachments(id),
  "order" INTEGER,
  PRIMARY KEY (part_id, attachment_id, "order")
);
CREATE INDEX IF NOT EXISTS part_attachments_attachment ON part_attachments(attachment_id);

CREATE TABLE IF NOT EXISTS part_fragments (
  part_id INTEGER REFERENCES parts(id),
  fragment_id INTEGER REFERENCES fragments(id),
  "order" INTEGER,
  PRIMARY KEY (part_id, fragment_id, "order")
);
CREATE INDEX IF NOT EXISTS part_fragments_fragment ON part_fragments(fragment_id);

CREATE TABLE IF NOT EXISTS threads (
  id TEXT PRIMARY KEY,
  name TEXT,
  tip_message_hash TEXT REFERENCES messages(hash),
  forked_from TEXT REFERENCES threads(id),
  datetime_utc TEXT
);

CREATE TABLE IF NOT EXISTS turns (
  id TEXT PRIMARY KEY,
  thread_id TEXT REFERENCES threads(id),
  parent_message_hash TEXT REFERENCES messages(hash),
  tip_message_hash TEXT REFERENCES messages(hash),
  model TEXT,
  resolved_model TEXT,
  options_json TEXT,
  schema_id TEXT REFERENCES schemas(id),
  input_tokens INTEGER,
  output_tokens INTEGER,
  token_details TEXT,
  duration_ms INTEGER,
  datetime_utc TEXT,
  response_json TEXT
);

CREATE TABLE IF NOT EXISTS turn_tools (
  turn_id TEXT REFERENCES turns(id),
  tool_id INTEGER REFERENCES tools(id),
  instance_id INTEGER REFERENCES tool_instances(id),
  PRIMARY KEY (turn_id, tool_id)
);

CREATE TABLE IF NOT EXISTS turn_fragments (
  turn_id TEXT REFERENCES turns(id),
  fragment_id INTEGER REFERENCES fragments(id),
  "order" INTEGER,
  kind TEXT,
  PRIMARY KEY (turn_id, fragment_id, kind, "order")
);
CREATE INDEX IF NOT EXISTS turn_fragments_fragment ON turn_fragments(fragment_id);

CREATE TABLE IF NOT EXISTS turn_search (
  id INTEGER PRIMARY KEY,
  turn_id TEXT REFERENCES turns(id),
  prompt TEXT,
  response TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS turn_search_turn ON turn_search(turn_id);

CREATE TABLE IF NOT EXISTS tool_instantiations (
  turn_id TEXT REFERENCES turns(id),
  tool_call_id TEXT,
  instance_id INTEGER REFERENCES tool_instances(id),
  PRIMARY KEY (turn_id, tool_call_id)
);

CREATE VIRTUAL TABLE IF NOT EXISTS responses_fts USING FTS5 (
  prompt, response, content="responses"
);
CREATE TRIGGER IF NOT EXISTS responses_fts_ai AFTER INSERT ON responses BEGIN
  INSERT INTO responses_fts (rowid, prompt, response)
  VALUES (new.rowid, new.prompt, new.response);
END;
CREATE TRIGGER IF NOT EXISTS responses_fts_ad AFTER DELETE ON responses BEGIN
  INSERT INTO responses_fts (responses_fts, rowid, prompt, response)
  VALUES ('delete', old.rowid, old.prompt, old.response);
END;
CREATE TRIGGER IF NOT EXISTS responses_fts_au AFTER UPDATE ON responses BEGIN
  INSERT INTO responses_fts (responses_fts, rowid, prompt, response)
  VALUES ('delete', old.rowid, old.prompt, old.response);
  INSERT INTO responses_fts (rowid, prompt, response)
  VALUES (new.rowid, new.prompt, new.response);
END;

CREATE VIRTUAL TABLE IF NOT EXISTS turn_search_fts USING FTS5 (
  prompt, response, content="turn_search"
);
CREATE TRIGGER IF NOT EXISTS turn_search_fts_ai AFTER INSERT ON turn_search BEGIN
  INSERT INTO turn_search_fts (rowid, prompt, response)
  VALUES (new.rowid, new.prompt, new.response);
END;
CREATE TRIGGER IF NOT EXISTS turn_search_fts_ad AFTER DELETE ON turn_search BEGIN
  INSERT INTO turn_search_fts (turn_search_fts, rowid, prompt, response)
  VALUES ('delete', old.rowid, old.prompt, old.response);
END;
CREATE TRIGGER IF NOT EXISTS turn_search_fts_au AFTER UPDATE ON turn_search BEGIN
  INSERT INTO turn_search_fts (turn_search_fts, rowid, prompt, response)
  VALUES ('delete', old.rowid, old.prompt, old.response);
  INSERT INTO turn_search_fts (rowid, prompt, response)
  VALUES (new.rowid, new.prompt, new.response);
END;
"#;

const MESSAGE_TREE_SQL: &str = "
DROP VIEW IF EXISTS message_tree;
CREATE VIEW message_tree AS
with recursive msg as (
  select m.hash, m.parent_hash, m.role, m.rowid as rid,
    replace(coalesce(
      nullif(p.text, ''),
      (select f.content from part_fragments pf
       join fragments f on f.id = pf.fragment_id
       where pf.part_id = p.id
       order by pf.\"order\" limit 1),
      '[' || coalesce(p.type, 'empty') || ']'
    ), char(10), ' ') as text,
    (select group_concat(p2.tool_name, ', ') from parts p2
     where p2.message_hash = m.hash and p2.type = 'tool_result'
       and p2.tool_name is not null) as tools
  from messages m
  left join parts p on p.message_hash = m.hash and p.position = 0
),
tree as (
  select hash, text, tools, 0 as depth,
    printf('%012d', rid) as path, hash as root_hash
  from msg where parent_hash is null
  union all
  select msg.hash, msg.text, msg.tools, t.depth + 1,
    t.path || '/' || printf('%012d', msg.rid),
    t.root_hash
  from msg join tree t on msg.parent_hash = t.hash
),
turn_chain as (
  select t.id as turn_id, t.datetime_utc, m.hash, m.parent_hash
  from turns t join messages m on m.hash = t.tip_message_hash
  union all
  select tc.turn_id, tc.datetime_utc, m.hash, m.parent_hash
  from turn_chain tc join messages m on m.hash = tc.parent_hash
)
select
  t.root_hash,
  strftime('%Y-%m-%d %H:%M:%S',
    (select min(tc.datetime_utc) from turn_chain tc where tc.hash = t.hash)
  ) as datetime,
  replace(hex(zeroblob(t.depth)), '00', '    ') || substr(t.text, 1, 60)
    as message,
  coalesce(t.tools, '') as tools,
  t.hash as message_hash,
  t.path
from tree t
order by t.path;
";

#[cfg(test)]
mod ulid_tests {
    use super::*;

    #[test]
    fn ulid_shape_and_order() {
        let a = ulid();
        let b = ulid();
        assert_eq!(a.len(), 26);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
        assert!(b > a, "same-process ulids must be strictly monotonic");
        assert!(!a.contains('i') && !a.contains('l') && !a.contains('o') && !a.contains('u'));
    }

    #[test]
    fn encode_known_layout() {
        assert_eq!(encode_ulid(0, 1), "00000000000000000000000001");
        assert_eq!(&encode_ulid(1, 0)[..10], "0000000001");
    }

    #[test]
    fn datetime_formats() {
        assert!(now_turn_datetime().contains('T'));
        assert!(now_turn_datetime().ends_with("+00:00"));
        assert!(now_thread_datetime().contains(' '));
    }
}
