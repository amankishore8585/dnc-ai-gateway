// ============================================================
// AI GATEWAY CORE
// ------------------------------------------------------------
// Acts as a reverse proxy for AI APIs (OpenAI for now)
//
// Flow:
// Client → Gateway → Upstream (OpenAI) → Gateway → Client
//
// Features:
// - API Key authentication
// - Rate limiting (token bucket)
// - Load balancing
// - TLS support
// - Token usage extraction
// - Metrics + Stats tracking
// ============================================================

const VERSION: &str = "0.1.0";

// ------------------------------------------------------------
// Per-user per-route statistics (stored in memory)
// ------------------------------------------------------------
// Tracks:
// - request count
// - latency
// - errors
// - cost (currently estimated, will be token-based)
// - token usage (prompt + completion)
// ------------------------------------------------------------
#[derive(Default)]
struct UserStats {
    requests: u64,
    total_latency: u128,
    errors: u64,
    total_cost: f64,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

mod metrics;
mod rate_limiter;
mod load_balancer;
mod config;
mod db;

use metrics::*;
use rate_limiter::{RateLimiter, TokenBucket};
use load_balancer::LoadBalancer;
use config::{ApiKeys, load_api_keys};
use crate::db::connect_db;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
type UsageMap = Arc<
    Mutex<
        HashMap<
            String, // user_id
            HashMap<
                String, // route
                HashMap<String, UserStats> // model → stats
            >
        >
    >
>;

use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, TcpListener};
use tokio::time::{timeout, Duration};
use tokio::io::{AsyncRead, AsyncWrite};

use tokio_native_tls::TlsConnector;
use native_tls::TlsConnector as NativeTlsConnector;
use tracing::{info, warn, error};

use sha2::{Sha256, Digest};

use flate2::read::GzDecoder;
use std::io::Read;

trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

type Balancers = Arc<Mutex<HashMap<String, LoadBalancer>>>;


// ------------------------------------------------------------
// Model pricing (per 1K tokens)
// ------------------------------------------------------------
// Returns: (input_price, output_price)
//
// NOTE:
// - Used later for real cost calculation
// - Currently not fully wired into stats
// ------------------------------------------------------------
fn get_model_price(model: &str) -> (f64, f64) {
    if model.starts_with("gpt-4o-mini") {
        (0.00015, 0.0006)
    } else if model.starts_with("gpt-4o") {
        (0.005, 0.015)
    } else {
        (0.0, 0.0)
    }
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .init();
}

pub struct Request {
    method: String,
    path: String,
    version: String,
    headers: HashMap<String, String>,
}

// ------------------------------------------------------------
// Basic HTTP request parser
// ------------------------------------------------------------
// Extracts:
// - method (GET, POST)
// - path (/v1/chat/completions)
// - headers
//
// NOTE:
// - Minimal parsing (not full HTTP compliant)
// ------------------------------------------------------------

fn parse_request(request: &str) -> Request {
    let request_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = request_line.split_whitespace().collect();

    let method = parts.get(0).unwrap_or(&"").to_string();
    let path = parts.get(1).unwrap_or(&"").to_string();
    let version = parts.get(2).unwrap_or(&"").to_string();

    let sections: Vec<&str> = request.split("\r\n\r\n").collect();
    let headers_block = sections.get(0).unwrap_or(&"");

    let mut headers = HashMap::new();

    for line in headers_block.lines().skip(1) {
        if let Some((k, v)) = line.split_once(":") {
            headers.insert(k.trim().to_string(), v.trim().to_string());
        }
    }

    Request {
        method,
        path,
        version,
        headers,
    }
}

// ------------------------------------------------------------
// /metrics endpoint (Prometheus-style)
// ------------------------------------------------------------
// Returns internal counters:
// - requests
// - failures
// - rate limits
// ------------------------------------------------------------

async fn handle_metrics(req: &Request, client: &mut TcpStream) -> bool {
    if req.path == "/metrics" {
        let body = metrics_text();

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );

        client.write_all(response.as_bytes()).await.ok();
        return true;
    }

    false
}

// ------------------------------------------------------------
// API Key authentication
// ------------------------------------------------------------
// Reads X-API-Key header
// Returns key if present
// ------------------------------------------------------------
async fn authenticate(req: &Request) -> Option<String> {
    req.headers.get("X-API-Key").cloned()
}


