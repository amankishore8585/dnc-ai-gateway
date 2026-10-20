use reqwest::Client;
use serde_json::Value;
use hmac::Mac;
use serde::Serialize;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

pub async fn create_order(
    client: &Client,
    key_id: &str,
    key_secret: &str,
    user_id: &str,
    app_id: &str,
) -> Result<Value, String> {
    let amount = 12900i64;

    let response = client
        .post("https://api.razorpay.com/v1/orders")
        .basic_auth(key_id, Some(key_secret))
        .json(&serde_json::json!({
            "amount": amount,
            "currency": "INR",
            "receipt": format!(
                "{}-{}",
                user_id,
                uuid::Uuid::new_v4()
            ),
            "notes": {
                "user_id": user_id,
                "app_id": app_id,
                "plan": "premium",
                "payment_type": "one_time"
            }
        }))
        .send()
        .await
        .map_err(|e| format!("Razorpay order request failed: {}", e))?;

    let status = response.status();

    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed reading Razorpay response: {}", e))?;

    if !status.is_success() {
        return Err(body);
    }

    serde_json::from_str(&body)
        .map_err(|e| format!("Invalid Razorpay response: {}", e))
}


pub fn verify_signature(
    key_secret: &str,
    order_id: &str,
    payment_id: &str,
    signature: &str,
) -> bool {
    let payload = format!("{}|{}", order_id, payment_id);

    let mut mac =
        hmac::Hmac::<sha2::Sha256>::new_from_slice(
            key_secret.as_bytes()
        )
        .expect("Invalid HMAC key");

    mac.update(payload.as_bytes());

    let expected =
        hex::encode(mac.finalize().into_bytes());

    expected == signature
}


pub async fn fetch_order(
    client: &Client,
    key_id: &str,
    key_secret: &str,
    order_id: &str,
) -> Result<Value, String> {
    let response = client
        .get(format!(
            "https://api.razorpay.com/v1/orders/{}",
            order_id
        ))
        .basic_auth(key_id, Some(key_secret))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = response.status();

    let body = response
        .text()
        .await
        .map_err(|e| e.to_string())?;

    if !status.is_success() {
        return Err(body);
    }

    serde_json::from_str(&body)
        .map_err(|e| e.to_string())
}


pub async fn fetch_payment(
    client: &Client,
    key_id: &str,
    key_secret: &str,
    payment_id: &str,
) -> Result<Value, String> {
    let response = client
        .get(format!(
            "https://api.razorpay.com/v1/payments/{}",
            payment_id
        ))
        .basic_auth(key_id, Some(key_secret))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = response.status();

    let body = response
        .text()
        .await
        .map_err(|e| e.to_string())?;

    if !status.is_success() {
        return Err(body);
    }

    serde_json::from_str(&body)
        .map_err(|e| e.to_string())
}

