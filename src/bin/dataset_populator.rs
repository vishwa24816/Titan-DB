use std::time::Instant;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

// Fast simple LCG pseudo-random generator without extra deps
struct SimpleRng(u64);
impl SimpleRng {
    fn new(seed: u64) -> Self { SimpleRng(seed) }
    fn gen_range(&mut self, low: u64, high: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        low + (self.0 % (high - low + 1))
    }
}

#[tokio::main]
async fn main() {
    let ws_url = "ws://127.0.0.1:3030/ws";
    println!("Connecting to WebSocket at: {}", ws_url);

    let (ws_stream, _) = connect_async(ws_url).await.expect("Failed to connect to WS");
    let (mut write, mut read) = ws_stream.split();

    // 1. Create table with 20 columns: id INT, col1 INT, col2 INT, ..., col19 INT
    let mut cols_def = vec!["id INT".to_string()];
    for c in 1..20 {
        cols_def.push(format!("col{} INT", c));
    }
    let create_sql = format!("CREATE TABLE dataset ({})", cols_def.join(", "));
    println!("Creating table: {}", create_sql);
    write.send(Message::Text(create_sql)).await.unwrap();
    let resp = read.next().await.unwrap().unwrap();
    println!("Create Table Response: {}", resp);

    // 2. Insert 200 rows with 20 columns of random numbers (100,000 to 900,000)
    let start = Instant::now();
    let mut rng = SimpleRng::new(123456789);

    println!("Inserting 200 rows x 20 columns of random numbers [100000..900000]...");
    for r in 1..=200 {
        let mut row_vals = vec![r.to_string()];
        for _ in 1..20 {
            row_vals.push(rng.gen_range(100_000, 900_000).to_string());
        }
        let insert_sql = format!("INSERT INTO dataset VALUES ({})", row_vals.join(", "));
        write.send(Message::Text(insert_sql)).await.unwrap();
        let _ = read.next().await.unwrap().unwrap();
    }
    let elapsed = start.elapsed();
    println!("Inserted 200 rows x 20 columns in {:?}", elapsed);

    // 3. Query sample via WS
    write.send(Message::Text("SELECT * FROM dataset".to_string())).await.unwrap();
    let query_resp = read.next().await.unwrap().unwrap();
    println!("Query verification received {} bytes of ResultSet data.", query_resp.to_text().unwrap().len());
    println!("\nDataset is ready in TitanDB! Test it live at: http://localhost:3030");
}
