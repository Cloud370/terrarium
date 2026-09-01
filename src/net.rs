//! Network fetch: a zero-consent, journaled HTTP client. A fetch response enters cage
//! memory only — reaching local disk requires the already-authorized write path, so the
//! local-mutation loop is closed by construction. The egress loop is *not* closed:
//! anything the operating-system user can read can be sent anywhere, and the journal
//! (`net/request` receipts) detects this after the fact rather than preventing it.
//! Physical limits are host-owned: 60 s per request (covering the response head and
//! its body consumption), an 8 MiB response-body cap, at most 4 concurrent requests,
//! an 8 KiB request-URL cap, and `--offline` disables the capability for the whole
//! invocation.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rquickjs::function::{Async, Opt};
use rquickjs::{Ctx, Function, Object, Value};
use tokio::sync::{watch, Semaphore};

/// 60 s per request, covering the response and its body consumption.
pub(crate) const REQUEST_BUDGET: Duration = Duration::from_secs(60);
/// The response body is rejected with a visible error past 8 MiB.
pub(crate) const BODY_CAP: u64 = 8 * 1024 * 1024;
/// At most 4 concurrent requests per run.
pub(crate) const MAX_CONCURRENT: usize = 4;
/// A request body is a bounded string; streaming upload is a spawned `curl`.
const REQUEST_BODY_CAP: usize = 1024 * 1024;
/// The request URL is itself bounded so one cage string cannot balloon the journal.
const URL_CAP: usize = 8 * 1024;

const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

struct BodyState {
    response: Arc<tokio::sync::Mutex<Option<reqwest::Response>>>,
    bytes: u64,
    receipt: Option<usize>,
    deadline: Instant,
    done: bool,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

fn js_err(msg: String) -> rquickjs::Error {
    rquickjs::Error::FromJs {
        from: "net",
        to: "result",
        message: Some(msg),
    }
}

fn validate_url(raw: &str) -> Result<url::Url, String> {
    if raw.len() > URL_CAP {
        return Err(format!("the URL exceeds the {URL_CAP}-byte limit"));
    }
    let url = url::Url::parse(raw).map_err(|e| format!("invalid URL {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "only http and https URLs are supported (got {:?})",
            url.scheme()
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with userinfo are rejected as invalid syntax".into());
    }
    if url.fragment().is_some() {
        return Err("URLs with a fragment are rejected as invalid syntax".into());
    }
    Ok(url)
}

fn parse_method(object: &Object<'_>) -> Result<reqwest::Method, String> {
    let method = object
        .get::<_, Option<String>>("method")
        .map_err(|_| "options.method must be a string".to_string())?
        .unwrap_or_else(|| "GET".into())
        .to_uppercase();
    if !METHODS.contains(&method.as_str()) {
        return Err(format!(
            "method must be one of {} (got {method:?})",
            METHODS.join(", ")
        ));
    }
    reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| format!("unsupported method {method:?}"))
}

fn header_value(value: &Value<'_>) -> Result<String, String> {
    if let Some(text) = value.as_string() {
        let text = text
            .to_string()
            .map_err(|_| "header values must be valid UTF-8 strings".to_string())?;
        if text.contains('\r') || text.contains('\n') {
            return Err("header values must not contain CR or LF".into());
        }
        return Ok(text);
    }
    let object = value
        .as_object()
        .ok_or_else(|| "header values must be strings or {env: NAME} references".to_string())?;
    let name = object
        .get::<_, Option<String>>("env")
        .map_err(|_| "an {env: NAME} header reference needs a string NAME".to_string())?
        .ok_or_else(|| "header values must be strings or {env: NAME} references".to_string())?;
    if name.is_empty() {
        return Err("an {env: NAME} header reference needs a non-empty NAME".into());
    }
    // credential values resolve host-side and never enter the cage
    std::env::var(&name).map_err(|_| format!("environment variable {name:?} is not set"))
}

fn parse_headers(object: &Object<'_>) -> Result<Vec<(String, String)>, String> {
    let headers = object
        .get::<_, Option<Object>>("headers")
        .map_err(|_| "options.headers must be an object of name to value".to_string())?;
    let Some(headers) = headers else {
        return Ok(Vec::new());
    };
    let mut parsed = Vec::new();
    for prop in headers.props::<String, Value>() {
        let (name, value) = prop.map_err(|e| format!("cannot read headers: {e}"))?;
        if name.is_empty() || name.contains(':') || name.contains('\r') || name.contains('\n') {
            return Err(format!(
                "header name {name:?} is empty or contains ':', CR, or LF"
            ));
        }
        parsed.push((name, header_value(&value)?));
    }
    Ok(parsed)
}

fn parse_body(object: &Object<'_>) -> Result<Option<String>, String> {
    let body = object
        .get::<_, Option<String>>("body")
        .map_err(|_| "options.body must be a string".to_string())?;
    match body {
        Some(body) if body.len() > REQUEST_BODY_CAP => Err(format!(
            "request body exceeds the {REQUEST_BODY_CAP}-byte limit; streaming upload is a \
             non-goal — spawn a declared curl for large payloads"
        )),
        other => Ok(other),
    }
}

async fn read_chunk(state: &Rc<RefCell<BodyState>>) -> Result<Option<Vec<u8>>, String> {
    let response = state.borrow().response.clone();
    let mut guard = response.lock().await;
    match guard.as_mut() {
        Some(response) => match response.chunk().await {
            Ok(Some(bytes)) => Ok(Some(bytes.to_vec())),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("failed to read response body: {e}")),
        },
        None => Ok(None),
    }
}

