//! Content-addressed message store: the write path (threads/turns/messages/
//! parts with "b2:"-prefixed blake2b hashes) and the reader over it,
//! mirroring the reference's LogStore. The legacy `responses`/`conversations`
//! tables are still created (schema parity with the reference tool) but no
//! longer read: every store this binary writes is threads/turns.

use rusqlite::{Connection, Transaction, params};
use serde_json::{Value, json};

use crate::blake2::blake2b16_hex;
use crate::core::db::{Db, ulid};
use crate::hash::sha256_hex;
use crate::jsonfmt;

// data model (the subset of the original's parts we produce)

#[derive(Clone)]
pub struct StoredAttachment {
    pub path: Option<String>,
    pub url: Option<String>,
    pub mime_type: Option<String>,
    /// bytes actually sent — always loaded for content-backed attachments
    pub content: Vec<u8>,
}

#[derive(Clone)]
pub enum Part {
    Text(String),
    Reasoning {
        text: String,
        redacted: bool,
    },
    Attachment(StoredAttachment),
    /// agent extension: a tool call the assistant made
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// agent extension: the result fed back for a tool call
    ToolResult {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone)]
pub struct Message {
    pub role: String,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn text(role: &str, text: impl Into<String>) -> Message {
        Message {
            role: role.to_string(),
            parts: vec![Part::Text(text.into())],
        }
    }
}

/// The hashed form of an attachment: content identity + media type.
/// URL attachments hash the URL itself (the log records which URL was
/// sent); path/content attachments hash the bytes.
fn canonical_attachment(a: &StoredAttachment) -> Value {
    let content_id = if let Some(url) = &a.url {
        // sha256 of json.dumps({"url": ...}) — default separators, ASCII-escaped
        sha256_hex(jsonfmt::dumps(&json!({"url": url})).as_bytes())
    } else if !a.content.is_empty() {
        sha256_hex(&a.content)
    } else if let Some(path) = &a.path {
        match std::fs::read(path) {
            Ok(bytes) => sha256_hex(&bytes),
            Err(_) => format!("missing:{path}"),
        }
    } else {
        sha256_hex(&a.content)
    };
    json!({"id": content_id, "type": a.mime_type.clone()})
}

fn attachment_row_id(a: &StoredAttachment) -> String {
    canonical_attachment(a)["id"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Part.to_dict() with attachments already in canonical (hashed) form —
/// this is what the message hash is taken over.
fn part_hash_dict(part: &Part) -> Value {
    match part {
        Part::Text(text) => json!({"type": "text", "text": text}),
        Part::Reasoning { text, redacted } => {
            let mut d = json!({"type": "reasoning", "text": text});
            if *redacted {
                d["redacted"] = json!(true);
            }
            d
        }
        Part::Attachment(a) => json!({
            "type": "attachment",
            "attachment": canonical_attachment(a),
        }),
        Part::ToolCall {
            id,
            name,
            arguments,
        } => json!({
            "type": "tool_call",
            "id": id,
            "name": name,
            "arguments": arguments,
        }),
        Part::ToolResult {
            call_id,
            name,
            content,
            is_error,
        } => {
            let mut d = json!({
                "type": "tool_result",
                "call_id": call_id,
                "name": name,
                "content": content,
            });
            if *is_error {
                d["is_error"] = json!(true);
            }
            d
        }
    }
}

fn message_hash_dict(message: &Message) -> Value {
    json!({
        "role": message.role,
        "parts": message.parts.iter().map(part_hash_dict).collect::<Vec<_>>(),
    })
}

/// "b2:" + blake2b-16 over canonical_json({"parent": parent, "message": ...}).
fn message_hash(message: &Message, parent: Option<&str>) -> String {
    let d = json!({
        "parent": parent,
        "message": message_hash_dict(message),
    });
    format!(
        "b2:{}",
        blake2b16_hex(jsonfmt::canonical_json(&d).as_bytes())
    )
}

// low-level ensures

fn ensure_attachment(
    conn: &Connection,
    id: &str,
    a: &StoredAttachment,
) -> Result<(), rusqlite::Error> {
    // path/url-backed attachments keep no bytes in the store, like the original
    let content: Option<&[u8]> = if a.path.is_none() && a.url.is_none() {
        Some(&a.content)
    } else {
        None
    };
    conn.execute(
        "INSERT OR REPLACE INTO attachments (id, type, path, url, content) VALUES (?1,?2,?3,?4,?5)",
        params![id, a.mime_type, a.path, a.url, content],
    )?;
    Ok(())
}

/// make_schema_id: blake2b-16 of the compact (unsorted, ASCII-escaped) JSON.
pub fn make_schema_id(schema: &Value) -> (String, String) {
    let compact = jsonfmt::schema_compact(schema);
    let id = blake2b16_hex(compact.as_bytes());
    (id, compact)
}

fn _ensure_message(
    conn: &Connection,
    message: &Message,
    parent: Option<&str>,
) -> Result<String, rusqlite::Error> {
    let hash = message_hash(message, parent);
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO messages (hash, parent_hash, role, provider_metadata) VALUES (?1,?2,?3,NULL)",
        params![hash, parent, message.role],
    )?;
    if inserted > 0 {
        for (position, part) in message.parts.iter().enumerate() {
            write_part(conn, &hash, position, part)?;
        }
    }
    Ok(hash)
}

/// (type, payload, text column, attachments with their precomputed row ids)
type PartWrite<'a> = (
    String,
    serde_json::Map<String, Value>,
    Option<String>,
    Vec<(&'a StoredAttachment, String)>,
);

fn write_part(
    conn: &Connection,
    message_hash: &str,
    position: usize,
    part: &Part,
) -> Result<(), rusqlite::Error> {
    // storage payload: part.to_dict() minus type, text popped into its column,
    // attachments replaced by their ids (computed once, shared with the row
    // insert — hashing the bytes twice per attachment is wasted work)
    let (part_type, payload, text, attachments): PartWrite<'_> = match part {
        Part::Text(t) => (
            "text".into(),
            serde_json::Map::new(),
            Some(t.clone()),
            Vec::new(),
        ),
        Part::Reasoning { text, .. } => (
            "reasoning".into(),
            serde_json::Map::new(),
            Some(text.clone()),
            Vec::new(),
        ),
        Part::Attachment(a) => {
            let mut map = serde_json::Map::new();
            map.insert("attachment".into(), json!({"id": attachment_row_id(a)}));
            (
                "attachment".into(),
                map,
                None,
                vec![(a, attachment_row_id(a))],
            )
        }
        Part::ToolCall {
            id,
            name,
            arguments,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("id".into(), json!(id));
            map.insert("name".into(), json!(name));
            map.insert("arguments".into(), arguments.clone());
            ("tool_call".into(), map, None, Vec::new())
        }

        Part::ToolResult {
            call_id,
            name,
            content,
            is_error,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("call_id".into(), json!(call_id));
            map.insert("name".into(), json!(name));
            if *is_error {
                map.insert("is_error".into(), json!(true));
            }
            ("tool_result".into(), map, Some(content.clone()), Vec::new())
        }
    };
    let tool_name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let payload_json = if payload.is_empty() {
        None
    } else {
        Some(jsonfmt::dumps(&Value::Object(payload)))
    };
    conn.execute(
        "INSERT INTO parts (message_hash, position, type, tool_name, text, payload) VALUES (?1,?2,?3,?4,?5,?6)",
        params![message_hash, position as i64, part_type, tool_name, text, payload_json],
    )?;
    let part_id: i64 = conn.last_insert_rowid();
    for (order, (attachment, attachment_id)) in attachments.iter().enumerate() {
        ensure_attachment(conn, attachment_id, attachment)?;
        conn.execute(
            "INSERT INTO part_attachments (part_id, attachment_id, \"order\") VALUES (?1,?2,?3)",
            params![part_id, attachment_id, order as i64],
        )?;
    }
    Ok(())
}

