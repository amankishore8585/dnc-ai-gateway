const VERSION: &str = "0.1.0";

#[derive(Default)]
struct UserStats {
    requests: u64,
    total_latency: u128,
    errors: u64,
    total_cost: f64,
}

mod metrics;
mod rate_limiter;
mod load_balancer;
mod config;

use metrics::*;
use rate_limiter::{RateLimiter, TokenBucket};
use load_balancer::LoadBalancer;
use config::{ApiKeys, load_api_keys};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
type UsageMap = Arc<Mutex<HashMap<String, HashMap<String, UserStats>>>>;

use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, TcpListener};
use tokio::time::{timeout, Duration};
use tokio::io::{AsyncRead, AsyncWrite};

use tokio_native_tls::TlsConnector;
use native_tls::TlsConnector as NativeTlsConnector;
use tracing::{info, warn, error};

trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

type Balancers = Arc<Mutex<HashMap<String, LoadBalancer>>>;

fn get_model_price(model: &str) -> (f64, f64) {
    match model {
        "gpt-4o-mini" => (0.00015, 0.0006),
        "gpt-4o" => (0.005, 0.015),
        _ => (0.0, 0.0), // unknown model
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

async fn handle_client(
    mut client: TcpStream,
    limiter: RateLimiter,
    balancers: Balancers,
    api_keys: ApiKeys,
    usage_map: UsageMap
    ){
    use uuid::Uuid;

    let request_id = Uuid::new_v4().to_string();
    let start = Instant::now();
    
    let upstream_status_code: u16;
    let upstream_latency_ms: u128;

    info!(
    request_id = %request_id,
    "request_started"
    );   
    
    let mut buffer = Vec::new();
    let mut temp = [0u8; 1024];

    // ---- STEP 1: read headers only ----
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

    //parsing
    let request_str = String::from_utf8_lossy(&buffer);
    let req = parse_request(&request_str);

    let should_parse = req.path.contains("/chat/completions");

    let model = if req.path.contains("/chat/completions") {
        "gpt-4o-mini".to_string() // default for now
    } else {
        "unknown".to_string()
    };

    if req.path == "/stats" && req.method == "GET" {
        // 🔒 lock only briefly
        let json = {
            let map = usage_map.lock().unwrap();

            let result: HashMap<_, _> = map.iter().map(|(user, routes)| {
                
                let route_map: HashMap<_, _> = routes.iter().map(|(route, stats)| {
                
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
                        route.clone(),
                        serde_json::json!({
                            "requests": stats.requests,
                            "avg_latency_ms": avg_latency,
                            "errors": stats.errors,
                            "error_rate": error_rate,
                            "estimated_total_cost": stats.total_cost
                        })
                    )
                
                }).collect();
                
                (user.clone(), route_map) 

            }).collect();

            serde_json::to_string(&result).unwrap() // ✅ OUTSIDE map    
        
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

    // 🔐 API KEY AUTHENTICATION
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

    // 👤 USER ID (optional attribution)
    let user_id = req
        .headers
        .get("X-User-Id")
        .cloned()
        .unwrap_or_else(|| format!("{}:{}", api_key, ip));
    
      

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

    // count usage
    {
        let mut map = usage_map.lock().unwrap();
        let user_entry = map.entry(user_id.clone()).or_default();
        let route_entry = user_entry.entry(req.path.clone()).or_default();
        route_entry.requests += 1;
    }
    
    let total_requests = {
        let map = usage_map.lock().unwrap();
        map.get(&user_id)
            .and_then(|routes| routes.get(&req.path))
            .map(|stats| stats.requests)
            .unwrap_or(0)
    };

    info!(
        user_id = %user_id,
        route = %req.path,
        total_requests = %total_requests,
        "user_usage_updated"
    );

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

    // -------- routing --------
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

    // connect to backend with timeout
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

    // ---- find end of headers ----
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

    let upstream_start = Instant::now();
    // send request
    upstream.write_all(&modified).await.unwrap();
    upstream.flush().await.unwrap();

    // 🔥 send any body bytes that were already read
    let header_end = buffer.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;

    if buffer.len() > header_end {
        let already_read_body = &buffer[header_end..];
        upstream.write_all(already_read_body).await.unwrap();
    }

    // ✅ FIX: read remaining body using Content-Length (no hanging)
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

        upstream.write_all(&remaining_buf).await.unwrap();
    }

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

        let headers_str = String::from_utf8_lossy(&response_buffer);

        // 🔥 detect chunked encoding
        let is_chunked = headers_str
            .to_lowercase()
            .contains("transfer-encoding: chunked");

        let header_end = response_buffer
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap() + 4;

        if is_chunked {

            let mut body = Vec::new();

            loop {
                let mut buf = [0u8; 1024];

                let n = upstream.read(&mut buf).await.unwrap();

                if n == 0 {
                    break;
                }

                body.extend_from_slice(&buf[..n]);

                // 🔥 detect end of chunked stream
                if body.windows(5).any(|w| w == b"0\r\n\r\n") {
                    break;
                }
            }

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


        // ---- parse WITHOUT modifying ----
        if let Some(split_pos) = response_buffer
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
        {
            let body = &response_buffer[split_pos + 4..];

            if let Ok(body_str) = std::str::from_utf8(body) {

                // 🔥 DEBUG
                println!("BODY DEBUG:\n{}\n----END----", body_str);

                // 🔥 extract JSON safely
                if let Some(start) = body_str.find('{') {
                    if let Some(end) = body_str.rfind('}') {

                    let json_str = &body_str[start..=end];

                    println!("EXTRACTED JSON:\n{}\n----END JSON----", json_str);

                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str) {

                        if let Some(usage) = json.get("usage") {

                            let prompt_tokens = usage.get("prompt_tokens")
                                .and_then(|v| v.as_u64()).unwrap_or(0);

                            let completion_tokens = usage.get("completion_tokens")
                                .and_then(|v| v.as_u64()).unwrap_or(0);

                            let total_tokens = usage.get("total_tokens")
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
        }

        // 🚨 send EXACT same bytes (do not modify)
        let _ = client.write_all(&response_buffer).await;

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
    
    {
        let mut map = usage_map.lock().unwrap();
        if let Some(user_stats) = map.get_mut(&user_id) {
            if let Some(route_stats) = user_stats.get_mut(&req.path) {

                // latency
                route_stats.total_latency += upstream_latency_ms;

                // errors
                if upstream_status_code >= 400 {
                    route_stats.errors += 1;
                    UPSTREAM_FAILURES.fetch_add(1, Ordering::Relaxed);
                } else {
                    UPSTREAM_SUCCESS.fetch_add(1, Ordering::Relaxed);
                }

                // cost
                let estimated_cost = match model.as_str() {
                    "gpt-4o-mini" => 0.0005,
                    "gpt-4o" => 0.01,
                    _ => 0.0,
                };

                route_stats.total_cost += estimated_cost;
            }
        }
    }

    //for request log
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

                    async move {
                        handle_client(stream, limiter, balancers, api_keys, usage_map).await;
                    }

                });
            }
            Err(e) => eprintln!("connection error: {}", e),
        }
    }
}