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
    let _ = client
        .execute(
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
        )
        .await;
}

pub async fn insert_cache_hit(
    client: &Client,
    user_id: &str,
    route: &str,
    model: &str,
) {
    let _ = client
        .execute(
            "INSERT INTO usage_logs
            (user_id, route, model, prompt_tokens, completion_tokens, total_tokens, cost, latency_ms, status_code)
            VALUES ($1,$2,$3,0,0,0,0,0,200)",
            &[
                &user_id,
                &route,
                &model,
            ],
        )
        .await;
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

pub async fn email_exists(
    client: &Client,
    email: &str,
    app_id: &str,
) -> Result<bool, tokio_postgres::Error> {
    let row = client
        .query_one(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM users
                WHERE email = $1
                  AND app_id = $2
            )
            "#,
            &[&email, &app_id],
        )
        .await?;

    Ok(row.get(0))
}

pub async fn create_user(
    client: &Client,
    username: &str,
    email: &str,
    password_hash: &str,
    app_id: &str,
) -> Result<i32, tokio_postgres::Error> {
    let row = client
        .query_one(
            "INSERT INTO users (
                username,
                email,
                password_hash,
                email_verified,
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
                $3,
                FALSE,
                $4,
                'free',
                100,
                NULL,
                'free',
                NULL
            )
            RETURNING id",
            &[&username, &email, &password_hash, &app_id],
        )
        .await?;

    let user_id: i32 = row.get("id");

    Ok(user_id)
}

pub async fn create_email_verification(
    client: &Client,
    user_id: i32,
    otp_hash: &str,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "INSERT INTO email_verifications (user_id, otp_hash, expires_at)
             VALUES ($1, $2, $3)",
            &[&user_id, &otp_hash, &expires_at],
        )
        .await?;

    Ok(())
}

pub async fn get_email_verification(
    client: &Client,
    user_id: i32,
) -> Result<Option<(String, chrono::DateTime<chrono::Utc>)>, tokio_postgres::Error> {
    client
        .query_opt(
            "SELECT otp_hash, expires_at
             FROM email_verifications
             WHERE user_id = $1
             ORDER BY created_at DESC
             LIMIT 1",
            &[&user_id],
        )
        .await
        .map(|row| {
            row.map(|r| {
                (
                    r.get::<_, String>(0),
                    r.get::<_, chrono::DateTime<chrono::Utc>>(1),
                )
            })
        })
}

pub async fn set_email_verified(
    client: &Client,
    user_id: i32,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE users
             SET email_verified = TRUE
             WHERE id = $1",
            &[&user_id],
        )
        .await?;

    Ok(())
}

pub async fn get_user_id_by_email(
    client: &Client,
    email: &str,
    app_id: &str,
) -> Result<Option<i32>, tokio_postgres::Error> {
    client
        .query_opt(
            "SELECT id
             FROM users
             WHERE email = $1
               AND app_id = $2
             LIMIT 1",
            &[&email, &app_id],
        )
        .await
        .map(|row| row.map(|r| r.get::<_, i32>("id")))
}

pub async fn delete_email_verification(
    client: &Client,
    user_id: i32,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "DELETE FROM email_verifications
             WHERE user_id = $1",
            &[&user_id],
        )
        .await?;

    Ok(())
}

pub async fn get_user_for_login(
    client: &Client,
    identifier: &str,
    app_id: &str,
) -> Result<Option<(i32, String, String, String, String, i32, bool)>, tokio_postgres::Error> {

    let row = client
        .query_opt(
            r#"
            SELECT
                id,
                username,
                email,
                password_hash,
                plan,
                monthly_limit,
                email_verified
            FROM users
            WHERE app_id = $2
              AND (
                  username = $1
                  OR email = $1
              )
            LIMIT 1
            "#,
            &[&identifier, &app_id],
        )
        .await?;

    Ok(row.map(|r| {
        (
            r.get("id"),
            r.get("username"),
            r.get("email"),
            r.get("password_hash"),
            r.get("plan"),
            r.get("monthly_limit"),
            r.get("email_verified"),
        )
    }))
}

pub async fn get_user_plan(
    client: &Client,
    user_id: &str,
) -> Result<(String, i32), tokio_postgres::Error> {

    // --------------------------------------------------
    // If Premium has expired, permanently move user
    // to the manual plan in the database.
    // --------------------------------------------------

    client
        .execute(
            r#"
            UPDATE users
            SET
                plan = 'manual',
                monthly_limit = 0
            WHERE username = split_part($1, ':', 1)
              AND app_id = split_part($1, ':', 2)
              AND plan = 'premium'
              AND premium_expires_at IS NOT NULL
              AND premium_expires_at <= NOW()
            "#,
            &[&user_id],
        )
        .await?;

    // --------------------------------------------------
    // Get the actual current plan from the database.
    // --------------------------------------------------

    let row = client
        .query_one(
            r#"
            SELECT
                plan,
                monthly_limit
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
              AND created_at >= (
                  SELECT premium_started_at
                  FROM users
                  WHERE username = split_part($1, ':', 1)
                    AND app_id = split_part($1, ':', 2)
              )
              AND total_tokens > 0
            "#,
            &[&user_id],
        )
        .await?;

    let count: i64 = row.get(0);

    Ok(count)
}

pub async fn get_lifetime_ai_calls(
    client: &Client,
    user_id: &str,
) -> Result<i64, tokio_postgres::Error> {
    let row = client
        .query_one(
            r#"
            SELECT COUNT(*)
            FROM usage_logs
            WHERE user_id = $1
              AND total_tokens > 0
            "#,
            &[&user_id],
        )
        .await?;

    let count: i64 = row.get(0);

    Ok(count)
}

pub async fn set_user_manual(
    client: &Client,
    user_id: &str,
) -> Result<bool, tokio_postgres::Error> {
    let updated = client
        .execute(
            r#"
            UPDATE users
            SET
                plan = 'manual',
                monthly_limit = 0
            WHERE username = split_part($1, ':', 1)
              AND app_id = split_part($1, ':', 2)
            "#,
            &[&user_id],
        )
        .await?;

    Ok(updated > 0)
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
    amount: i32,
    currency: &str,
) -> Result<bool, tokio_postgres::Error> {
    let updated = client
        .execute(
            r#"
            UPDATE users
            SET
                plan = 'premium',
                monthly_limit = 1000,
                premium_started_at = NOW(),
                premium_expires_at = NOW() + INTERVAL '30 days',
                payment_type = 'one_time'
            WHERE username = $1
              AND app_id = $2
            "#,
            &[&user_id, &app_id],
        )
        .await?;

    if updated == 0 {
        return Ok(false);
    }

    println!(
        "PAYMENT INSERT DEBUG: user_id={:?}, order_id={:?}, payment_id={:?}, amount={}, currency={:?}",
        user_id,
        order_id,
        payment_id,
        amount,
        currency
    );

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