/// Store a chain of messages, returning the tip hash.
fn ensure_chain(
    conn: &Connection,
    messages: &[Message],
    parent: Option<&str>,
) -> Result<Option<String>, rusqlite::Error> {
    let mut tip = parent.map(|s| s.to_string());
    for message in messages {
        tip = Some(_ensure_message(conn, message, tip.as_deref())?);
    }
    Ok(tip)
}

fn conversation_name(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 32 {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(31).collect();
        format!("{truncated}…")
    }
}

// the turn-level write path

pub struct TurnToLog<'a> {
    /// existing thread/conversation id, if continuing
    pub thread_id: Option<&'a str>,
    /// hash of the stored message this turn appends to — for continuations
    /// this is the thread's current tip, so the stored chain is reused
    /// verbatim (reasoning parts and all) instead of being rebuilt lossily
    pub history_tip: Option<&'a str>,
    /// the current turn's input: [system?, ] user message with attachments —
    /// prior turns already live in the stored chain when history_tip is set
    pub input_messages: &'a [Message],
    /// assistant output: reasoning part (if any) then text
    pub reasoning: Option<&'a str>,
    pub response_text: &'a str,
    pub model: &'a str,
    pub options: &'a serde_json::Map<String, Value>,
    pub schema: Option<&'a Value>,
    pub usage: Option<(u64, u64)>,
    pub duration_ms: Option<i64>,
}

/// Record a completed exchange. Returns the turn id. The whole write runs in
/// one transaction; a failure rolls back, is reported on stderr, and the turn
/// id is still returned so callers keep their shape.
/// A finished exchange ready to persist: the caller supplies the input
/// messages it sent (without the system message); `log_completed_turn`
/// resolves the stored chain tip, prepends the system message when the
/// thread has none yet, builds the options map and writes the turn.
pub struct CompletedTurn<'a> {
    pub conversation_id: Option<&'a str>,
    pub system: Option<&'a str>,
    pub input_messages: &'a [Message],
    pub reasoning: Option<&'a str>,
    pub response_text: &'a str,
    pub model: &'a str,
    pub options: &'a [(String, String)],
    pub schema: Option<&'a Value>,
    pub usage: Option<(u64, u64)>,
    pub duration_ms: i64,
}

pub fn log_completed_turn(db: &Db, t: &CompletedTurn) -> String {
    // continuations append to the stored chain tip (full fidelity, dedup);
    // fresh threads store [system?, user...] from scratch
    let history_tip = t.conversation_id.and_then(|cid| thread_tip(db, cid));
    let mut input_messages: Vec<Message> = Vec::new();
    if history_tip.is_none()
        && let Some(system) = t.system
        && !system.is_empty()
    {
        input_messages.push(Message::text("system", system.to_string()));
    }
    input_messages.extend(t.input_messages.iter().cloned());

    let mut options_map = serde_json::Map::new();
    for (k, v) in t.options {
        options_map.insert(
            k.clone(),
            serde_json::from_str::<Value>(v).unwrap_or_else(|_| json!(v)),
        );
    }
    log_turn(
        db,
        &TurnToLog {
            thread_id: t.conversation_id,
            history_tip: history_tip.as_deref(),
            input_messages: &input_messages,
            reasoning: t.reasoning,
            response_text: t.response_text,
            model: t.model,
            options: &options_map,
            schema: t.schema,
            usage: t.usage,
            duration_ms: Some(t.duration_ms),
        },
    )
}

pub fn log_turn(db: &Db, turn: &TurnToLog) -> String {
    let turn_id = ulid();
    let tx = match db.conn().unchecked_transaction() {
        Ok(tx) => tx,
        Err(e) => {
            eprintln!("Warning: failed to log turn to database: {e}");
            return turn_id;
        }
    };
    if let Err(e) = log_turn_tx(&tx, turn, &turn_id) {
        // dropping tx rolls the partial write back
        eprintln!("Warning: failed to log turn to database: {e}");
        return turn_id;
    }
    if let Err(e) = tx.commit() {
        eprintln!("Warning: failed to commit logged turn: {e}");
    }
    turn_id
}

fn log_turn_tx(
    conn: &Transaction<'_>,
    turn: &TurnToLog,
    turn_id: &str,
) -> Result<(), rusqlite::Error> {
    let thread_id = match turn.thread_id {
        Some(id) => {
            let exists: i64 = conn.query_row(
                "SELECT count(*) FROM threads WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            if exists == 0 {
                conn.execute(
                    "INSERT INTO threads (id, name, tip_message_hash, forked_from, datetime_utc) VALUES (?1,?2,NULL,NULL,?3)",
                    params![id, Option::<String>::None, crate::core::db::now_thread_datetime()],
                )?;
            }
            id.to_string()
        }
        None => {
            let id = ulid();
            let name_source = turn
                .input_messages
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .and_then(|m| {
                    m.parts.iter().find_map(|p| match p {
                        Part::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                })
                .or_else(|| {
                    turn.input_messages
                        .iter()
                        .find(|m| m.role == "system")
                        .and_then(|m| {
                            m.parts.iter().find_map(|p| match p {
                                Part::Text(t) => Some(t.clone()),
                                _ => None,
                            })
                        })
                })
                .unwrap_or_default();
            conn.execute(
                "INSERT INTO threads (id, name, tip_message_hash, forked_from, datetime_utc) VALUES (?1,?2,NULL,NULL,?3)",
                params![id, conversation_name(&name_source), crate::core::db::now_thread_datetime()],
            )?;
            id
        }
    };

    let parent = ensure_chain(conn, turn.input_messages, turn.history_tip)?;
    let mut own_parts = Vec::new();
    if let Some(reasoning) = turn.reasoning
        && !reasoning.is_empty()
    {
        own_parts.push(Part::Reasoning {
            text: reasoning.to_string(),
            redacted: false,
        });
    }
    own_parts.push(Part::Text(turn.response_text.to_string()));
    let own = vec![Message {
        role: "assistant".into(),
        parts: own_parts,
    }];
    let tip = ensure_chain(conn, &own, parent.as_deref())?;

    let mut schema_id = None;
    if let Some(schema) = turn.schema {
        let (id, content) = make_schema_id(schema);
        conn.execute(
            "INSERT OR IGNORE INTO schemas (id, content) VALUES (?1,?2)",
            params![id, content],
        )?;
        schema_id = Some(id);
    }

    let options_json = if turn.options.is_empty() {
        None
    } else {
        Some(jsonfmt::dumps(&Value::Object(turn.options.clone())))
    };

    conn.execute(
        "INSERT OR REPLACE INTO turns
         (id, thread_id, parent_message_hash, tip_message_hash, model, resolved_model,
          options_json, schema_id, input_tokens, output_tokens, token_details,
          duration_ms, datetime_utc, response_json)
         VALUES (?1,?2,?3,?4,?5,?5,?6,?7,?8,?9,NULL,?10,?11,NULL)",
        params![
            turn_id,
            thread_id,
            parent,
            tip,
            turn.model,
            options_json,
            schema_id,
            turn.usage.map(|u| u.0 as i64),
            turn.usage.map(|u| u.1 as i64),
            turn.duration_ms,
            crate::core::db::now_turn_datetime(),
        ],
    )?;

    refresh_turn_search(conn, turn_id)?;
    conn.execute(
        "UPDATE threads SET tip_message_hash = ?2 WHERE id = ?1",
        params![thread_id, tip],
    )?;
    Ok(())
}

const TURN_SEARCH_LITERAL: &str = "coalesce(
  parts.text,
  (select group_concat(json_extract(je.value, '$.literal'), '')
     from json_each(parts.payload, '$.text_ref') je
    where json_extract(je.value, '$.literal') is not null)
)";

fn refresh_turn_search(conn: &Connection, turn_id: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM turn_search WHERE turn_id = ?1",
        params![turn_id],
    )?;
    let sql = TURN_SEARCH_INSERT_SQL.replace("{LITERAL}", TURN_SEARCH_LITERAL);
    conn.execute(&sql, params![turn_id])?;
    Ok(())
}

