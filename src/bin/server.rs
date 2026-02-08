use std::sync::Arc;
use tokio::sync::Mutex;
use warp::Filter;
use parking_lot::RwLock;

use titan_db::storage::pager::Pager;
use titan_db::catalog::Catalog;
use titan_db::sql::executor::Executor;
use titan_db::sql::ExecutionResult;

#[tokio::main]
async fn main() {
    // Initialize DB
    let pager = Arc::new(Pager::open("titan_web.db").expect("Failed to open DB"));
    let catalog = Arc::new(RwLock::new(Catalog::new()));
    let executor = Arc::new(Executor::new(pager, catalog));
    
    // Allow sharing executor across threads/tasks
    // Executor uses Arc internally for pager/catalog, so it's cheap to clone if it implemented Clone
    // But Executor struct itself doesn't derive Clone. Let's wrap it.
    let executor = Arc::new(Mutex::new(executor));

    println!("TitanDB Server starting on 127.0.0.1:3030");

    // Static files for UI
    let static_files = warp::fs::dir("web");

    // WebSocket route
    let executor_filter = warp::any().map(move || executor.clone());
    let ws_route = warp::path("ws")
        .and(warp::ws())
        .and(executor_filter)
        .map(|ws: warp::ws::Ws, executor| {
            ws.on_upgrade(move |socket| handle_ws(socket, executor))
        });

    let routes = static_files.or(ws_route);

    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;
}

async fn handle_ws(mut ws: warp::ws::WebSocket, executor: Arc<Mutex<Arc<Executor>>>) {
    use futures::{StreamExt, SinkExt};

    while let Some(result) = ws.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("websocket error: {}", e);
                break;
            }
        };

        if let Ok(text) = msg.to_str() {
            println!("Received query: {}", text);
            
            // Execute query
            let response_json = {
                let exec_lock = executor.lock().await;
                match exec_lock.execute(text) {
                    Ok(res) => serde_json::to_string(&res).unwrap(),
                    Err(e) => serde_json::to_string(&ExecutionResult::Message(format!("Error: {}", e))).unwrap(),
                }
            };

            if let Err(e) = ws.send(warp::ws::Message::text(response_json)).await {
                 eprintln!("websocket send error: {}", e);
                 break;
            }
        }
    }
}