fn body_next<'a>(
    state: &'a Rc<RefCell<BodyState>>,
    receipts: &'a Rc<RefCell<Vec<serde_json::Value>>>,
    cancel: &'a mut watch::Receiver<bool>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, rquickjs::Error>> + 'a>>
{
    Box::pin(async move {
        let deadline = state.borrow().deadline;
        if state.borrow().done {
            return Ok(Vec::new());
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(js_err(
                "the 60-second request budget expired while reading the response body".into(),
            ));
        };
        let chunk = tokio::select! {
            chunk = tokio::time::timeout(remaining, read_chunk(state)) => {
                match chunk {
                    Err(_) => {
                        return Err(js_err(
                            "the 60-second request budget expired while reading the response \
                             body"
                                .into(),
                        ));
                    }
                    Ok(Err(e)) => return Err(js_err(e)),
                    Ok(Ok(chunk)) => chunk,
                }
            }
            _ = crate::kernel::cancelled(cancel) => return Ok(Vec::new()),
        };
        let Some(bytes) = chunk else {
            state.borrow_mut().done = true;
            return Ok(Vec::new());
        };
        {
            let mut st = state.borrow_mut();
            st.bytes = st.bytes.saturating_add(bytes.len() as u64);
            if st.bytes > BODY_CAP {
                return Err(js_err(format!(
                    "response body exceeds the {BODY_CAP}-byte limit"
                )));
            }
            if let Some(index) = st.receipt {
                if let Some(receipt) = receipts.borrow_mut().get_mut(index) {
                    receipt["bytes"] = serde_json::json!(st.bytes);
                }
            }
        }
        Ok(vec![String::from_utf8_lossy(&bytes).into_owned()])
    })
}