const TURN_SEARCH_INSERT_SQL: &str = r#"
with recursive output_messages(turn_id, hash) as (
    select turns.id, turns.tip_message_hash
      from turns
     where turns.tip_message_hash is not null
       and (turns.parent_message_hash is null
            or turns.tip_message_hash != turns.parent_message_hash)
       and turns.id = ?1
    union all
    select om.turn_id, messages.parent_hash
      from output_messages om
      join messages on messages.hash = om.hash
      join turns on turns.id = om.turn_id
     where messages.parent_hash is not null
       and (turns.parent_message_hash is null
            or messages.parent_hash != turns.parent_message_hash)
),
prompt_chain(hash, depth) as (
    select turns.tip_message_hash, 0
      from turns
     where turns.tip_message_hash is not null and turns.id = ?1
    union all
    select messages.parent_hash, prompt_chain.depth + 1
      from prompt_chain
      join messages on messages.hash = prompt_chain.hash
     where messages.parent_hash is not null
),
first_user as (
    select prompt_chain.hash as hash
      from prompt_chain
      join messages on messages.hash = prompt_chain.hash
     where messages.role = 'user'
     order by prompt_chain.depth
     limit 1
),
prompt_text as (
    select turns.id as turn_id,
           (select group_concat({LITERAL}, '')
              from parts
             where parts.message_hash = (select hash from first_user)
               and parts.type = 'text'
             order by parts.position) as text
      from turns
     where (select hash from first_user) is not null and turns.id = ?1
),
response_text as (
    select om.turn_id, group_concat(part_text.text, '') as text
      from output_messages om
      join messages on messages.hash = om.hash and messages.role = 'assistant'
      join (
          select parts.message_hash, parts.position, {LITERAL} as text
            from parts where parts.type = 'text'
      ) part_text on part_text.message_hash = om.hash
     group by om.turn_id
)
insert into turn_search (turn_id, prompt, response)
select turns.id,
       coalesce(prompt_text.text, ''),
       coalesce(response_text.text, '')
  from turns
  left join prompt_text on prompt_text.turn_id = turns.id
  left join response_text on response_text.turn_id = turns.id
 where (coalesce(prompt_text.text, '') != ''
        or coalesce(response_text.text, '') != '') and turns.id = ?1
"#;

// reading

/// The chain from `tip` back to its root in ONE query (recursive CTE),
/// newest first, each entry carrying the message role.
pub fn chain_with_roles(conn: &Connection, tip: &str) -> Vec<(String, Option<String>)> {
    let mut stmt = match conn.prepare(
        "WITH RECURSIVE chain(hash, parent_hash, role) AS (
             SELECT m.hash, m.parent_hash, m.role FROM messages m WHERE m.hash = ?1
             UNION ALL
             SELECT m2.hash, m2.parent_hash, m2.role FROM messages m2
              JOIN chain c ON m2.hash = c.parent_hash
             WHERE c.parent_hash IS NOT NULL
         ) SELECT hash, role FROM chain LIMIT 100000",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: cannot read the message chain: {e}");
            return Vec::new();
        }
    };
    match stmt.query_map(params![tip], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    }) {
        Ok(rows) => rows
            .filter_map(|r| {
                r.map_err(|e| eprintln!("Warning: unreadable chain row: {e}"))
                    .ok()
            })
            .collect(),
        Err(e) => {
            eprintln!("Warning: cannot read the message chain: {e}");
            Vec::new()
        }
    }
}

/// Rebuild a stored message: role + parts with text (or text_ref resolved
/// via the fragments table), attachment summaries, and the raw part payload
/// (agent tool parts reconstruct their structured fields from it).
pub struct ReadPart {
    pub part_type: String,
    pub text: Option<String>,
    pub payload: Option<String>,
}

/// One stored message by hash: its role plus parts, in position order.
/// A single joined query (role + parts together).
pub fn message_parts(conn: &Connection, hash: &str) -> Option<(String, Vec<ReadPart>)> {
    let mut stmt = conn
        .prepare(
            "SELECT m.role, p.type, p.text, p.payload FROM messages m
             LEFT JOIN parts p ON p.message_hash = m.hash
             WHERE m.hash = ?1 ORDER BY p.position",
        )
        .map_err(|e| eprintln!("Warning: cannot read message {hash}: {e}"))
        .ok()?;
    let rows = stmt
        .query_map(params![hash], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| eprintln!("Warning: cannot read message {hash}: {e}"))
        .ok()?;
    let mut role: Option<String> = None;
    let mut parts = Vec::new();
    for row in rows {
        let (r, part_type, text, payload) = match row {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Warning: unreadable part of message {hash}: {e}");
                return None;
            }
        };
        if role.is_none() {
            role = Some(r);
        }
        let text = match text {
            Some(t) => Some(t),
            None => payload.as_ref().and_then(|p| resolve_text_ref(conn, p)),
        };
        parts.push(ReadPart {
            part_type: part_type.unwrap_or_default(),
            text,
            payload,
        });
    }
    role.map(|role| (role, parts))
}

