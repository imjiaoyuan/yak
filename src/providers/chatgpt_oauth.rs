//! ChatGPT subscription login (Sign in with ChatGPT, the Codex channel):
//! an OAuth 2.0 PKCE public-client flow, no client secret. Login talks to
//! auth.openai.com; the resulting access token rides as a Bearer to
//! chatgpt.com/backend-api/codex/responses (the Responses API) and bills
//! the Plus/Pro subscription.
//!
//! Storage is `<user_dir>/auth.json`, one entry per provider: access /
//! refresh / expiry. Tokens refresh before they expire; a refresh that
//! fails says so instead of silently degrading.

use serde_json::Value;

use crate::core::sha256;

/// The fixed public client id (the Codex CLI's, on OpenAI's SPA allow-list).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
/// The JWT claim path holding the account id (the `chatgpt-account-id`
/// request header needs it).
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
/// Refresh this far ahead of the real expiry: a request never starts with a
/// token about to die.
const EXPIRY_MARGIN_SECS: u64 = 180;

/// One saved OAuth credential (an auth.json entry).
#[derive(Clone, Debug)]
pub struct Token {
    pub access: String,
    pub refresh: String,
    /// Unix epoch seconds, already carrying the early-refresh margin
    pub expires: u64,
}

/// `~/.yak/auth.json`: one JSON object, provider name → token. 0600, the
/// same JSON formatting config.json uses.
fn auth_path() -> std::path::PathBuf {
    crate::core::config::user_dir().join("auth.json")
}

pub fn read_auth() -> std::collections::BTreeMap<String, Token> {
    let text = match std::fs::read_to_string(auth_path()) {
        Ok(t) => t,
        Err(_) => return std::collections::BTreeMap::new(),
    };
    let root: Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        eprintln!("Warning: cannot parse {}: {e}", auth_path().display());
        Value::Null
    });
    let Some(map) = root.as_object() else {
        return std::collections::BTreeMap::new();
    };
    let mut out = std::collections::BTreeMap::new();
    for (name, v) in map {
        let Some(access) = v["access"].as_str() else {
            continue;
        };
        let Some(refresh) = v["refresh"].as_str() else {
            continue;
        };
        let expires = v["expires"].as_u64().unwrap_or(0);
        out.insert(
            name.clone(),
            Token {
                access: access.to_string(),
                refresh: refresh.to_string(),
                expires,
            },
        );
    }
    out
}

