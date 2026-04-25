use tokio_postgres::{NoTls, Client};

pub async fn connect_db() -> Client {
    let (client, connection) =
        tokio_postgres::connect("host=localhost user=gateway_user password=strongpassword dbname=ai_gateway", NoTls)
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
    user_id: &str,
    route: &str,
    model: &str,
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    cost: f64,
    latency_ms: i64,
    status_code: i32,
) {
    let _ = client.execute(
        "INSERT INTO usage_logs 
        (user_id, route, model, prompt_tokens, completion_tokens, total_tokens, cost, latency_ms, status_code)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        &[
            &user_id,
            &route,
            &model,
            &prompt_tokens,
            &completion_tokens,
            &total_tokens,
            &cost,
            &latency_ms,
            &status_code,
        ],
    ).await;
}

pub async fn insert_cache_hit(
    client: &Client,
    user_id: &str,
    route: &str,
    model: &str,
) {
    let _ = client.execute(
        "INSERT INTO usage_logs 
        (user_id, route, model, prompt_tokens, completion_tokens, total_tokens, cost, latency_ms, status_code)
        VALUES ($1,$2,$3,0,0,0,0,0,200)",
        &[
            &user_id,
            &route,
            &model,
        ],
    ).await;
}