/// Full chain of messages ending at `tip`, oldest first. TWO queries total
/// (the chain itself, then every part on it) instead of one per message.
fn chain_parts(conn: &Connection, tip: &str) -> Vec<(String, Vec<ReadPart>)> {
    let order = chain_with_roles(conn, tip); // newest first
    if order.is_empty() {
        return Vec::new();
    }
    let mut by_hash: std::collections::HashMap<String, Vec<ReadPart>> =
        std::collections::HashMap::with_capacity(order.len());
    let mut stmt = match conn.prepare(
        "WITH RECURSIVE chain(hash) AS (
             SELECT ?1
             UNION ALL
             SELECT m.parent_hash FROM messages m JOIN chain c ON m.hash = c.hash
              WHERE m.parent_hash IS NOT NULL
         )
         SELECT p.message_hash, p.type, p.text, p.payload
           FROM parts p JOIN chain c ON p.message_hash = c.hash
          ORDER BY p.message_hash, p.position",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: cannot read the chain's parts: {e}");
            return Vec::new();
        }
    };
    let rows = match stmt.query_map(params![tip], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("Warning: cannot read the chain's parts: {e}");
            return Vec::new();
        }
    };
    for row in rows {
        let (hash, part_type, text, payload) = match row {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Warning: unreadable chain part: {e}");
                continue;
            }
        };
        let text = match text {
            Some(t) => Some(t),
            None => payload.as_ref().and_then(|p| resolve_text_ref(conn, p)),
        };
        by_hash.entry(hash).or_default().push(ReadPart {
            part_type: part_type.unwrap_or_default(),
            text,
            payload,
        });
    }
    order
        .into_iter()
        .rev()
        .map(|(hash, _)| {
            let parts = by_hash.remove(&hash).unwrap_or_default();
            (hash, parts)
        })
        .collect()
}

fn resolve_text_ref(conn: &Connection, payload: &str) -> Option<String> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let pieces = value.get("text_ref")?.as_array()?;
    let mut out = String::new();
    for piece in pieces {
        if let Some(literal) = piece.get("literal").and_then(|v| v.as_str()) {
            out.push_str(literal);
        } else if let Some(fragment_id) = piece.get("fragment").and_then(|v| v.as_i64()) {
            match conn.query_row(
                "SELECT content FROM fragments WHERE id = ?1",
                params![fragment_id],
                |r| r.get::<_, String>(0),
            ) {
                Ok(content) => out.push_str(&content),
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    eprintln!("Warning: fragment {fragment_id} referenced but missing")
                }
                Err(e) => eprintln!("Warning: cannot read fragment {fragment_id}: {e}"),
            }
        }
    }
    Some(out)
}

/// The message hash a thread currently points at.
pub fn thread_chain(db: &Db, thread_id: &str) -> Vec<(String, Vec<ReadPart>)> {
    match thread_tip(db, thread_id) {
        Some(tip) => chain_parts(db.conn(), &tip),
        None => Vec::new(),
    }
}

/// The message hash a thread currently points at.
pub fn thread_tip(db: &Db, thread_id: &str) -> Option<String> {
    match db.conn().query_row(
        "SELECT tip_message_hash FROM threads WHERE id = ?1",
        params![thread_id],
        |r| r.get::<_, Option<String>>(0),
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            eprintln!("Warning: cannot read the tip of thread {thread_id}: {e}");
            None
        }
    }
}

/// A conversation summary for session browsing (`llm agent /resume`).
#[derive(Clone)]
pub struct ThreadSummary {
    pub id: String,
    pub turns: usize,
    pub last: String,
}

/// Most recently touched threads, newest first.
pub fn recent_threads(db: &Db, limit: usize) -> Vec<ThreadSummary> {
    let mut stmt = match db.conn().prepare(
        "SELECT thread_id, COUNT(*), coalesce(MAX(datetime_utc), '')
         FROM turns WHERE thread_id IS NOT NULL
         GROUP BY thread_id ORDER BY MAX(datetime_utc) DESC LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: cannot list threads: {e}");
            return Vec::new();
        }
    };
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(ThreadSummary {
            id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            turns: r.get::<_, i64>(1)? as usize,
            last: r.get::<_, String>(2)?,
        })
    });
    match rows {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Full message chain of a thread (roles + parts), oldest first — the
/// wire-level source for `llm agent` continuation.
/// First prompt of a thread (the preview shown in the /resume picker).
/// Walks the first turn's message chain to the nearest user message — the
/// same shape as turn_search's prompt rule, but live, so sessions logged
/// before that rule existed still preview correctly.
pub fn thread_first_prompt(db: &Db, thread_id: &str) -> String {
    // the indexed turn_search row first (the write path maintains it with
    // the same rule); the live chain walk below is the fallback for
    // sessions logged before that rule existed
    let indexed: Option<String> = match db.conn().query_row(
        "SELECT ts.prompt FROM turns t
          JOIN turn_search ts ON ts.turn_id = t.id
         WHERE t.thread_id = ?1 AND ts.prompt != ''
         ORDER BY t.datetime_utc ASC LIMIT 1",
        params![thread_id],
        |r| r.get(0),
    ) {
        Ok(p) => Some(p),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            eprintln!("Warning: cannot read the first prompt of {thread_id}: {e}");
            None
        }
    };
    if let Some(prompt) = indexed {
        return prompt;
    }
    db.conn()
        .query_row(
            "WITH RECURSIVE chain(hash, depth) AS (
                 SELECT tip, 0
                   FROM (SELECT t.tip_message_hash AS tip
                           FROM turns t
                          WHERE t.thread_id = ?1 AND t.tip_message_hash IS NOT NULL
                          ORDER BY t.datetime_utc ASC
                          LIMIT 1)
                 UNION ALL
                 SELECT m.parent_hash, chain.depth + 1
                   FROM chain JOIN messages m ON m.hash = chain.hash
                  WHERE m.parent_hash IS NOT NULL
             ),
             first_user AS (
                 SELECT chain.hash AS hash
                   FROM chain JOIN messages m ON m.hash = chain.hash
                  WHERE m.role = 'user'
                  ORDER BY chain.depth LIMIT 1
             )
             SELECT coalesce((
                 SELECT group_concat(p.text, '')
                   FROM parts p
                  WHERE p.message_hash = (SELECT hash FROM first_user)
                    AND p.type = 'text'
             ), '')",
            params![thread_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
}

/// Last user question of a thread — the picker preview should show the most
/// recent thing the user asked, not the first prompt.
pub fn thread_last_prompt(db: &Db, thread_id: &str) -> String {
    // the most recent turn whose first user message has text, newest first
    let indexed: Option<String> = match db.conn().query_row(
        "SELECT ts.prompt FROM turns t
          JOIN turn_search ts ON ts.turn_id = t.id
         WHERE t.thread_id = ?1 AND ts.prompt != ''
         ORDER BY t.datetime_utc DESC LIMIT 1",
        params![thread_id],
        |r| r.get(0),
    ) {
        Ok(p) => Some(p),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            eprintln!("Warning: cannot read the last prompt of {thread_id}: {e}");
            None
        }
    };
    if let Some(prompt) = indexed {
        return prompt;
    }
    thread_first_prompt(db, thread_id)
}

/// Most recent thread/conversation id across both identifier spaces.
/// Fork a thread: a new thread id pointing at the same message-chain tip.
/// The original keeps its tip, so the two sessions diverge from here without
/// touching each other (the chain itself is immutable and content-addressed).
pub fn fork_thread(db: &Db, source: &str) -> Option<String> {
    let (name, tip): (Option<String>, Option<String>) = match db.conn().query_row(
        "SELECT name, tip_message_hash FROM threads WHERE id = ?1",
        params![source],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return None,
        Err(e) => {
            eprintln!("Warning: cannot read thread {source} to fork: {e}");
            return None;
        }
    };
    let new_id = ulid();
    if let Err(e) = db.conn().execute(
        "INSERT INTO threads (id, name, tip_message_hash, forked_from, datetime_utc)              VALUES (?1,?2,?3,?4,?5)",
        params![
            new_id,
            name,
            tip,
            source,
            crate::core::db::now_thread_datetime()
        ],
    ) {
        eprintln!("Warning: cannot fork thread {source}: {e}");
        return None;
    }
    Some(new_id)
}

