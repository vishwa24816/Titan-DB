use std::time::Instant;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let ws_url = "ws://127.0.0.1:3030/ws";
    println!("Connecting to WebSocket at: {}", ws_url);

    // Initial setup query
    {
        let (ws_stream, _) = connect_async(ws_url).await.expect("Failed to connect to WS");
        let (mut write, mut read) = ws_stream.split();
        write.send(Message::Text("CREATE TABLE ws_items (id INT, value TEXT)".to_string())).await.unwrap();
        let resp = read.next().await.unwrap().unwrap();
        println!("Table Setup: {}", resp);
    }

    let start = Instant::now();
    let mut tasks = Vec::new();

    // 50 concurrent WebSocket Inserts
    for i in 0..50 {
        let task = tokio::spawn(async move {
            let (ws_stream, _) = connect_async("ws://127.0.0.1:3030/ws").await.expect("WS connect error");
            let (mut write, mut read) = ws_stream.split();
            let query = format!("INSERT INTO ws_items (id, value) VALUES ({}, 'ws_val_{}')", i, i);
            write.send(Message::Text(query)).await.unwrap();
            let resp = read.next().await.unwrap().unwrap();
            resp.to_text().unwrap().to_string()
        });
        tasks.push(task);
    }

    // 50 concurrent WebSocket Updates
    for i in 0..50 {
        let task = tokio::spawn(async move {
            let (ws_stream, _) = connect_async("ws://127.0.0.1:3030/ws").await.expect("WS connect error");
            let (mut write, mut read) = ws_stream.split();
            let query = format!("UPDATE ws_items SET value = 'ws_mod_{}' WHERE id = {}", i, i);
            write.send(Message::Text(query)).await.unwrap();
            let resp = read.next().await.unwrap().unwrap();
            resp.to_text().unwrap().to_string()
        });
        tasks.push(task);
    }

    // 50 concurrent WebSocket Deletions
    for i in 0..50 {
        let task = tokio::spawn(async move {
            let (ws_stream, _) = connect_async("ws://127.0.0.1:3030/ws").await.expect("WS connect error");
            let (mut write, mut read) = ws_stream.split();
            let query = format!("DELETE FROM ws_items WHERE id = {}", i);
            write.send(Message::Text(query)).await.unwrap();
            let resp = read.next().await.unwrap().unwrap();
            resp.to_text().unwrap().to_string()
        });
        tasks.push(task);
    }

    let mut ok_count = 0;
    let mut err_count = 0;

    for task in tasks {
        match task.await {
            Ok(res) => {
                if res.contains("Error") {
                    err_count += 1;
                } else {
                    ok_count += 1;
                }
            }
            Err(_) => err_count += 1,
        }
    }

    let elapsed = start.elapsed();
    let total_ops = ok_count + err_count;
    let throughput = (total_ops as f64) / elapsed.as_secs_f64();

    println!("============================================================");
    println!("TITAN-DB WEBSOCKET CONCURRENCY TEST RESULTS (150 WS CLIENTS)");
    println!("============================================================");
    println!("Endpoint:            {}", ws_url);
    println!("Elapsed Time:        {:?}", elapsed);
    println!("Total WS Clients:    {}", total_ops);
    println!("Successful WS Ops:   {}", ok_count);
    println!("Failed WS Ops:       {}", err_count);
    println!("Throughput:          {:.2} ops/sec", throughput);
    println!("============================================================");
}