async fn send_response(
    client: &mut TcpStream,
    status: &str,
    body: &str,
    request_id: &str,
    start: Instant,
) {
    let duration = start.elapsed().as_millis();

    let response = format!(
        "HTTP/1.1 {}\r\nX-Request-ID: {}\r\nX-Response-Time: {}ms\r\nContent-Length: {}\r\n\r\n{}",
        status,
        request_id,
        duration,
        body.len(),
        body
    );

    let _ = client.write_all(response.as_bytes()).await;
}


// ------------------------------------------------------------
// Rate limiting (Token Bucket per user + route)
// ------------------------------------------------------------
// Each API key has buckets per route
// Controls request rate
// ------------------------------------------------------------
async fn check_rate_limit(
    api_key: &str,
    path: &str,
    limit: f64,
    limiter: &RateLimiter,
) -> bool {

    let allowed = {
        let mut map = limiter.lock().unwrap();

        let routes = map.entry(api_key.to_string()).or_insert_with(HashMap::new);

        let bucket = routes
            .entry(path.to_string())
            .or_insert_with(|| TokenBucket::new(limit, limit));

        bucket.allow()
    };

    if !allowed {
        RATE_LIMITED.fetch_add(1, Ordering::Relaxed);
        return true;
    }

    false
}

// ------------------------------------------------------------
// Request logging
// ------------------------------------------------------------
// Logs:
// - request_id
// - latency
// - upstream status
// - user + API key
// ------------------------------------------------------------

fn log_request(
    req: &Request,
    start: Instant,
    request_id: &str,
    upstream_status: u16,
    upstream_latency: u128,
    api_key: &str,
    user_id: &str,
    ) 
{
    let duration = start.elapsed();

    info!(
        request_id = %request_id,
        method = %req.method,
        path = %req.path,
        api_key = %api_key,  
        user_id = %user_id, 
        duration_ms = duration.as_millis(),
        upstream_status = upstream_status,
        upstream_latency_ms = upstream_latency,
        "request_completed"
    );
}

// ------------------------------------------------------------
// Connect to upstream with retry + timeout
// ------------------------------------------------------------
// Attempts connection up to 2 times
// Used for fault tolerance
// ------------------------------------------------------------

async fn connect_with_retry(addr: String, request_id: &str) -> Option<TcpStream> {
    for attempt in 1..=2 {
        match timeout(Duration::from_secs(3), TcpStream::connect(addr.clone())).await {
            Ok(Ok(stream)) => {
                if attempt > 1 {
                    info!(
                        request_id = %request_id,
                        upstream = %addr,
                        attempt = attempt,
                        "retry_success"
                    );
                }
                return Some(stream);
            }
            _ => {
                warn!(
                    request_id = %request_id,
                    upstream = %addr,
                    attempt = attempt,
                    "retry_attempt"
                );
            }
        }
    }
    None
}


fn extract_model_from_body(buffer: &[u8]) -> String {
    if let Some(header_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
        let body = &buffer[header_end + 4..];

        if let Ok(body_str) = std::str::from_utf8(body) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(body_str) {
                if let Some(model) = json.get("model").and_then(|m| m.as_str()) {
                    return model.to_string();
                }
            }
        }
    }

    "unknown".to_string()
}

fn generate_cache_key(model: &str, body: &str) -> String {
    let input = format!("{}:{}", model, body);
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn extract_and_normalize_prompt(body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {

        // OpenAI format: messages[...].content
        if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
            
            let mut combined = String::new();

            for msg in messages {
                if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                    combined.push_str(content);
                    combined.push(' ');
                }
            }

            return normalize_text(&combined);
        }

        // fallback (your simpler case)
        if let Some(msg) = json.get("message").and_then(|m| m.as_str()) {
            return normalize_text(msg);
        }
    }

    "".to_string()
}

fn normalize_text(input: &str) -> String {
    input
        .to_lowercase()
        .trim()
        .replace(".", "")
        .replace(",", "")
}

// ============================================================
// MAIN REQUEST HANDLER (CORE LOGIC)
// ------------------------------------------------------------
// Handles full lifecycle:
//
// 1. Read request
// 2. Parse HTTP
// 3. Authenticate
// 4. Rate limit
// 5. Route request
// 6. Forward to upstream
// 7. Read response
// 8. Extract token usage (if OpenAI)
// 9. Send response back
// 10. Update stats + logs
// ============================================================