/// Undo the most recent turn: rewind the thread's tip to the chain tip that
/// round started from (each `turns` row records `parent_message_hash` —
/// the tip before the round) so the last round is no longer reachable.
pub fn undo_thread(db: &Db, thread_id: &str) -> Result<(), String> {
    let parent: Option<String> = match db.conn().query_row(
        "SELECT parent_message_hash FROM turns WHERE thread_id = ?1
          ORDER BY datetime_utc DESC LIMIT 1",
        params![thread_id],
        |r| r.get(0),
    ) {
        Ok(p) => p,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Err("no turns to undo".to_string()),
        Err(e) => return Err(format!("cannot read turns of {thread_id}: {e}")),
    };
    let Some(parent) = parent else {
        return Err("no turn boundary to rewind to".to_string());
    };
    db.conn()
        .execute(
            "UPDATE threads SET tip_message_hash = ?2 WHERE id = ?1",
            params![thread_id, parent],
        )
        .map(|_| ())
        .map_err(|e| format!("cannot undo {thread_id}: {e}"))
}

pub fn latest_conversation_id(db: &Db) -> Option<String> {
    match db
        .conn()
        .query_row("SELECT id FROM threads ORDER BY id DESC LIMIT 1", [], |r| {
            r.get(0)
        }) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            eprintln!("Warning: cannot find the latest conversation: {e}");
            None
        }
    }
}

/// Does this thread exist in either store?
/// Resolve a conversation-id prefix to its full id: an exact match passes
/// through, an unambiguous prefix completes, anything else is None.
pub fn resolve_conversation(db: &Db, prefix: &str) -> Option<String> {
    if conversation_exists(db, prefix) {
        return Some(prefix.to_string());
    }
    let mut stmt = db
        .conn()
        .prepare("SELECT id FROM threads WHERE id LIKE ?1 || '%' ORDER BY id")
        .map_err(|e| eprintln!("Warning: cannot resolve thread id {prefix}: {e}"))
        .ok()?;
    let ids: Vec<String> = stmt
        .query_map(params![prefix], |r| r.get(0))
        .ok()?
        .flatten()
        .collect();
    if ids.len() == 1 {
        ids.into_iter().next()
    } else {
        None
    }
}

/// The parsed options of a thread's newest turn: mode/cwd/kb provenance for
/// list rendering. None when the thread has no turns.
pub fn newest_turn_options(db: &Db, thread_id: &str) -> Option<Value> {
    let stored: Option<String> = match db.conn().query_row(
        "SELECT options_json FROM turns WHERE thread_id = ?1 ORDER BY id DESC LIMIT 1",
        params![thread_id],
        |r| r.get::<_, Option<String>>(0),
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            eprintln!("Warning: cannot read turn options of {thread_id}: {e}");
            None
        }
    };
    stored.and_then(|s| {
        serde_json::from_str(&s)
            .map_err(|e| eprintln!("Warning: unparsable turn options of {thread_id}: {e}"))
            .ok()
    })
}

pub fn conversation_exists(db: &Db, id: &str) -> bool {
    match db.conn().query_row(
        "SELECT count(*) FROM threads WHERE id = ?1",
        params![id],
        |r| r.get::<_, i64>(0),
    ) {
        Ok(n) => n > 0,
        Err(e) => {
            eprintln!("Warning: cannot look up conversation {id}: {e}");
            false
        }
    }
}

/// Model (and name) a conversation should continue with.
pub fn conversation_info(db: &Db, id: &str) -> Option<(String, Option<String>)> {
    if let Ok(model) = db.conn().query_row(
        "SELECT model FROM turns WHERE thread_id = ?1 ORDER BY datetime_utc DESC, id DESC LIMIT 1",
        params![id],
        |r| r.get::<_, Option<String>>(0),
    ) && let Some(model) = model
    {
        let name: Option<String> = db
            .conn()
            .query_row("SELECT name FROM threads WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .unwrap_or(None);
        return Some((model, name));
    }
    None
}

/// (id, parent hash, tip hash, model) per turn of a thread, oldest first
/// (ULID ids sort chronologically) — one query for the whole thread.
fn thread_turn_tips(
    db: &Db,
    thread_id: &str,
) -> Vec<(String, Option<String>, Option<String>, String)> {
    let mut stmt = match db.conn().prepare(
        "SELECT id, parent_message_hash, tip_message_hash, coalesce(model, '')
             FROM turns WHERE thread_id = ?1 ORDER BY id",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map(params![thread_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
        ))
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Concatenated text of a message's parts, optionally only those of one
/// type — no intermediate Vec<String> of clones.
fn parts_text(parts: &[ReadPart], only: Option<&str>) -> String {
    let mut out = String::new();
    for p in parts {
        if only.is_none_or(|t| p.part_type == t)
            && let Some(t) = &p.text
        {
            out.push_str(t);
        }
    }
    out
}

/// A message's text parts, fetched at most once per hash (the per-turn
/// system prompt resolves to the same root message for every turn).
fn cached_text(
    conn: &Connection,
    hash: &str,
    cache: &mut std::collections::HashMap<String, String>,
) -> Option<String> {
    if let Some(t) = cache.get(hash) {
        return Some(t.clone());
    }
    let text = message_parts(conn, hash).map(|(_, parts)| parts_text(&parts, Some("text")))?;
    cache.insert(hash.to_string(), text.clone());
    Some(text)
}

/// (chain newest-first, hash → index, nearest user/system at-or-older per
/// index) — the tables conversation_history resolves every turn from.
type ChainLookup = (
    Vec<(String, Option<String>)>,
    std::collections::HashMap<String, usize>,
    Vec<Option<usize>>,
    Vec<Option<usize>>,
);

/// One walk of a thread's final chain (newest first) plus the lookup tables
/// the per-turn prompt/system resolution needs: each message's index, and
/// for every index the nearest user / system message at that index or older.
fn chain_lookup_tables(conn: &Connection, final_tip: &str) -> ChainLookup {
    let chain = chain_with_roles(conn, final_tip); // newest first
    let mut index_of = std::collections::HashMap::with_capacity(chain.len());
    let mut next_user = vec![None; chain.len()];
    let mut next_system = vec![None; chain.len()];
    for i in (0..chain.len()).rev() {
        let role = chain[i].1.as_deref();
        next_user[i] = if role == Some("user") {
            Some(i)
        } else {
            next_user.get(i + 1).copied().flatten()
        };
        next_system[i] = if role == Some("system") {
            Some(i)
        } else {
            next_system.get(i + 1).copied().flatten()
        };
        index_of.insert(chain[i].0.clone(), i);
    }
    (chain, index_of, next_user, next_system)
}

/// Direct per-turn walk (prompt, system): the fallback for a turn whose
/// parent is somehow not on the thread's final chain.
fn walk_turn_prompt_system(conn: &Connection, parent: &str) -> (String, Option<String>) {
    let mut prompt = String::new();
    let mut system = None;
    let mut have_prompt = false;
    for (hash, role) in chain_with_roles(conn, parent) {
        match role.as_deref() {
            Some("user") if !have_prompt => {
                have_prompt = true;
                if let Some((_, parts)) = message_parts(conn, &hash) {
                    prompt = parts_text(&parts, Some("text"));
                }
            }
            Some("system") if system.is_none() => {
                if let Some((_, parts)) = message_parts(conn, &hash) {
                    system = Some(parts_text(&parts, Some("text")));
                }
            }
            _ => {}
        }
        if have_prompt && system.is_some() {
            break;
        }
    }
    (prompt, system)
}

/// (prompt, response, system, model) per turn of a conversation, oldest first.
///
/// prompt = newest user message on the input chain; system = newest system
/// message (a compaction summary re-becomes the system); response = the
/// turn's own assistant message (the tip itself). ONE walk of the final
/// chain feeds every turn (walking each turn's whole parent chain was
/// quadratic in session length); texts are fetched once per message.
pub fn conversation_history(
    db: &Db,
    thread_id: &str,
) -> Vec<(String, String, Option<String>, String)> {
    let turns = thread_turn_tips(db, thread_id);
    let (chain, index_of, next_user, next_system) = match turns.last().and_then(|t| t.2.as_deref())
    {
        Some(final_tip) => chain_lookup_tables(db.conn(), final_tip),
        None => (
            Vec::new(),
            std::collections::HashMap::new(),
            Vec::new(),
            Vec::new(),
        ),
    };
    let mut text_cache: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut rows: Vec<(String, String, Option<String>, String)> = Vec::with_capacity(turns.len());
    for (_turn_id, parent_tip, tip, model) in &turns {
        let mut prompt = String::new();
        let mut system = None;
        if let Some(parent) = parent_tip
            && let Some(&p) = index_of.get(parent)
        {
            if let Some(u) = next_user[p]
                && let Some(t) = cached_text(db.conn(), &chain[u].0, &mut text_cache)
            {
                prompt = t;
            }
            if let Some(s) = next_system[p]
                && let Some(t) = cached_text(db.conn(), &chain[s].0, &mut text_cache)
            {
                system = Some(t);
            }
        } else if let Some(parent) = parent_tip {
            let (p, s) = walk_turn_prompt_system(db.conn(), parent);
            prompt = p;
            system = s;
        }
        let mut response = String::new();
        if let Some(tip) = tip
            && let Some((role, parts)) = message_parts(db.conn(), tip)
            && role == "assistant"
        {
            response = parts_text(&parts, Some("text"));
        }
        rows.push((prompt, response, system, model.clone()));
    }
    rows
}

// read model (llm logs / llm prompt --json): row collection and annotation

pub struct RowFilters<'a> {
    pub conversation: Option<&'a str>,
    pub model: Option<&'a str>,
    pub query: Option<&'a str>,
    pub schema_id: Option<&'a str>,
    pub id_gt: Option<&'a str>,
    pub id_gte: Option<&'a str>,
    pub count: Option<i64>,
    pub search: bool,
}