pub fn write_token(name: &str, token: &Token) -> Result<(), String> {
    let path = auth_path();
    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("{} does not parse: {e}", path.display()))?,
        Err(_) => Value::Object(Default::default()),
    };
    let Some(map) = root.as_object_mut() else {
        return Err(format!("{} is not an auth table", path.display()));
    };
    map.insert(
        name.to_string(),
        serde_json::json!({
            "access": token.access,
            "refresh": token.refresh,
            "expires": token.expires,
        }),
    );
    let body = crate::jsonfmt::dumps_indent(&root, 2) + "\n";
    crate::core::fsx::write_atomic(&path, body.as_bytes(), Some(0o600))
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn remove_token(name: &str) -> Result<(), String> {
    let path = auth_path();
    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("{} does not parse: {e}", path.display()))?,
        Err(_) => return Ok(()),
    };
    if let Some(map) = root.as_object_mut() {
        map.remove(name);
        let body = crate::jsonfmt::dumps_indent(&root, 2) + "\n";
        crate::core::fsx::write_atomic(&path, body.as_bytes(), Some(0o600))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A random string (n bytes → unpadded base64url), off the same OS source
/// the ULID randomness uses.
fn random_b64url(n: usize) -> String {
    let mut buf = vec![0u8; n];
    crate::core::db::fill_random(&mut buf);
    b64url(&buf)
}

/// base64url, no padding.
fn b64url(data: &[u8]) -> String {
    crate::b64::encode(data)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// A PKCE pair: the verifier (random) and the challenge
/// (base64url(SHA-256(verifier))).
fn pkce() -> (String, String) {
    let verifier = random_b64url(64);
    let challenge = b64url(&sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// Decode a JWT payload without verifying (the token was issued to us;
/// reading our own account id is not a trust decision).
fn jwt_payload(token: &str) -> Option<Value> {
    let part = token.split('.').nth(1)?;
    let std = part.replace('-', "+").replace('_', "/");
    let padded = match std.len() % 4 {
        2 => std + "==",
        3 => std + "=",
        _ => std,
    };
    let bytes = crate::b64::decode(&padded)?;
    serde_json::from_slice(&bytes).ok()
}

/// The chatgpt_account_id inside an access token (one header per request).
pub fn account_id(access: &str) -> Option<String> {
    let payload = jwt_payload(access)?;
    let id = payload[JWT_AUTH_CLAIM]["chatgpt_account_id"].as_str()?;
    Some(id.to_string())
}

/// One login attempt. The caller opens the URL and feeds the callback.
pub struct LoginFlow {
    pub url: String,
    verifier: String,
    state: String,
}

pub fn start_login() -> Result<LoginFlow, String> {
    let (verifier, challenge) = pkce();
    let state = random_b64url(32);
    let mut url = format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={CLIENT_ID}\
         &redirect_uri={REDIRECT_URI}&scope={SCOPE}\
         &code_challenge={challenge}&code_challenge_method=S256&state={state}\
         &id_token_add_organizations=true&codex_cli_simplified_flow=true&originator=yak"
    );
    url = url.replace(' ', "%20");
    Ok(LoginFlow {
        url,
        verifier,
        state,
    })
}

impl LoginFlow {
    /// Validate the callback URL (state match, a code present) and exchange
    /// it for tokens.
    pub fn complete(self, callback_url: &str) -> Result<Token, String> {
        let query = callback_url
            .split_once('?')
            .map(|(_, q)| q)
            .ok_or("the callback must be a URL with a query string")?;
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').ok_or("malformed callback query")?;
            let v = v.replace("%3A", ":").replace("%2F", "/");
            match k {
                "code" => code = Some(v),
                "state" => state = Some(v),
                "error" => error = Some(v),
                _ => {}
            }
        }
        if let Some(err) = error {
            return Err(format!("authorization failed: {err}"));
        }
        if state.as_deref() != Some(self.state.as_str()) {
            return Err("state mismatch — not your login callback".into());
        }
        let code = code.ok_or("callback carried no authorization code")?;
        exchange_code(&code, &self.verifier)
    }
}

/// POST a form to the token endpoint.
fn token_post(form: &str) -> Result<Value, String> {
    let agent = crate::core::http::short_agent();
    let resp = agent
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .send(form)
        .map_err(|e| format!("token endpoint unreachable: {e}"))?;
    let status = resp.status().as_u16();
    let mut body = String::new();
    std::io::Read::read_to_string(&mut resp.into_body().into_reader(), &mut body)
        .map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&body)
        .map_err(|e| format!("token response not JSON ({status}): {e}"))?;
    if status >= 400 {
        let msg = value["error"].as_str().unwrap_or(&body).to_string();
        return Err(format!("token exchange failed (HTTP {status}): {msg}"));
    }
    Ok(value)
}

fn token_from_response(v: &Value) -> Result<Token, String> {
    let access = v["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("token response missing access_token")?
        .to_string();
    let refresh = v["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("token response missing refresh_token")?
        .to_string();
    let expires_in = v["expires_in"].as_u64().ok_or("no expires_in")?;
    Ok(Token {
        access,
        refresh,
        expires: now_secs() + expires_in.saturating_sub(EXPIRY_MARGIN_SECS),
    })
}

fn exchange_code(code: &str, verifier: &str) -> Result<Token, String> {
    let form = format!(
        "grant_type=authorization_code&client_id={CLIENT_ID}\
         &code={code}&code_verifier={verifier}&redirect_uri={REDIRECT_URI}"
    );
    token_from_response(&token_post(&form)?)
}

/// Refresh with the (rotating) refresh token. A failure stays a failure: no
/// silent downgrade to an empty key — say it here, where the fix (login
/// again) is the message, instead of surfacing as an opaque 401 later.
pub fn refresh(name: &str, refresh_token: &str) -> Result<Token, String> {
    let form =
        format!("grant_type=refresh_token&client_id={CLIENT_ID}&refresh_token={refresh_token}");
    let token = token_from_response(&token_post(&form)?)
        .map_err(|e| format!("refresh failed for '{name}': {e} — run /login again"))?;
    write_token(name, &token)?;
    Ok(token)
}

/// The provider's current access token: fresh ones return as-is, expired
/// ones refresh in place and write back to auth.json.
pub fn access_token(name: &str) -> Result<String, String> {
    let tokens = read_auth();
    let t = tokens
        .get(name)
        .ok_or_else(|| format!("no saved ChatGPT login for '{name}' — run /login"))?;
    if t.expires > now_secs() {
        return Ok(t.access.clone());
    }
    let refreshed = refresh(name, &t.refresh)?;
    Ok(refreshed.access)
}

/// Complete a login from a pasted callback URL (headless / remote boxes):
/// `/login` lands here.
pub fn manual_complete(flow: LoginFlow, pasted: &str) -> Result<Token, String> {
    flow.complete(pasted.trim())
}

/// The auth.json path, for messages.
pub fn auth_file_display() -> std::path::PathBuf {
    auth_path()
}

/// The login callback server: listen on 127.0.0.1:1455/auth/callback and
/// send each full callback URL (query included) down the channel. The HTTP
/// parse is handwritten — one GET line is not worth an HTTP server crate.
/// Returns the receiver and the bound port.
pub fn spawn_callback_server() -> Result<(std::sync::mpsc::Receiver<String>, u16), String> {
    use std::io::{BufRead, BufReader};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 1455)).map_err(|e| {
        format!("cannot bind port 1455 for the login callback ({e}) — another login running?")
    })?;
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(1455);
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or("");
            let url = format!("http://127.0.0.1:{port}{path}");
            let _ = std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<html><body style=\"font-family: sans-serif; text-align: center; padding-top: 3em\"><h2>Sign-in complete.</h2><p>You can close this tab and go back to the terminal.</p></body></html>\n",
            );
            if tx.send(url).is_err() {
                return;
            }
        }
    });
    Ok((rx, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let (v, c) = pkce();
        assert_eq!(c, b64url(&sha256::digest(v.as_bytes())));
        assert!(v.len() >= 43 && v.len() <= 128);
    }

    #[test]
    fn jwt_payload_decodes() {
        // header.payload.sig, the payload being base64url of {"a":1}
        let jwt = "x.eyJhIjoxfQ.z";
        assert_eq!(jwt_payload(jwt).unwrap()["a"], 1);
    }

    #[test]
    fn account_id_reads_the_auth_claim() {
        let claims = serde_json::json!({
            JWT_AUTH_CLAIM: {"chatgpt_account_id": "acct-1"}
        });
        let payload = crate::b64::encode(claims.to_string().as_bytes());
        let jwt = format!("x.{payload}.z");
        assert_eq!(account_id(&jwt).unwrap(), "acct-1");
    }

    #[test]
    fn callback_state_mismatch_refuses() {
        let flow = start_login().unwrap();
        let err = flow.complete("http://localhost:1455/auth/callback?code=x&state=other");
        assert!(err.unwrap_err().contains("state mismatch"));
    }
}
