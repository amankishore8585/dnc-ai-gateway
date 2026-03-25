const VERSION: &str = "0.1.0";

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

fn log_request(req: &Request, start: Instant, request_id: &str,upstream_status: u16,upstream_latency: u128) {
    let duration = start.elapsed();

    info!(
        request_id = %request_id,
        method = %req.method,
        path = %req.path,
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
    api_keys: ApiKeys
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
        Some(k) => k,
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

    let limit = match api_keys.get(&api_key) {
        Some(l) => *l,
        None => {
            AUTH_FAILURES.fetch_add(1, Ordering::Relaxed);

            warn!(request_id = %request_id, "invalid_api_key");

            send_response(
                &mut client,
                "403 Forbidden",
                "",
                &request_id,
                start
            ).await;
            return;
        }
    };

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
    SUCCESSFUL_REQUESTS.fetch_add(1, Ordering::Relaxed);

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

        // split headers and body
        let (headers, _body) = modified.split_at(headers_end + 4);

        let headers_str = String::from_utf8_lossy(headers);

        let host = upstream_addr.split(':').next().unwrap_or("");

        // rebuild headers with correct Host
        let mut new_headers = Vec::new();

        for line in headers.split(|&b| b == b'\n') {
            if line.starts_with(b"Host:") || line.starts_with(b"host:") {
                new_headers.extend_from_slice(format!("Host: {}\r\n", host).as_bytes());
            } else {
                new_headers.extend_from_slice(line);
                new_headers.extend_from_slice(b"\n");
            }
        }

        // rebuild full request
        let new_request = new_headers;

        modified = new_request;
    }

    let upstream_start = Instant::now();
    // send request
    upstream.write_all(&modified).await.unwrap();
    upstream.flush().await.unwrap();

    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((_from_client, _from_upstream)) => {
            upstream_status_code = 200;
            info!(
                request_id = %request_id,
                upstream_status = 200,
                "upstream_success"
            );
        }
        Err(e) => {
            upstream_status_code = 0;
            error!(
                request_id = %request_id,
                error = %e,
                "upstream_stream_error"
            );
        }
    }


    upstream_latency_ms = upstream_start.elapsed().as_millis();
    
    // close client connection cleanly
    let _ = client.shutdown().await;    

    //for request log
    log_request(&req, start, &request_id, upstream_status_code,upstream_latency_ms);
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

                    async move {
                        handle_client(stream, limiter, balancers, api_keys).await;
                    }

                });
            }
            Err(e) => eprintln!("connection error: {}", e),
        }
    }
}