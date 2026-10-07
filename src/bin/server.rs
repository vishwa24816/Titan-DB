use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use warp::Filter;

use titan_db::catalog::Catalog;
use titan_db::sql::executor::Executor;
use titan_db::sql::ExecutionResult;
use titan_db::storage::pager::Pager;
use titan_db::txn::TxContext;

const MAX_MSG_BYTES: usize = 1024 * 1024; // 1 MiB WS cap (T-01-11)
const MAX_MSG_PER_SEC: u64 = 100; // per-conn rate limit (T-01-11)
const SESSION_IDLE_SECS: u64 = 300; // idle tx rollback timeout

struct Session {
    tx: Option<TxContext>,
    last_active: Instant,
    msg_count: u64,
    window_start: Instant,
}

type Sessions = Arc<Mutex<HashMap<String, Session>>>;

#[derive(Debug, serde::Deserialize)]
struct TxMsg {
    session: Option<String>,
    op: Option<String>,
    sql: Option<String>,
    token: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct TxResp {
    session: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ExecutionResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn args() -> (String, Option<String>, String) {
    // --db <path> | TITAN_DB env (no CWD default); --token <t> | TITAN_TOKEN; --bind <addr>
    let mut db = std::env::var("TITAN_DB").unwrap_or_default();
    let mut token = std::env::var("TITAN_TOKEN").ok();
    let mut bind = "127.0.0.1:3030".to_string();
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--db" if i + 1 < argv.len() => {
                db = argv[i + 1].clone();
                i += 2;
            }
            "--token" if i + 1 < argv.len() => {
                token = Some(argv[i + 1].clone());
                i += 2;
            }
            "--bind" if i + 1 < argv.len() => {
                bind = argv[i + 1].clone();
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }
    if db.is_empty() {
        eprintln!("missing --db <path> (or TITAN_DB env); refusing CWD-relative default");
        std::process::exit(2);
    }
    (db, token, bind)
}

/// Constant-time token compare (T-01-10). None token = open only on loopback.
fn token_ok(provided: Option<&str>, expected: &Option<String>) -> bool {
    match expected {
        None => true,
        Some(exp) => {
            let p = provided.unwrap_or("").as_bytes();
            let e = exp.as_bytes();
            if p.len() != e.len() {
                return false;
            }
            let mut diff = 0u8;
            for (a, b) in p.iter().zip(e.iter()) {
                diff |= a ^ b;
            }
            diff == 0
        }
    }
}

fn err_resp(session: String, code: &str, msg: &str) -> String {
    let r = TxResp {
        session,
        status: "error".to_string(),
        tx_id: None,
        result: None,
        code: Some(code.to_string()),
        error: Some(msg.to_string()),
    };
    serde_json::to_string(&r).unwrap_or_else(|_| r#"{"status":"error"}"#.to_string())
}

#[tokio::main]
async fn main() {
    let (db_path, token, bind) = args();
    let token = Arc::new(token);
    let remote = !bind.starts_with("127.") && !bind.starts_with("localhost");
    if remote && token.as_ref().is_none() {
        eprintln!("refusing non-local bind without --token (T-01-10)");
        std::process::exit(2);
    }
    let pager = Arc::new(Pager::open(&db_path).unwrap_or_else(|e| {
        eprintln!("Failed to open DB {}: {}", db_path, e);
        std::process::exit(1);
    }));
    let catalog = Arc::new(parking_lot::RwLock::new(Catalog::new()));
    let executor = Arc::new(Executor::new(pager, catalog));
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    // Idle-tx reaper: rollback sessions idle > SESSION_IDLE_SECS.
    {
        let sessions = sessions.clone();
        let executor = executor.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let stale: Vec<(String, TxContext)> = {
                    let mut s = sessions.lock();
                    let now = Instant::now();
                    let mut out = Vec::new();
                    for (id, sess) in s.iter_mut() {
                        if let Some(ctx) = sess.tx.take() {
                            if now.duration_since(sess.last_active)
                                > Duration::from_secs(SESSION_IDLE_SECS)
                            {
                                out.push((id.clone(), ctx));
                            } else {
                                sess.tx = Some(ctx);
                            }
                        }
                    }
                    out
                };
                for (_, ctx) in stale {
                    executor.tx_manager().rollback(ctx);
                }
            }
        });
    }
    println!("TitanDB Server on {} (db: {})", bind, db_path);

    // rust-embed decision: sidecar dir (no new dep; embed skipped pending
    // human crates.io review per T-01-SC). Debug-fallback = live web/ dir.
    let static_files = warp::fs::dir("web");
    let ex = warp::any().map({
        let executor = executor.clone();
        move || executor.clone()
    });
    let tk = warp::any().map({
        let token = token.clone();
        move || token.clone()
    });
    let se = warp::any().map({
        let sessions = sessions.clone();
        move || sessions.clone()
    });
    let ws_route = warp::path("ws")
        .and(warp::ws())
        .and(ex)
        .and(tk)
        .and(se)
        .map(
            |ws: warp::ws::Ws,
             executor: Arc<Executor>,
             token: Arc<Option<String>>,
             sessions: Sessions| {
                ws.on_upgrade(move |socket| handle_ws(socket, executor, token, sessions))
            },
        );
    let routes = static_files.or(ws_route);
    let addr: std::net::SocketAddr = bind.parse().unwrap_or_else(|_| {
        eprintln!("bad --bind value");
        std::process::exit(2);
    });
    warp::serve(routes).run(addr).await;
}

