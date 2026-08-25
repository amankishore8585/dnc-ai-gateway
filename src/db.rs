use tokio_postgres::{NoTls, Client};

pub async fn connect_db() -> Client {
    let (client, connection) =
        tokio_postgres::connect(
            "host=localhost user=gateway_user password=strongpassword dbname=ai_gateway",
            NoTls,
        )
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

pub async fn username_exists(
    client: &Client,
    username: &str,
    app_id: &str,
) -> Result<bool, tokio_postgres::Error> {
    let row = client
        .query_one(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM users
                WHERE username = $1
                  AND app_id = $2
            )
            "#,
            &[&username, &app_id],
        )
        .await?;

    Ok(row.get(0))
}

pub async fn create_user(
    client: &Client,
    username: &str,
    app_id: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            r#"
            INSERT INTO users (
                username,
                app_id,
                plan,
                monthly_limit,
                premium_expires_at,
                payment_type,
                subscription_id
            )
            VALUES (
                $1,
                $2,
                'free',
                100,
                NULL,
                'free',
                NULL
            )
            "#,
            &[&username, &app_id],
        )
        .await?;

    Ok(())
}

pub async fn get_user_plan(
    client: &Client,
    user_id: &str,
) -> Result<(String, i32), tokio_postgres::Error> {
    let row = client
        .query_one(
            r#"
            SELECT plan, monthly_limit
            FROM users
            WHERE username = split_part($1, ':', 1)
              AND app_id = split_part($1, ':', 2)
            "#,
            &[&user_id],
        )
        .await?;

    let plan: String = row.get("plan");
    let monthly_limit: i32 = row.get("monthly_limit");

    Ok((plan, monthly_limit))
}

pub async fn get_monthly_ai_calls(
    client: &Client,
    user_id: &str,
) -> Result<i64, tokio_postgres::Error> {
    let row = client
        .query_one(
            r#"
            SELECT COUNT(*)
            FROM usage_logs
            WHERE user_id = $1
              AND created_at >= date_trunc('month', NOW())
              AND total_tokens > 0
            "#,
            &[&user_id],
        )
        .await?;

    let count: i64 = row.get(0);

    Ok(count)
}

pub async fn payment_exists(
    client: &tokio_postgres::Client,
    payment_id: &str,
) -> Result<bool, tokio_postgres::Error> {
    let row = client
        .query_one(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM payments
                WHERE razorpay_payment_id = $1
            )
            "#,
            &[&payment_id],
        )
        .await?;

    Ok(row.get(0))
}

pub async fn activate_premium(
    client: &tokio_postgres::Client,
    user_id: &str,
    app_id: &str,
    order_id: &str,
    payment_id: &str,
    amount: i64,
    currency: &str,
) -> Result<bool, tokio_postgres::Error> {

    let updated = client
        .execute(
            r#"
            UPDATE users
            SET
                plan = 'premium',
                monthly_limit = 1000,
                premium_expires_at = NOW() + INTERVAL '30 days',
                payment_type = 'one_time',
                subscription_id = $3
            WHERE username = $1
              AND app_id = $2
            "#,
            &[&user_id, &app_id, &order_id],
        )
        .await?;

    if updated == 0 {
        return Ok(false);
    }

    client
        .execute(
            r#"
            INSERT INTO payments (
                user_id,
                razorpay_order_id,
                razorpay_payment_id,
                amount,
                currency,
                payment_type
            )
            VALUES ($1, $2, $3, $4, $5, 'one_time')
            "#,
            &[
                &user_id,
                &order_id,
                &payment_id,
                &amount,
                &currency,
            ],
        )
        .await?;

    Ok(true)
}