/// Most recent conversation across both stores.
pub fn latest_conversation(db: &Db) -> Option<String> {
    match db.conn().query_row(
        "SELECT thread_id FROM turns ORDER BY id DESC LIMIT 1",
        [],
        |r| r.get(0),
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            eprintln!("Warning: cannot find the latest conversation: {e}");
            None
        }
    }
}

/// Query the turn store, newest-first (or relevance-ranked for -q).
pub fn collect_rows(db: &Db, f: &RowFilters) -> Vec<Value> {
    let mut rows: Vec<(f64, Value)> = Vec::new();

    // -- new store ---------------------------------------------------------
    let mut where_clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut from = String::from("turns");
    if f.search {
        from.push_str(" join turn_search_fts on turn_search_fts.rowid = (select turn_search.id from turn_search where turn_search.turn_id = turns.id)");
    }
    if let Some(conv) = f.conversation {
        where_clauses.push(format!("turns.thread_id = ?{}", params.len() + 1));
        params.push(Box::new(conv.to_string()));
    }
    if let Some(model) = f.model {
        where_clauses.push(format!(
            "(turns.model LIKE ?{} OR turns.resolved_model LIKE ?{})",
            params.len() + 1,
            params.len() + 1
        ));
        params.push(Box::new(format!("%{model}%")));
    }
    if let Some(q) = f.query
        && f.search
    {
        where_clauses.push(format!("turn_search_fts MATCH ?{}", params.len() + 1));
        params.push(Box::new(q.to_string()));
    }
    if let Some(sid) = f.schema_id {
        where_clauses.push(format!("turns.schema_id = ?{}", params.len() + 1));
        params.push(Box::new(sid.to_string()));
    }
    if let Some(gt) = f.id_gt {
        where_clauses.push(format!("turns.id > ?{}", params.len() + 1));
        params.push(Box::new(gt.to_string()));
    }
    if let Some(gte) = f.id_gte {
        where_clauses.push(format!("turns.id >= ?{}", params.len() + 1));
        params.push(Box::new(gte.to_string()));
    }
    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };
    let rank_sql = if f.search {
        "bm25(turn_search_fts, 10.0, 1.0)"
    } else {
        "0.0"
    };
    let limit_sql = f.count.map(|n| format!("LIMIT {n}")).unwrap_or_default();
    let sql = format!(
        "SELECT turns.id, turns.model, turns.resolved_model, turns.options_json, turns.thread_id,
                turns.duration_ms, turns.datetime_utc, turns.input_tokens, turns.output_tokens,
                turns.token_details, turns.schema_id,
                (SELECT prompt FROM turn_search WHERE turn_id = turns.id),
                (SELECT response FROM turn_search WHERE turn_id = turns.id),
                (SELECT name FROM threads WHERE id = turns.thread_id),
                {rank_sql} AS rank_value
         FROM {from} {where_sql}
         ORDER BY {rank_sql}, turns.id DESC
         {limit_sql}"
    );
    if let Ok(mut stmt) = db.conn().prepare(&sql) {
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        match stmt.query_map(refs.as_slice(), |r| {
            let rank: f64 = r.get(14).unwrap_or(0.0);
            Ok((rank, new_store_row(r)))
        }) {
            Ok(mapped) => {
                for (rank, row) in mapped.flatten() {
                    rows.push((rank, row));
                }
            }
            Err(err) => {
                eprintln!(
                    "Error: Invalid search query: {err} - see the FTS5 query syntax documentation at https://sqlite.org/fts5.html#full_text_query_syntax"
                );
                std::process::exit(1);
            }
        }
    } else if let Err(e) = db.conn().prepare(&sql) {
        eprintln!("Warning: cannot query turns: {e}");
    }

    // newest-first unless relevance search already ordered inside each store
    if !f.search {
        rows.sort_by(|a, b| {
            let id_a = a.1["id"].as_str().unwrap_or("");
            let id_b = b.1["id"].as_str().unwrap_or("");
            id_b.cmp(id_a)
        });
    } else {
        rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    }
    rows.into_iter().map(|(_, row)| row).collect()
}

macro_rules! get_str {
    ($r:expr, $i:literal) => {
        $r.get::<_, Option<String>>($i)
            .ok()
            .flatten()
            .unwrap_or_default()
    };
}

