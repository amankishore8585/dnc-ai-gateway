use tokio_postgres::{NoTls, Client};

pub async fn connect_db() -> Client {
    let (client, connection) =
        tokio_postgres::connect("host=localhost user=postgres password=postgres dbname=postgres", NoTls)
            .await
            .expect("Failed to connect to DB");

    // spawn connection handler
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("DB connection error: {}", e);
        }
    });

    client
}

pub async fn insert_usage(
    client: &Client,
    api_key: &str,
    endpoint: &str,
    latency: i64,
) {
    let _ = client.execute(
        "INSERT INTO usage_records (api_key, endpoint, latency_ms) VALUES ($1, $2, $3)",
        &[&api_key, &endpoint, &latency],
    ).await;
}