fn build_fetch_result<'js>(
    ctx: &Ctx<'js>,
    status: u16,
    final_url: &str,
    state: Rc<RefCell<BodyState>>,
    receipts: &Rc<RefCell<Vec<serde_json::Value>>>,
    cancel: &watch::Receiver<bool>,
) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("status", status)?;
    obj.set("finalUrl", final_url)?;
    let iter_state = state;
    let iter_receipts = receipts.clone();
    let iter_cancel = cancel.clone();
    let next_fn = Function::new(
        ctx.clone(),
        Async(move || {
            let state = iter_state.clone();
            let receipts = iter_receipts.clone();
            let mut cancel = iter_cancel.clone();
            async move { body_next(&state, &receipts, &mut cancel).await }
        }),
    )?;
    let iter = Object::new(ctx.clone())?;
    iter.set("next", next_fn)?;
    obj.set("body", iter)?;
    Ok(obj)
}

/// Registers the host.net namespace. Every completed request is journaled as a
/// `net/request` receipt with method, final URL, status, and byte count.
pub(crate) fn install<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    offline: bool,
    receipts: &Rc<RefCell<Vec<serde_json::Value>>>,
    cancel: &watch::Receiver<bool>,
) -> rquickjs::Result<()> {
    let obj = Object::new(ctx.clone())?;
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let fetch_receipts = receipts.clone();
    let fetch_cancel = cancel.clone();
    let fetch_fn = Function::new(
        ctx.clone(),
        Async(move |ctx: Ctx<'js>, url: String, opts: Opt<Object<'js>>| {
            let receipts = fetch_receipts.clone();
            let cancel = fetch_cancel.clone();
            let permits = permits.clone();
            async move {
                if offline {
                    return Err(js_err(
                        "network access is disabled for this invocation (--offline)".into(),
                    ));
                }
                run_fetch(&ctx, &url, &opts, &receipts, &cancel, &permits)
                    .await
                    .map_err(js_err)
            }
        }),
    )?;
    obj.set("fetch", fetch_fn)?;
    host.set("net", obj)
}

/// A request whose bytes may have left the machine is an egress event even when no
/// response arrives: journal it with status 0 so the audit trail stays complete.
fn push_failed_request(
    receipts: &Rc<RefCell<Vec<serde_json::Value>>>,
    method: &reqwest::Method,
    url: &str,
) {
    receipts.borrow_mut().push(serde_json::json!({
        "method": method.as_str(),
        "url": url,
        "status": 0,
        "bytes": 0,
    }));
}