fn new_store_row(r: &rusqlite::Row<'_>) -> Value {
    let id: String = get_str!(r, 0);
    let model: String = get_str!(r, 1);
    let resolved: String = get_str!(r, 2);
    let options_json: Option<String> = r.get(3).unwrap_or(None);
    let thread_id: String = get_str!(r, 4);
    let duration_ms: Option<i64> = r.get(5).unwrap_or(None);
    let datetime_utc: String = get_str!(r, 6);
    let input_tokens: Option<i64> = r.get(7).unwrap_or(None);
    let output_tokens: Option<i64> = r.get(8).unwrap_or(None);
    let token_details: Option<String> = r.get(9).unwrap_or(None);
    let schema_id: Option<String> = r.get(10).unwrap_or(None);
    let prompt: String = get_str!(r, 11);
    let response: String = get_str!(r, 12);
    let conversation_name: Option<String> = r.get(13).unwrap_or(None);
    json!({
        "id": id,
        "model": model,
        "resolved_model": if resolved.is_empty() { model.clone() } else { resolved },
        "options_json": options_json
            .and_then(|o| serde_json::from_str::<Value>(&o).ok())
            .unwrap_or_else(|| json!({})),
        "conversation_id": thread_id,
        "duration_ms": duration_ms,
        "datetime_utc": datetime_utc,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "token_details": token_details
            .and_then(|t| serde_json::from_str::<Value>(&t).ok()),
        "conversation_name": conversation_name,
        "conversation_model": model,
        "schema_id": schema_id,
        "prompt": prompt,
        "response": response,
        "_store": "turns",
    })
}

/// Stored schema content by id, shared by schema resolution and row annotation.
pub fn schema_content(db: &Db, id: &str) -> Option<String> {
    db.conn()
        .query_row(
            "SELECT content FROM schemas WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .ok()
}

/// Row counts for `llm logs status`.
pub struct StoreCounts {
    pub threads: i64,
    pub turns: i64,
}

pub fn store_counts(db: &Db) -> StoreCounts {
    let count = |sql: &str| db.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0);
    StoreCounts {
        threads: count("SELECT count(*) FROM threads"),
        turns: count("SELECT count(*) FROM turns"),
    }
}

/// Fill in the fields that need extra queries: schema json, fragments,
/// attachments, system/reasoning for new-store rows.
pub fn annotate(db: &Db, rows: Vec<Value>, truncate: bool) -> Vec<Value> {
    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        let id = row["id"].as_str().unwrap_or_default().to_string();

        // schema json
        if let Some(schema_id) = row["schema_id"].as_str()
            && let Some(content) = schema_content(db, schema_id)
        {
            row["schema_json"] = serde_json::from_str::<Value>(&content).unwrap_or(Value::Null);
        }

        // parent + tip in one query; the system prompt is the newest
        // system message on the input chain, reasoning sits on the tip
        // message itself — no full-chain walk needed for either
        let (parent, tip): (Option<String>, Option<String>) = db
            .conn()
            .query_row(
                "SELECT parent_message_hash, tip_message_hash FROM turns WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((None, None));
        let mut system = Value::Null;
        let mut reasoning = Value::Null;
        let mut attachments: Vec<Value> = Vec::new();
        if let Some(parent) = &parent {
            for (hash, role) in chain_with_roles(db.conn(), parent) {
                if role.as_deref() == Some("system") {
                    if let Some((_, parts)) = message_parts(db.conn(), &hash) {
                        system = json!(parts_text(&parts, None));
                    }
                    break;
                }
            }
            attachments = chain_attachments(db.conn(), parent);
        }
        if let Some(tip) = &tip
            && let Some((_, parts)) = message_parts(db.conn(), tip)
        {
            let text = parts_text(&parts, Some("reasoning"));
            reasoning = if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            };
        }
        row["system"] = system;
        row["reasoning"] = reasoning;
        row["prompt_json"] = Value::Null;
        row["response_json"] = Value::Null;
        row["attachments"] = Value::Array(attachments);
        row["tools"] = json!([]);
        row["tool_calls"] = json!([]);
        row["tool_results"] = json!([]);

        // -t: truncate long strings and drop *_json keys (mirrors annotate_log_rows)
        if truncate {
            for key in ["prompt", "response", "system", "reasoning"] {
                if let Some(text) = row[key].as_str() {
                    row[key] = json!(truncate_string(text, 100));
                }
            }
            for key in [
                "options_json",
                "schema_json",
                "response_json",
                "token_details",
            ] {
                row[key] = Value::Null;
            }
        }
        if let Some(m) = row.as_object_mut() {
            m.remove("schema_id");
            m.remove("_store");
        }
        out.push(row);
    }
    out
}

/// Attachments on a message chain: part_attachments → attachments rows, in
/// ONE query over the whole chain instead of one per message.
fn chain_attachments(conn: &Connection, tip: &str) -> Vec<Value> {
    let mut stmt = match conn.prepare(
        "WITH RECURSIVE chain(hash) AS (
             SELECT ?1
             UNION ALL
             SELECT m.parent_hash FROM messages m JOIN chain c ON m.hash = c.hash
              WHERE m.parent_hash IS NOT NULL
         )
         SELECT a.id, a.type, a.path, a.url, length(a.content)
           FROM chain c
           JOIN parts p ON p.message_hash = c.hash
           JOIN part_attachments pa ON pa.part_id = p.id
           JOIN attachments a ON a.id = pa.attachment_id
          ORDER BY p.id, pa.\"order\"",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: cannot read chain attachments: {e}");
            return Vec::new();
        }
    };
    match stmt.query_map(params![tip], |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "type": r.get::<_, Option<String>>(1)?,
            "path": r.get::<_, Option<String>>(2)?,
            "url": r.get::<_, Option<String>>(3)?,
            "content": r.get::<_, Option<i64>>(4)?.map(|n| n > 0).unwrap_or(false),
            "content_length": r.get::<_, Option<i64>>(4)?,
        }))
    }) {
        Ok(rows) => rows
            .filter_map(|r| {
                r.map_err(|e| eprintln!("Warning: unreadable attachment row: {e}"))
                    .ok()
            })
            .collect(),
        Err(e) => {
            eprintln!("Warning: cannot read chain attachments: {e}");
            Vec::new()
        }
    }
}

pub(crate) fn truncate_string(text: &str, max: usize) -> String {
    crate::core::text::truncate_chars(text, max)
}