async fn handle_ws(
    mut ws: warp::ws::WebSocket,
    executor: Arc<Executor>,
    token: Arc<Option<String>>,
    sessions: Sessions,
) {
    // Per-conn rate-limit state.
    let mut count: u64 = 0;
    let mut window = Instant::now();
    let rate = Arc::new(AtomicU64::new(0));
    let _ = rate;
    while let Some(result) = ws.next().await {
        let msg = match result {
            Ok(m) => m,
            Err(_) => break,
        };
        let text = match msg.to_str() {
            Ok(t) => t,
            Err(_) => continue,
        };
        // 1 MiB cap (T-01-11).
        if text.len() > MAX_MSG_BYTES {
            let _ = ws
                .send(warp::ws::Message::text(err_resp(
                    String::new(),
                    "MSG_TOO_LARGE",
                    "message exceeds 1 MiB cap",
                )))
                .await;
            break; // close oversize conn
        }
        // Rate limit: 100 msg/s per conn.
        if window.elapsed() > Duration::from_secs(1) {
            window = Instant::now();
            count = 0;
        }
        count += 1;
        if count > MAX_MSG_PER_SEC {
            let _ = ws
                .send(warp::ws::Message::text(err_resp(
                    String::new(),
                    "RATE_LIMITED",
                    "too many messages",
                )))
                .await;
            continue;
        }
        // Parse: legacy plain-SQL string OR {session, op, sql?, token?} JSON.
        let (session_id, op, sql, msg_token): (String, String, String, Option<String>) =
            match serde_json::from_str::<TxMsg>(text) {
                Ok(m) => (
                    m.session.unwrap_or_else(|| "default".to_string()),
                    m.op.unwrap_or_else(|| "stmt".to_string()),
                    m.sql.unwrap_or_default(),
                    m.token,
                ),
                Err(_) => (
                    "default".to_string(),
                    "stmt".to_string(),
                    text.to_string(),
                    None,
                ),
            };
        if !token_ok(msg_token.as_deref(), &token) {
            let _ = ws
                .send(warp::ws::Message::text(err_resp(
                    session_id,
                    "UNAUTHORIZED",
                    "bad token",
                )))
                .await;
            continue;
        }
        let resp = handle_op(&session_id, &op, &sql, &executor, &sessions).await;
        if ws.send(warp::ws::Message::text(resp)).await.is_err() {
            break;
        }
    }
}

async fn handle_op(
    session_id: &str,
    op: &str,
    sql: &str,
    executor: &Arc<Executor>,
    sessions: &Sessions,
) -> String {
    let ok = |status: &str, tx_id: Option<u64>, result: Option<ExecutionResult>| -> String {
        serde_json::to_string(&TxResp {
            session: session_id.to_string(),
            status: status.to_string(),
            tx_id,
            result,
            code: None,
            error: None,
        })
        .unwrap_or_else(|_| r#"{"status":"error"}"#.to_string())
    };
    match op {
        "begin" => {
            let ctx = executor.tx_manager().begin();
            let mut s = sessions.lock();
            let e = s.entry(session_id.to_string()).or_insert(Session {
                tx: None,
                last_active: Instant::now(),
                msg_count: 0,
                window_start: Instant::now(),
            });
            if e.tx.is_some() {
                return err_resp(session_id.to_string(), "TX_ACTIVE", "session already in tx");
            }
            e.tx = Some(ctx);
            e.last_active = Instant::now();
            ok("ok", Some(ctx.tx_id), None)
        }
        "commit" => {
            let ctx = {
                sessions
                    .lock()
                    .get_mut(session_id)
                    .and_then(|s| s.tx.take())
            };
            match ctx {
                None => err_resp(session_id.to_string(), "NO_TX", "no active tx"),
                Some(c) => {
                    let ex = executor.clone();
                    let r = tokio::task::spawn_blocking(move || ex.tx_manager().commit(c)).await;
                    match r {
                        Ok(Ok(ts)) => ok("committed", Some(ts), None),
                        Ok(Err(e)) => {
                            let code = e.user_safe_code();
                            let msg = e.user_message().unwrap_or_else(|| "commit failed".to_string());
                            err_resp(session_id.to_string(), code, &msg)
                        }
                        Err(_) => err_resp(session_id.to_string(), "TX_ABORTED", "commit failed"),
                    }
                }
            }
        }
        "rollback" => {
            let ctx = {
                sessions
                    .lock()
                    .get_mut(session_id)
                    .and_then(|s| s.tx.take())
            };
            match ctx {
                None => err_resp(session_id.to_string(), "NO_TX", "no active tx"),
                Some(c) => {
                    executor.tx_manager().rollback(c);
                    ok("rolled-back", None, None)
                }
            }
        }
        _ => {
            // stmt (or legacy plain SQL): never block warp handler.
            let sql = sql.to_string();
            let ex = executor.clone();
            let r = tokio::task::spawn_blocking(move || ex.execute(&sql)).await;
            match r {
                Ok(Ok(res)) => ok("ok", None, Some(res)),
                Ok(Err(e)) => {
                    let code = e.user_safe_code();
                    let msg = e.user_message().unwrap_or_else(|| "query failed".to_string());
                    err_resp(session_id.to_string(), code, &msg)
                }
                Err(_) => err_resp(session_id.to_string(), "TX_ABORTED", "query failed"),
            }
        }
    }
}