async fn run_fetch<'js>(
    ctx: &Ctx<'js>,
    url: &str,
    opts: &Opt<Object<'js>>,
    receipts: &Rc<RefCell<Vec<serde_json::Value>>>,
    cancel: &watch::Receiver<bool>,
    permits: &Arc<Semaphore>,
) -> Result<Object<'js>, String> {
    let url = validate_url(url)?;
    let (method, headers, body) = match opts.0.as_ref() {
        None => (reqwest::Method::GET, Vec::new(), None),
        Some(object) => {
            let method = parse_method(object)?;
            let headers = parse_headers(object)?;
            let body = parse_body(object)?;
            if body.is_some() && (method == reqwest::Method::GET || method == reqwest::Method::HEAD)
            {
                return Err(format!(
                    "{method} cannot carry a request body; use POST, PUT, PATCH, or DELETE"
                ));
            }
            (method, headers, body)
        }
    };
    let permit = permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| format!("too many concurrent fetches; the limit is {MAX_CONCURRENT}"))?;
    let mut request = crate::llm::http_client()
        .request(method.clone(), url.as_str())
        .header(
            "user-agent",
            concat!("terrarium/", env!("CARGO_PKG_VERSION")),
        );
    for (name, value) in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let request = match body {
        Some(body) => request.body(body),
        None => request,
    };
    let mut cancel_rx = cancel.clone();
    // one budget covers the response head and its body consumption
    let deadline = Instant::now() + REQUEST_BUDGET;
    let response = tokio::select! {
        response = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), request.send()) => {
            match response {
                Err(_) => {
                    push_failed_request(receipts, &method, url.as_str());
                    return Err(format!(
                        "the request exceeded its {}-second budget",
                        REQUEST_BUDGET.as_secs()
                    ));
                }
                Ok(Err(e)) => {
                    push_failed_request(receipts, &method, url.as_str());
                    return Err(format!("request failed: {e}"));
                }
                Ok(Ok(response)) => response,
            }
        }
        _ = crate::kernel::cancelled(&mut cancel_rx) => {
            push_failed_request(receipts, &method, url.as_str());
            return Err("the run ended before the response arrived".into());
        }
    };
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let receipt = serde_json::json!({
        "method": method.as_str(),
        "url": final_url,
        "status": status,
        "bytes": 0,
    });
    let receipt_index = {
        let mut queue = receipts.borrow_mut();
        queue.push(receipt);
        queue.len() - 1
    };
    let state = Rc::new(RefCell::new(BodyState {
        response: Arc::new(tokio::sync::Mutex::new(Some(response))),
        bytes: 0,
        receipt: Some(receipt_index),
        deadline,
        done: false,
        _permit: Some(permit),
    }));
    build_fetch_result(ctx, status, &final_url, state, receipts, cancel)
        .map_err(|e| format!("cannot build the fetch result: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_rules_reject_userinfo_fragments_and_schemes() {
        assert!(validate_url("https://api.example.com/x?y=1").is_ok());
        assert!(validate_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("https://user:pass@example.com").is_err());
        assert!(validate_url("https://example.com/#frag").is_err());
        assert!(validate_url("not a url").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        let huge = format!("https://example.com/{}", "x".repeat(URL_CAP));
        let error = validate_url(&huge).unwrap_err();
        assert!(error.contains("URL exceeds"), "{error}");
    }

    #[test]
    fn method_rules_match_the_contract() {
        let ctx = rquickjs::Context::full(&rquickjs::Runtime::new().unwrap()).unwrap();
        ctx.with(|ctx| {
            for method in ["GET", "post", "Delete"] {
                let opts: Object = ctx
                    .eval(format!("({{method: '{method}'}})").as_str())
                    .unwrap();
                assert!(parse_method(&opts).is_ok(), "{method}");
            }
            let opts: Object = ctx.eval("({method: 'TRACE'})").unwrap();
            assert!(parse_method(&opts).unwrap_err().contains("TRACE"));
        });
    }

    #[test]
    fn header_rules_resolve_env_and_reject_crlf() {
        let ctx = rquickjs::Context::full(&rquickjs::Runtime::new().unwrap()).unwrap();
        ctx.with(|ctx| {
            let opts: Object = ctx
                .eval("({headers: {'X-Literal': 'ok', 'X-Bad': 'a\\r\\nb'}})")
                .unwrap();
            let error = parse_headers(&opts).unwrap_err();
            assert!(error.contains("CR"), "{error}");
            let opts: Object = ctx
                .eval("({headers: {'X-Name': {env: 'TERRARIUM_TEST_ENV_VAR'}}})")
                .unwrap();
            let error = parse_headers(&opts).unwrap_err();
            assert!(error.contains("TERRARIUM_TEST_ENV_VAR"), "{error}");
            std::env::set_var("TERRARIUM_TEST_ENV_VAR", "resolved");
            let opts: Object = ctx
                .eval("({headers: {'X-Name': {env: 'TERRARIUM_TEST_ENV_VAR'}}})")
                .unwrap();
            assert_eq!(
                parse_headers(&opts).unwrap(),
                vec![("X-Name".to_string(), "resolved".to_string())]
            );
        });
    }

    #[test]
    fn request_body_is_bounded() {
        let ctx = rquickjs::Context::full(&rquickjs::Runtime::new().unwrap()).unwrap();
        ctx.with(|ctx| {
            let small: Object = ctx.eval("({body: 'hello'})").unwrap();
            assert_eq!(parse_body(&small).unwrap().as_deref(), Some("hello"));
            let big: Object = ctx
                .eval(format!("({{body: 'x'.repeat({})}})", REQUEST_BODY_CAP + 1).as_str())
                .unwrap();
            assert!(parse_body(&big).unwrap_err().contains("limit"));
        });
    }

    // ------------------------------------------------------------------
    // End-to-end through the cage: offline fails closed, a live local
    // server exercises the real request path without the network
    // ------------------------------------------------------------------

    fn net_env(root: &std::path::Path, offline: bool) -> crate::RunEnv {
        crate::RunEnv {
            fs: crate::fs::RunFilesystemAuthority::ReadOnly,
            proc: crate::proc::ProcAuthority::default(),
            net_offline: offline,
            table: std::rc::Rc::new(crate::proc::ProcTable::new(root.join("procs"))),
            working_root: root.canonicalize().unwrap(),
            receipts: crate::RunEnv::receipts(),
        }
    }

    #[tokio::test]
    async fn offline_fetch_fails_closed() {
        let root =
            std::env::temp_dir().join(format!("terrarium-net-offline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let env = net_env(&root, true);
        let (tx, _rx) = watch::channel(false);
        let out = crate::kernel::eval_js(
            "return await host.net.fetch('https://example.com/')",
            2_000,
            &env,
            tx,
        )
        .await;
        assert!(!out.ok, "value: {:?}", out.value);
        let message = out.error.expect("offline").message;
        assert!(message.contains("--offline"), "{message}");
        assert!(out.receipts.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn fetch_streams_a_local_http_response_and_journals_the_receipt() {
        use std::io::{Read, Write};
        let root = std::env::temp_dir().join(format!("terrarium-net-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() > deadline {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).ok();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok();
            let mut buf = [0u8; 8192];
            let mut seen = Vec::new();
            for _ in 0..64 {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        seen.extend_from_slice(&buf[..n]);
                        if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            );
            let _ = stream.flush();
        });
        let env = net_env(&root, false);
        let source = format!(
            "const r = await host.net.fetch('http://127.0.0.1:{port}/hello');\n\
             let body = '';\n\
             for await (const chunk of r.body) body += chunk;\n\
             return {{status: r.status, body}}"
        );
        let (tx, _rx) = watch::channel(false);
        let out = crate::kernel::eval_js(&source, 5_000, &env, tx).await;
        server.join().unwrap();
        assert!(out.ok, "error: {:?}", out.error);
        assert_eq!(
            out.value,
            Some(serde_json::json!({"status": 200, "body": "hello"}))
        );
        assert_eq!(out.receipts.len(), 1);
        assert_eq!(out.receipts[0]["method"], serde_json::json!("GET"));
        assert!(out.receipts[0]["url"].as_str().unwrap().ends_with("/hello"));
        assert_eq!(out.receipts[0]["status"], serde_json::json!(200));
        assert_eq!(out.receipts[0]["bytes"], serde_json::json!(5));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_request_that_never_gets_a_response_is_still_journaled() {
        // port 1 on loopback refuses the connection: bytes were dispatched, no
        // response arrives — the egress event must still leave a receipt
        let root = std::env::temp_dir().join(format!("terrarium-net-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let env = net_env(&root, false);
        let source =
            "try { await host.net.fetch('http://127.0.0.1:1/nope'); return 'no error'; }\n\
             catch (e) { return 'caught'; }";
        let (tx, _rx) = watch::channel(false);
        let out = crate::kernel::eval_js(source, 5_000, &env, tx).await;
        assert!(out.ok, "error: {:?}", out.error);
        assert_eq!(out.value, Some(serde_json::json!("caught")));
        assert_eq!(out.receipts.len(), 1);
        assert_eq!(out.receipts[0]["method"], serde_json::json!("GET"));
        assert_eq!(out.receipts[0]["status"], serde_json::json!(0));
        assert_eq!(out.receipts[0]["bytes"], serde_json::json!(0));
        let _ = std::fs::remove_dir_all(&root);
    }
}
