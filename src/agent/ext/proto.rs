use super::*;

pub(super) fn writer_loop(mut stdin: ChildStdin, rx: Receiver<String>) {
    while let Ok(line) = rx.recv() {
        if stdin.write_all(line.as_bytes()).is_err() {
            return;
        }
        let _ = stdin.flush();
    }
}

/// Read reply lines, correlate by id, dim non-matching lines into the tail.
/// Decode a newline-delimited reader line by line, lossily: one byte that is
/// not valid UTF-8 must not end the stream. A legacy-codepage byte on stderr
/// (a Windows console writing cp1252) would otherwise cost every later
/// diagnostic, and a garbled line on stdout would cost the replies with it.
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const OVERSIZED_FRAME: &str = "[oversized extension frame discarded]";

pub(super) fn lossy_lines<'a>(
    reader: &'a mut impl BufRead,
    buf: &'a mut Vec<u8>,
) -> impl Iterator<Item = String> + 'a {
    std::iter::from_fn(move || {
        buf.clear();
        let mut oversized = false;
        loop {
            let available = reader.fill_buf().ok()?;
            if available.is_empty() {
                return if buf.is_empty() && !oversized {
                    None
                } else {
                    Some(if oversized {
                        OVERSIZED_FRAME.to_string()
                    } else {
                        String::from_utf8_lossy(buf).into_owned()
                    })
                };
            }
            let newline = available.iter().position(|b| *b == b'\n');
            let take = newline.map_or(available.len(), |i| i + 1);
            if !oversized {
                let room = MAX_FRAME_BYTES.saturating_sub(buf.len());
                buf.extend_from_slice(&available[..take.min(room)]);
                if take > room {
                    oversized = true;
                }
            }
            reader.consume(take);
            if newline.is_some() {
                if oversized {
                    return Some(OVERSIZED_FRAME.to_string());
                }
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                return Some(String::from_utf8_lossy(buf).into_owned());
            }
        }
    })
}

pub(super) fn reader_loop(
    stdout: impl std::io::Read,
    pending: &PendingMap,
    dead: &AtomicBool,
    tail: &Mutex<VecDeque<String>>,
    lanes: &Mutex<VecDeque<(String, String)>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    let mut oversized = false;
    for line in lossy_lines(&mut reader, &mut buf) {
        if line == OVERSIZED_FRAME {
            // An oversized reply may belong to an in-flight tool. Reject the
            // connection instead of waiting out its timeout or running it twice.
            oversized = true;
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            let mut t = crate::core::sync::lock(tail);
            if t.len() >= TAIL_LINES {
                t.pop_front();
            }
            t.push_back(line);
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            // an id-less but well-formed frame: a lane status update an
            // extension streams while a call is in flight (see the subagent
            // example). Anything else is protocol noise → the tail.
            if value.get("type").and_then(Value::as_str) == Some("lane") {
                let (Some(lane), Some(text)) = (
                    value.get("lane").and_then(Value::as_str),
                    value.get("text").and_then(Value::as_str),
                ) else {
                    let mut t = crate::core::sync::lock(tail);
                    if t.len() >= TAIL_LINES {
                        t.pop_front();
                    }
                    t.push_back(line);
                    continue;
                };
                let mut q = crate::core::sync::lock(lanes);
                if q.len() >= PROGRESS_LINES {
                    q.pop_front();
                }
                q.push_back((lane.to_string(), text.to_string()));
            } else {
                let mut t = crate::core::sync::lock(tail);
                if t.len() >= TAIL_LINES {
                    t.pop_front();
                }
                t.push_back(line);
            }
            continue;
        };
        let sender = crate::core::sync::lock(pending).remove(&id);
        if let Some(tx) = sender {
            if value.get("error").is_some() {
                let _ = tx.send(Err(value["error"]
                    .as_str()
                    .unwrap_or("extension error")
                    .to_string()));
            } else {
                let _ = tx.send(Ok(value));
            }
        }
    }
    // the process is gone: fail everything still waiting (the pending map
    // holds a sender clone, so waiters alone would never see a disconnect)
    dead.store(true, Ordering::Relaxed);
    for (_, tx) in crate::core::sync::lock(pending).drain() {
        let _ = tx.send(Err(if oversized {
            "extension stdout frame exceeded 1 MiB".to_string()
        } else {
            "extension closed its stdout".to_string()
        }));
    }
}