async fn handle_client(
    mut client: TcpStream,
    limiter: RateLimiter,
    balancers: Balancers,
    api_keys: ApiKeys,
    usage_map: UsageMap,
    db_client: Arc<tokio_postgres::Client>
    ){
    use uuid::Uuid;

    let request_id = Uuid::new_v4().to_string();
    let start = Instant::now();
    
    let upstream_status_code: u16;
    let upstream_latency_ms: u128;

    let DAILY_LIMIT: f64 = 0.01; // example: $0.01

    info!(
    request_id = %request_id,
    "request_started"
    );   
    
    let mut buffer = Vec::new();
    let mut temp = [0u8; 1024];

    // ---- STEP 1: Read request headers ----
    // Reads until \r\n\r\n (end of headers)
    // Body handled separately
    loop {
        let n = match client.read(&mut temp).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return,
        };

        buffer.extend_from_slice(&temp[..n]);

        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);

    let ip = client.peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    // ---- STEP 2: Parse incoming HTTP request ----

    let request_str = String::from_utf8_lossy(&buffer);
    let req = parse_request(&request_str);

    // Detect Ai request + model
    let should_parse = req.path.contains("/chat/completions");

    let mut model = if req.path.contains("/chat/completions") {
        extract_model_from_body(&buffer)
    } else {
        "unknown".to_string()
    };

    let mut body_str = String::new();
    let mut cache_key = String::new(); 

    // ---- STEP 3: Handle /stats endpoint ----
    // Returns aggregated usage data

    if req.path == "/stats" && req.method == "GET" {
        let json = {
            let map = usage_map.lock().unwrap();

            let result: HashMap<_, _> = map.iter().map(|(user, routes)| {
            
                let route_map: HashMap<_, _> = routes.iter().map(|(route, models)| {

                    let model_map: HashMap<_, _> = models.iter().map(|(model, stats)| {

                        let avg_latency = if stats.requests > 0 {
                            stats.total_latency / stats.requests as u128
                        } else {
                            0
                        };

                        let error_rate = if stats.requests > 0 {
                            stats.errors as f64 / stats.requests as f64
                        } else {
                            0.0
                        };

                        (
                            model.clone(),
                            serde_json::json!({
                                "upstream_requests": stats.requests,
                                "avg_latency_ms": avg_latency,
                                "errors": stats.errors,
                                "error_rate": error_rate,
                                "total_cost": stats.total_cost,
                                "prompt_tokens": stats.prompt_tokens,
                                "completion_tokens": stats.completion_tokens,
                                "total_tokens": stats.total_tokens,
                            })
                        )

                    }).collect();

                    (route.clone(), model_map)

                }).collect();

                (user.clone(), route_map)

            }).collect();

            serde_json::to_string(&result).unwrap()
        };

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            json.len(),
            json
        );

        let _ = client.write_all(response.as_bytes()).await;
        return;
    }

    info!(
        request_id = %request_id,
        path = %req.path,
        method = %req.method,
        ip = %ip,
        "incoming_request"
    );

    //metrics
    if handle_metrics(&req, &mut client).await {
        return;
    }

    // ---- STEP 3.1: Handle /stats-db (from PostgreSQL) ----
    if req.path.starts_with("/stats-db") && req.method == "GET" {

        // ---- Parse query params ----
        let mut user_filter: Option<String> = None;
        let mut range_filter = "1 day".to_string(); // default

        if let Some(pos) = req.path.find('?') {
            let query_str = &req.path[pos + 1..];

            for pair in query_str.split('&') {
                let mut kv = pair.split('=');
                let key = kv.next().unwrap_or("");
                let val = kv.next().unwrap_or("");

                match key {
                    "user" => user_filter = Some(val.to_string()),
                    "range" => {
                        if val == "24h" {
                            range_filter = "1 day".to_string();
                        } else if val == "7d" {
                            range_filter = "7 days".to_string();
                        } else if val == "all" {
                            range_filter = "all".to_string();
                        }
                    }
                    _ => {}
                }
            }
        }

        // ---- Build dynamic SQL query ----
        let mut query = String::from(
            "SELECT 
                user_id,
                route,
                model,
                COUNT(*) as requests,

                -- ✅ cache hits
                SUM(
                    CASE 
                        WHEN total_tokens = 0 AND cost = 0 THEN 1 
                        ELSE 0 
                    END
                ) as cache_hits,

                SUM(total_tokens)::BIGINT as total_tokens,
                SUM(cost)::DOUBLE PRECISION as total_cost,
                AVG(latency_ms)::DOUBLE PRECISION as avg_latency

            FROM usage_logs"
        
        );

        // 👉 ONLY add WHERE if not "all"
        if range_filter != "all" {
            query.push_str(" WHERE created_at > NOW() - INTERVAL '");
            query.push_str(&range_filter);
            query.push_str("'");

            if user_filter.is_some() {
                query.push_str(" AND user_id = $1");
            }
        } else if user_filter.is_some() {
            query.push_str(" WHERE user_id = $1");
        }

        query.push_str(" GROUP BY user_id, route, model");

        // ---- Execute query ----
        let rows = match if let Some(user) = &user_filter {
            db_client.query(&query, &[user]).await
        } else {
            db_client.query(&query, &[]).await
        } {
            Ok(r) => r,
            Err(e) => {
                let body = format!("DB query failed: {}", e);
                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = client.write_all(response.as_bytes()).await;
                return;
            }
        };

        // Build nested JSON: user → route → model
        let mut result: HashMap<String, HashMap<String, HashMap<String, serde_json::Value>>> = HashMap::new();

        for row in rows {
            let user_id: String = row.get("user_id");
            let route: String = row.get("route");
            let model: String = row.get("model");

            let requests: i64 = row.get::<_, i64>("requests");

            let cache_hits: i64 = row.get("cache_hits");

            let cache_hit_rate = if requests > 0 {
                cache_hits as f64 / requests as f64
            } else {
                0.0
            };

            let total_tokens: i64 = row.get("total_tokens");
            let total_cost: f64 = row.get("total_cost");
            let avg_latency: f64 = row.get("avg_latency");

            let user_entry = result.entry(user_id).or_default();
            let route_entry = user_entry.entry(route).or_default();

            route_entry.insert(
                model,
                json!({
                    "requests": requests,
                    "cache_hits": cache_hits,
                    "cache_hit_rate": cache_hit_rate,
                    "total_tokens": total_tokens,
                    "total_cost": total_cost,
                    "avg_latency_ms": avg_latency
                })
            );
        }

        let json_body = serde_json::to_string(&result).unwrap();

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            json_body.len(),
            json_body
        );

        let _ = client.write_all(response.as_bytes()).await;
        return;
    }

    // ---- STEP 4: API Key Authentication ----

    let api_key = match authenticate(&req).await {
        Some(k) => {
            AUTH_SUCCESS.fetch_add(1, Ordering::Relaxed);
            k
        }
        None => {
            AUTH_FAILURES.fetch_add(1, Ordering::Relaxed);

            warn!(request_id = %request_id, "missing_api_key");

            send_response(
                &mut client,
                "401 Unauthorized",
                "Missing API Key",
                &request_id,
                start
            ).await;
            return;
        }
    };

    let app_id = req
        .headers
        .get("X-App-Id")
        .cloned()
        .unwrap_or_else(|| "default".to_string());
    
    if req.headers.get("X-App-Id").is_none() {
        warn!(
            request_id = %request_id,
            "missing_app_id_using_default"
        );
    }   

    // 👤 USER ID (optional attribution)
    let base_user = req
        .headers
        .get("X-User-Id")
        .cloned()
        .unwrap_or_else(|| format!("{}:{}", api_key, ip));

    let user_id = format!("{}:{}", base_user, app_id);
      

    let limit = match api_keys.get(&api_key) {
        Some(l) => *l,
        None => {
            AUTH_FAILURES.fetch_add(1, Ordering::Relaxed);

            warn!(request_id = %request_id, "invalid_api_key");

            send_response(
                &mut client,
                "403 Forbidden",
                "Invalid API Key",
                &request_id,
                start
            ).await;
            return;
        }
    };

    // ---- STEP 5: Track request count ----
    
    let total_requests = {
        let map = usage_map.lock().unwrap();
        map.get(&user_id)
            .and_then(|routes| routes.get(&req.path))
            .map(|models| {
                models.values().map(|s| s.requests).sum::<u64>()
            })
            .unwrap_or(0)
    };

    info!(
        user_id = %user_id,
        route = %req.path,
        total_requests = %total_requests,
        "user_usage_updated"
    );
    // ---- STEP 6: Rate limiting ----

    if check_rate_limit(&api_key, &req.path, limit, &limiter).await {
        warn!(
            request_id = %request_id,
            path = %req.path,
            "rate_limited"
        );

        send_response(
            &mut client,
            "429 Too Many Requests",
            "",
            &request_id,
            start
        ).await;
        return;
    }
    GATEWAY_ACCEPTED.fetch_add(1, Ordering::Relaxed);

    // ---- STEP 6.5: Enforce daily cost limit ----

    let rows = db_client.query(
        "SELECT COALESCE(SUM(cost), 0)
        FROM usage_logs
        WHERE user_id = $1
        AND created_at > NOW() - INTERVAL '1 day'",
        &[&user_id]
    ).await;

    let current_cost: f64 = match rows {
        Ok(r) => {
            let val: f64 = r[0].get(0);
            val
        }
        Err(_) => 0.0,
    };

    if current_cost >= DAILY_LIMIT {
        let body = format!("Daily limit exceeded. Used: ${:.6}", current_cost);

        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );

        let _ = client.write_all(response.as_bytes()).await;
        return;
    }

    // ---- STEP 7: Route request to upstream ----
    let upstream_addr = {
        let mut map = balancers.lock().unwrap();

        let mut selected = None;

        for (route, lb) in map.iter_mut() {
            if req.path == *route || req.path.starts_with(&(route.clone() + "/")) {
                selected = lb.next();
                break;
            }
        }

        selected
    };

    let upstream_addr = match upstream_addr {
        Some(addr) => addr,
        None => {
            warn!(request_id = %request_id, "route_not_found");

            send_response(
                &mut client,
                "404 Not Found",
                "",
                &request_id,
                start
            ).await;
            return;
        }
    };
    info!(
        request_id = %request_id,
        upstream = %upstream_addr,
        "routing"
    );

    // ---- STEP 8: Connect to upstream (TLS or TCP) ----

    let mut upstream: Box<dyn IoStream> =
        if upstream_addr.ends_with(":443") {

            let host = upstream_addr.split(':').next().unwrap_or("");

            let stream = match connect_with_retry(upstream_addr.clone(), &request_id).await {
                Some(s) => s,
                None => {
                    error!(request_id = %request_id, "upstream_failed");

                    send_response(
                        &mut client,
                        "502 Bad Gateway",
                        "",
                        &request_id,
                        start
                    ).await;
                    return;
                }
            };

            let cx = NativeTlsConnector::builder().build().unwrap();
            let cx = TlsConnector::from(cx);

            match cx.connect(host, stream).await {
                Ok(tls_stream) => Box::new(tls_stream),
                Err(_) => {
                    error!(request_id = %request_id, "upstream_failed");

                    send_response(
                        &mut client,
                        "502 Bad Gateway",
                        "",
                        &request_id,
                        start
                    ).await;
                    return;
                }
            }

        } else {

            let stream = match connect_with_retry(upstream_addr.clone(), &request_id).await {
                Some(s) => s,
                None => {

                    {
                        let mut map = balancers.lock().unwrap();

                        for (route, lb) in map.iter_mut() {
                            if req.path.starts_with(route) {
                                lb.mark_unhealthy(&upstream_addr);
                                break;
                            }
                        }
                    }

                    error!(request_id = %request_id, "upstream_failed");

                    send_response(
                        &mut client,
                        "502 Bad Gateway",
                        "",
                        &request_id,
                        start
                    ).await;
                    return;
                }
            };

            Box::new(stream)
        };    

    let mut modified = buffer.clone();


    // ---- STEP 9: Forward request to upstream ----
    // Fix Host header + send body
    if let Some(headers_end) = modified.windows(4).position(|w| w == b"\r\n\r\n") {

        let mut headers = modified[..headers_end + 4].to_vec();

        let host = upstream_addr.split(':').next().unwrap_or("");

        // find and replace Host header IN-PLACE
        if let Some(pos) = headers
            .windows(6)
            .position(|w| w.eq_ignore_ascii_case(b"host: "))
        {
            // find end of that line
            if let Some(line_end) = headers[pos..]
                .windows(2)
                .position(|w| w == b"\r\n")
            {
                let end = pos + line_end;

                // replace Host line
                headers.splice(
                    pos..end,
                    format!("Host: {}", host).as_bytes().iter().cloned()
                );
            }
        }

        modified = headers;

    }

    // ---- STEP 10: Read upstream response ----
    // Handles:
    // - chunked encoding
    // - content-length fallback

    let header_end = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap() + 4;
    
    let content_length = req
        .headers
        .get("Content-Length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    let already_read = buffer.len() - header_end;

    let remaining = content_length.saturating_sub(already_read);

    if remaining > 0 {
        let mut remaining_buf = vec![0u8; remaining];

        if let Err(e) = client.read_exact(&mut remaining_buf).await {
            error!(request_id = %request_id, error = %e, "body_read_failed");
        }
        // ✅ IMPORTANT: add this line
        buffer.extend_from_slice(&remaining_buf);

        //upstream.write_all(&remaining_buf).await.unwrap();
    }

    // ---- Generate FULL body + cache key (after full read) ----
    if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
        body_str = String::from_utf8_lossy(&buffer[pos + 4..]).to_string();
    }

    let normalized_prompt = extract_and_normalize_prompt(&body_str);
    cache_key = generate_cache_key(&model, &normalized_prompt);

    info!("should_parse: {}", should_parse);
    info!("prompt_received len={}", normalized_prompt.len());
    info!("model: {}", model);
    info!("cache_key: {}", cache_key);

    // ---- CACHE CHECK ----
    if should_parse && !normalized_prompt.is_empty() && model != "unknown" {

        let cached = db_client.query(
            "SELECT response FROM prompt_cache 
            WHERE cache_key = $1", 
            &[&cache_key]
        ).await;

        if let Ok(rows) = cached {
            if !rows.is_empty() {

                let cached_response: Vec<u8> = rows[0].get(0);
                let _ = client.write_all(&cached_response).await;

                info!(
                    request_id = %request_id,
                    "cache_hit"
                );

                crate::db::insert_cache_hit(
                    &db_client,
                    &user_id,
                    &req.path,
                    &model,
                ).await;

                return;
            }
        }
    }

    let upstream_start = Instant::now();
    // send request
    upstream.write_all(&modified).await.unwrap();
    upstream.flush().await.unwrap();


    if buffer.len() > header_end {
        let already_read_body = &buffer[header_end..];
        upstream.write_all(already_read_body).await.unwrap();
    }

    let mut prompt_tokens: u64 = 0;
    let mut completion_tokens: u64 = 0;
    let mut total_tokens: u64 = 0;

    if should_parse {

        let mut response_buffer = Vec::new();
        let mut temp = [0u8; 1024];

        // 🔥 STEP 1: read response headers first
        loop {
            let n = upstream.read(&mut temp).await.unwrap();
            if n == 0 {
                break;
            }

            response_buffer.extend_from_slice(&temp[..n]);

            if response_buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        let headers_str = String::from_utf8_lossy(&response_buffer).to_string();

        // 🔥 detect chunked encoding
        let is_chunked = headers_str
            .to_lowercase()
            .contains("transfer-encoding: chunked");

        let header_end = response_buffer
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap() + 4;

        if is_chunked {

            // 🔥 capture already-read body
            let mut body = response_buffer[header_end..].to_vec();

        loop {
            let mut buf = [0u8; 1024];

            let n = upstream.read(&mut buf).await.unwrap();

            if n == 0 {
                break;
            }

            body.extend_from_slice(&buf[..n]);

            if body.windows(5).any(|w| w == b"0\r\n\r\n") {
                break;
            }
        }

        // 🔥 rebuild full response
        response_buffer.truncate(header_end);
        response_buffer.extend_from_slice(&body);
        
        } else {
            // fallback: content-length
            let content_length = headers_str
                .lines()
                .find(|l| l.to_lowercase().starts_with("content-length"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);

            let already_read = response_buffer.len() - header_end;
            let remaining = content_length.saturating_sub(already_read);

            if remaining > 0 {
                let mut body_buf = vec![0u8; remaining];
                upstream.read_exact(&mut body_buf).await.unwrap();
                response_buffer.extend_from_slice(&body_buf);
            }
        }


        // ---- STEP 11: Parse OpenAI response ----
        // Extract token usage from JSON
        // (only for /chat/completions)
        // ---- STEP 11: Parse OpenAI response ----
        println!("HEADERS:\n{}", headers_str);

        let is_gzip = headers_str
            .lines()
            .any(|l| l.to_lowercase().starts_with("content-encoding: gzip"));

        println!("IS GZIP: {}", is_gzip);

        if let Some(split_pos) = response_buffer
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
        {
            let body = &response_buffer[split_pos + 4..];

            let body_bytes = if is_gzip {
                let mut d = GzDecoder::new(body);
                let mut decompressed = Vec::new();

                match d.read_to_end(&mut decompressed) {
                    Ok(_) => decompressed,
                    Err(e) => {
                        println!("GZIP DECODE FAILED: {:?}", e);
                        println!("RAW BODY SIZE: {}", body.len());
                        Vec::new()
                    }
                }
            } else {
                body.to_vec()
            };

            if let Ok(body_str_raw) = std::str::from_utf8(&body_bytes) {

                let body_str = body_str_raw.to_string();
                // 🔥 DEBUG
                //println!("BODY DEBUG:\n{}\n----END----", body_str);

                // 🔥 extract JSON safely
                if let Some(start) = body_str.find('{') {
                    if let Some(end) = body_str.rfind('}') {
                    
                    let json_str = &body_str[start..=end];
                    let json_str = json_str.trim();

                    //println!("EXTRACTED JSON:\n{}\n----END JSON----", json_str);

                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str) {
                        
                        // ✅ FIX: extract model from response
                        if let Some(m) = json.get("model").and_then(|v| v.as_str()) {
                            model = m.to_string();
                        }

                        if let Some(usage) = json.get("usage") {

                            prompt_tokens = usage.get("prompt_tokens")
                                .and_then(|v| v.as_u64()).unwrap_or(0);

                            completion_tokens = usage.get("completion_tokens")
                                .and_then(|v| v.as_u64()).unwrap_or(0);

                            total_tokens = usage.get("total_tokens")
                                .and_then(|v| v.as_u64()).unwrap_or(0);

                            info!(
                                request_id = %request_id,
                                prompt_tokens = prompt_tokens,
                                completion_tokens = completion_tokens,
                                total_tokens = total_tokens,
                                "token_usage"
                            );
                        }

                    } else {
                        println!("JSON PARSE FAILED");
                    }

                } else {
                    println!("NO JSON END FOUND");
                }

            } else {
                println!("NO JSON START FOUND");
            }

        } else {
            println!("BODY NOT UTF-8");
        }
    }
        

        // 🚨 send EXACT same bytes (do not modify)
        let _ = client.write_all(&response_buffer).await;

        // ---- CACHE STORE ----
        if should_parse && !body_str.is_empty() && model != "unknown" {

            let db = db_client.clone();
            let cache_key_clone = cache_key.clone();
            let response_bytes = response_buffer.clone();

            tokio::spawn(async move {
                let _ = db.execute(
                    "INSERT INTO prompt_cache (cache_key, response) 
                    VALUES ($1, $2)
                    ON CONFLICT (cache_key) DO NOTHING",
                    &[&cache_key_clone, &response_bytes]
                ).await;
            });
        }

        upstream_status_code = 200;

    } else {

        match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
            Ok(_) => {
                upstream_status_code = 200;
            }
            Err(e) => {
                upstream_status_code = 0;
                error!(request_id = %request_id, error = %e, "upstream_stream_error");
            }
        }
    }


    upstream_latency_ms = upstream_start.elapsed().as_millis();
    
    // close client connection cleanly
    let _ = client.shutdown().await;
    
    // ---- STEP 13: Update usage stats ----
    // latency, errors, cost

    // 🔥 calculate cost once (outside block)
    let (input_price, output_price) = get_model_price(&model);

    let cost =
        (prompt_tokens as f64 / 1000.0) * input_price +
        (completion_tokens as f64 / 1000.0) * output_price;
    
    {
        let mut map = usage_map.lock().unwrap();

        let user_entry = map.entry(user_id.clone()).or_default();
        let route_entry = user_entry.entry(req.path.clone()).or_default();
        let model_entry = route_entry.entry(model.clone()).or_default();

        // latency
        model_entry.total_latency += upstream_latency_ms;

        // errors
        if upstream_status_code >= 400 {
            model_entry.errors += 1;
            UPSTREAM_FAILURES.fetch_add(1, Ordering::Relaxed);
        } else {
            UPSTREAM_SUCCESS.fetch_add(1, Ordering::Relaxed);
        }

        model_entry.total_cost += cost;

        // 🔥 TOKENS
        model_entry.prompt_tokens += prompt_tokens;
        model_entry.completion_tokens += completion_tokens;
        model_entry.total_tokens += total_tokens;

        // 🔥 requests count (IMPORTANT: move it here ideally)
        model_entry.requests += 1;
    }

    // ---- STEP 13.5: Save to DB (async, non-blocking) ----
    let db = db_client.clone();
    let user_id_clone = user_id.clone();
    let route = req.path.clone();
    let model_clone = model.clone();
    let cost_copy = cost;

    tokio::spawn(async move {
        crate::db::insert_usage(
            &*db,
            &user_id_clone,
            &route,
            &model_clone,
            prompt_tokens as i64,
            completion_tokens as i64,
            total_tokens as i64,
            // same cost you calculated above
            cost_copy,
            upstream_latency_ms as i64,
            upstream_status_code as i32,
        ).await;
    });

    // ---- STEP 14: Final request log ----
    log_request(
        &req,
        start,
        &request_id,
        upstream_status_code,
        upstream_latency_ms,
        &api_key,
        &user_id,
    );

}