/// Rows for specific ids, serialized as the `llm logs --json` shape —
/// used by `llm prompt --json`.
pub fn rows_for_ids_json(db: Option<&Db>, ids: &[String]) -> String {
    let Some(db) = db else {
        return "[]".to_string();
    };
    let ids: Vec<&String> = ids.iter().filter(|i| !i.is_empty()).collect();
    if ids.is_empty() {
        return "[]".to_string();
    }
    let placeholders: Vec<String> = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect();
    let params: Vec<&dyn rusqlite::types::ToSql> = ids
        .iter()
        .map(|i| *i as &dyn rusqlite::types::ToSql)
        .collect();
    // newest-first, then reversed for chronological output
    let mut rows: Vec<Value> = Vec::new();
    let sql = format!(
        "SELECT turns.id, turns.model, turns.resolved_model, turns.options_json, turns.thread_id,
                turns.duration_ms, turns.datetime_utc, turns.input_tokens, turns.output_tokens,
                turns.token_details, turns.schema_id,
                (SELECT prompt FROM turn_search WHERE turn_id = turns.id),
                (SELECT response FROM turn_search WHERE turn_id = turns.id),
                (SELECT name FROM threads WHERE id = turns.thread_id)
         FROM turns WHERE turns.id IN ({}) ORDER BY turns.id DESC",
        placeholders.join(",")
    );
    if let Ok(mut stmt) = db.conn().prepare(&sql)
        && let Ok(mapped) = stmt.query_map(params.as_slice(), |r| Ok(new_store_row(r)))
    {
        rows.extend(mapped.flatten());
    } else if let Err(e) = db.conn().prepare(&sql) {
        eprintln!("Warning: cannot read turns by id: {e}");
    }
    rows.reverse();
    let annotated = annotate(db, rows, false);
    jsonfmt::dumps_indent(&Value::Array(annotated), 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::Db;

    #[test]
    fn forked_thread_shares_the_tip_but_diverges() {
        let db = Db::open_memory().unwrap();
        log_completed_turn(
            &db,
            &CompletedTurn {
                conversation_id: None,
                system: None,
                input_messages: &[Message::text("user", "hello")],
                reasoning: None,
                response_text: "hi",
                model: "m/x",
                options: &[],
                schema: None,
                usage: Some((1, 1)),
                duration_ms: 10,
            },
        );
        let cid = latest_conversation_id(&db).unwrap();
        let fork = fork_thread(&db, &cid).unwrap();
        assert_ne!(fork, cid);
        // both point at the same chain tip right after forking
        assert_eq!(thread_tip(&db, &fork), thread_tip(&db, &cid));
        // a new turn on the fork leaves the original tip untouched
        log_completed_turn(
            &db,
            &CompletedTurn {
                conversation_id: Some(&fork),
                system: None,
                input_messages: &[Message::text("user", "more")],
                reasoning: None,
                response_text: "ok",
                model: "m/x",
                options: &[],
                schema: None,
                usage: None,
                duration_ms: 5,
            },
        );
        let chain = thread_chain(&db, &fork);
        let texts: Vec<&str> = chain
            .iter()
            .flat_map(|(_, parts)| parts.iter().filter_map(|p| p.text.as_deref()))
            .collect();
        assert!(texts.contains(&"hello"));
        assert!(texts.contains(&"more"));
        assert!(thread_chain(&db, &cid).len() == 2); // system-less: user+assistant
    }

    #[test]
    fn agent_tool_turn_prompt_lands_in_turn_search() {
        // an agent task with tool rounds: the input chain ends in a tool
        // message, so the prompt must be found by walking the chain, not by
        // looking at the turn's parent message
        let db = Db::open_memory().unwrap();
        let input = vec![
            Message::text("system", "be brief"),
            Message::text("user", "检查这些包"),
            Message {
                role: "assistant".to_string(),
                parts: vec![Part::ToolCall {
                    id: "c1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({"command": "ls"}),
                }],
            },
            Message {
                role: "tool".to_string(),
                parts: vec![Part::ToolResult {
                    call_id: "c1".to_string(),
                    name: "bash".to_string(),
                    content: "a\nb".to_string(),
                    is_error: false,
                }],
            },
        ];
        let options = serde_json::Map::new();
        log_turn(
            &db,
            &TurnToLog {
                thread_id: None,
                history_tip: None,
                input_messages: &input,
                reasoning: None,
                response_text: "共 2 个文件",
                model: "prov/model",
                options: &options,
                schema: None,
                usage: None,
                duration_ms: None,
            },
        );
        let (prompt, response): (String, String) = db
            .conn()
            .query_row("SELECT prompt, response FROM turn_search", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(prompt, "检查这些包");
        assert_eq!(response, "共 2 个文件");
        // the /resume preview reads the same value
        let thread_id: String = db
            .conn()
            .query_row("SELECT thread_id FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(thread_first_prompt(&db, &thread_id), "检查这些包");
    }

    #[test]
    fn write_and_read_back() {
        let db = Db::open_memory().unwrap();
        let input = vec![
            Message::text("system", "be brief"),
            Message::text("user", "hello there"),
        ];
        let options = serde_json::Map::new();
        let turn_id = log_turn(
            &db,
            &TurnToLog {
                thread_id: None,
                history_tip: None,
                input_messages: &input,
                reasoning: Some("thinking..."),
                response_text: "hi!",
                model: "prov/model",
                options: &options,
                schema: None,
                usage: Some((3, 5)),
                duration_ms: Some(120),
            },
        );
        assert_eq!(turn_id.len(), 26);

        // thread was created and named from the user prompt
        let name: String = db
            .conn()
            .query_row("SELECT name FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "hello there");

        // history round-trips
        let thread_id: String = db
            .conn()
            .query_row(
                "SELECT thread_id FROM turns WHERE id = ?1",
                params![turn_id],
                |r| r.get(0),
            )
            .unwrap();
        let history = conversation_history(&db, &thread_id);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].0, "hello there");
        assert_eq!(history[0].1, "hi!");
        assert_eq!(history[0].2.as_deref(), Some("be brief"));
        assert_eq!(history[0].3, "prov/model");

        // turn_search got populated for full-text search
        let (prompt, response): (String, String) = db
            .conn()
            .query_row("SELECT prompt, response FROM turn_search", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(prompt, "hello there");
        assert_eq!(response, "hi!");

        // a second turn on the same thread appends to the stored chain tip,
        // reusing the recorded messages verbatim (reasoning parts included)
        let tip = thread_tip(&db, &thread_id).expect("thread has a tip");
        let next_input = vec![Message::text("user", "and again")];
        log_turn(
            &db,
            &TurnToLog {
                thread_id: Some(&thread_id),
                history_tip: Some(&tip),
                input_messages: &next_input,
                reasoning: None,
                response_text: "ok",
                model: "prov/model",
                options: &options,
                schema: None,
                usage: None,
                duration_ms: None,
            },
        );
        let turns: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(turns, 2);
        let history2 = conversation_history(&db, &thread_id);
        assert_eq!(history2.len(), 2);
        assert_eq!(history2[1].0, "and again");

        // /resume preview reads the first prompt back through turn_search
        assert_eq!(thread_first_prompt(&db, &thread_id), "hello there");

        // dedup: shared prefix messages are stored once
        let messages: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        // system + user1 + assistant1(with reasoning) + user2 + assistant2 = 5
        assert_eq!(messages, 5);
    }

    #[test]
    fn attachment_hashing() {
        let a = StoredAttachment {
            path: None,
            url: None,
            mime_type: Some("image/png".into()),
            content: vec![1, 2, 3, 4],
        };
        let canonical = canonical_attachment(&a);
        assert_eq!(canonical["id"], json!(sha256_hex(&[1, 2, 3, 4])));
        assert_eq!(canonical["type"], json!("image/png"));
    }

    #[test]
    fn message_hash_is_deterministic_and_parent_sensitive() {
        let m = Message::text("user", "hi");
        let h1 = message_hash(&m, None);
        let h2 = message_hash(&m, None);
        let h3 = message_hash(&m, Some("b2:abc"));
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert!(h1.starts_with("b2:"));
        assert_eq!(h1.len(), 3 + 32);
    }
}