pub async fn verify_payment(
    client: &Client,
    key_id: &str,
    key_secret: &str,
    user_id: &str,
    order_id: &str,
    payment_id: &str,
    signature: &str,
) -> Result<(), String> {

    // --------------------------------------------------------
    // 1. Verify Razorpay signature
    // --------------------------------------------------------

    if !verify_signature(
        key_secret,
        order_id,
        payment_id,
        signature,
    ) {
        return Err(
            "Invalid payment signature".to_string()
        );
    }

    // --------------------------------------------------------
    // 2. Fetch order from Razorpay
    // --------------------------------------------------------

    let order =
        fetch_order(
            client,
            key_id,
            key_secret,
            order_id,
        )
        .await?;

    // --------------------------------------------------------
    // 3. Verify order belongs to this user
    // --------------------------------------------------------

    let order_user_id =
        order
            .get("notes")
            .and_then(|v| v.get("user_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

    if order_user_id != user_id {
        return Err(
            "Order does not belong to this user".to_string()
        );
    }

    // --------------------------------------------------------
    // 4. Verify order amount and currency
    // --------------------------------------------------------

    let amount =
        order
            .get("amount")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

    let currency =
        order
            .get("currency")
            .and_then(|v| v.as_str())
            .unwrap_or("");

    if amount != 12900 || currency != "INR" {
        return Err(
            "Invalid order amount or currency".to_string()
        );
    }

    // --------------------------------------------------------
    // 5. Fetch payment from Razorpay
    // --------------------------------------------------------

    let payment =
        fetch_payment(
            client,
            key_id,
            key_secret,
            payment_id,
        )
        .await?;

    // --------------------------------------------------------
    // 6. Verify payment belongs to this order
    // --------------------------------------------------------

    let payment_order_id =
        payment
            .get("order_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

    if payment_order_id != order_id {
        return Err(
            "Payment does not belong to order".to_string()
        );
    }

    // --------------------------------------------------------
    // 7. Verify payment amount, currency and status
    // --------------------------------------------------------

    let payment_amount =
        payment
            .get("amount")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

    let payment_currency =
        payment
            .get("currency")
            .and_then(|v| v.as_str())
            .unwrap_or("");

    let payment_status =
        payment
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("");

    if payment_amount != 12900
        || payment_currency != "INR"
        || payment_status != "captured"
    {
        return Err(
            "Payment has not been successfully captured"
                .to_string()
        );
    }

    // --------------------------------------------------------
    // Everything Razorpay-side is valid.
    //
    // Database activation will be handled by db.rs.
    // --------------------------------------------------------

    Ok(())
}

///google play 
#[derive(Serialize)]
struct GoogleJwtClaims {
    iss: String,
    scope: String,
    aud: String,
    iat: usize,
    exp: usize,
}

pub async fn get_google_access_token(
    client: &Client,
    service_account_email: &str,
    private_key: &str,
) -> Result<String, String> {
    let now = chrono::Utc::now().timestamp() as usize;

    let claims = GoogleJwtClaims {
        iss: service_account_email.to_string(),
        scope: "https://www.googleapis.com/auth/androidpublisher".to_string(),
        aud: "https://oauth2.googleapis.com/token".to_string(),
        iat: now,
        exp: now + 3600,
    };

    let private_key = private_key.replace("\\n", "\n");

    let encoding_key =
        EncodingKey::from_rsa_pem(private_key.as_bytes())
            .map_err(|e| format!("Invalid Google private key: {}", e))?;

    let jwt = encode(
        &Header::new(Algorithm::RS256),
        &claims,
        &encoding_key,
    )
    .map_err(|e| format!("Failed creating Google JWT: {}", e))?;

    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:jwt-bearer",
            ),
            ("assertion", jwt.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("Google OAuth request failed: {}", e))?;

    let status = response.status();

    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed reading Google OAuth response: {}", e))?;

    if !status.is_success() {
        return Err(format!("Google OAuth failed: {}", body));
    }

    let json: Value = serde_json::from_str(&body)
        .map_err(|e| format!("Invalid Google OAuth response: {}", e))?;

    json.get("access_token")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
        .ok_or_else(|| "Google OAuth response missing access_token".to_string())
}

pub async fn verify_google_play_purchase(
    client: &Client,
    access_token: &str,
    package_name: &str,
    product_id: &str,
    purchase_token: &str,
) -> Result<Value, String> {
    let url = format!(
        "https://androidpublisher.googleapis.com/androidpublisher/v3/applications/{}/purchases/products/{}/tokens/{}",
        package_name,
        product_id,
        purchase_token
    );

    let response = client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| format!("Google Play verification request failed: {}", e))?;

    let status = response.status();

    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed reading Google Play response: {}", e))?;

    if !status.is_success() {
        return Err(format!("Google Play verification failed: {}", body));
    }

    serde_json::from_str(&body)
        .map_err(|e| format!("Invalid Google Play purchase response: {}", e))
}

pub async fn consume_google_play_purchase(
    client: &Client,
    access_token: &str,
    package_name: &str,
    product_id: &str,
    purchase_token: &str,
) -> Result<(), String> {
    let url = format!(
        "https://androidpublisher.googleapis.com/androidpublisher/v3/applications/{}/purchases/products/{}/tokens/{}:consume",
        package_name,
        product_id,
        purchase_token
    );

    let response = client
        .post(url)
        .bearer_auth(access_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| format!("Google Play consume request failed: {}", e))?;

    let status = response.status();

    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());

        return Err(format!("Google Play consume failed: {}", body));
    }

    Ok(())
}