async fn health_checker(balancers: Balancers) {
    loop {

        // STEP 1: collect all backend addresses
        let backends: Vec<(String, String)> = {
            let map = balancers.lock().unwrap();

            let mut list = Vec::new();

            for (path, lb) in map.iter() {
                for backend in &lb.backends {
                    list.push((path.clone(), backend.addr.clone()));
                }
            }

            list
        }; // mutex released here


        // STEP 2: check each backend
        for (path, addr) in backends {

            let result = timeout(
                Duration::from_secs(1),
                TcpStream::connect(addr.clone())
            ).await;


            // STEP 3: update health
            let mut map = balancers.lock().unwrap();

            if let Some(lb) = map.get_mut(&path) {
                for backend in &mut lb.backends {
                    if backend.addr == addr {
                        backend.healthy = result.is_ok();
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::main]
async fn main() {
    init_logging();
    let usage_map: UsageMap = Arc::new(Mutex::new(HashMap::new()));

    let api_keys = load_api_keys();
    
    let limiter: RateLimiter = Arc::new(Mutex::new(HashMap::new()));

    let balancers: Balancers = Arc::new(Mutex::new(HashMap::new()));
    
    let db_client = Arc::new(connect_db().await);

    // start background health checker
    {
        let balancers_clone = balancers.clone();

        tokio::spawn(async move {
            health_checker(balancers_clone).await;
        });
    }

    {
        let mut map = balancers.lock().unwrap();

        map.insert(
            "/test".to_string(),
            LoadBalancer::new(vec![
                "127.0.0.1:9002".to_string(),
                "127.0.0.1:9003".to_string(),
            ]),
        );

        map.insert(
            "/local".to_string(),
            LoadBalancer::new(vec![
                "127.0.0.1:9001".to_string(),
            ]),
        );

        map.insert(
            "/v1".to_string(),
            LoadBalancer::new(vec![
                "api.openai.com:443".to_string(),
            ]),
        );
    }

    let listener = TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("failed to bind");

    println!("AI Gateway v{} running on 0.0.0.0:8080", VERSION);

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                tokio::spawn({
                    let api_keys = api_keys.clone();
                    let limiter = limiter.clone();
                    let balancers = balancers.clone();
                    let usage_map = usage_map.clone();
                    let db_client = db_client.clone(); 

                    async move {
                        handle_client(stream, limiter, balancers, api_keys, usage_map, db_client).await;
                    }

                });
            }
            Err(e) => eprintln!("connection error: {}", e),
        }
    }
}