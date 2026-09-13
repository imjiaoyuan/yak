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
pub(super) fn lossy_lines<'a>(
    reader: &'a mut impl BufRead,
    buf: &'a mut Vec<u8>,
) -> impl Iterator<Item = String> + 'a {
    std::iter::from_fn(move || {
        buf.clear();
        match reader.read_until(b'\n', buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                Some(String::from_utf8_lossy(buf).into_owned())
            }
        }
    })
}

pub(super) fn reader_loop(
    stdout: impl std::io::Read,
    pending: &PendingMap,
    dead: &AtomicBool,
    tail: &Mutex<VecDeque<String>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    for line in lossy_lines(&mut reader, &mut buf) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            let mut t = lock(tail);
            if t.len() >= TAIL_LINES {
                t.pop_front();
            }
            t.push_back(line);
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            let mut t = lock(tail);
            if t.len() >= TAIL_LINES {
                t.pop_front();
            }
            t.push_back(line);
            continue;
        };
        let sender = lock(pending).remove(&id);
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
    for (_, tx) in lock(pending).drain() {
        let _ = tx.send(Err("extension closed its stdout".to_string()));
    }
}

// ============================================================================
// the Tool wrapper
// ============================================================================
