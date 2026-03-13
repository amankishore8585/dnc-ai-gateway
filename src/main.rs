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

trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

const DEBUG: bool = false;

type Balancers = Arc<Mutex<HashMap<String, LoadBalancer>>>;

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

async fn read_request(client: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut temp = [0; 1024];

    loop {
        let n = client.read(&mut temp).await.expect("read failed");

        if n == 0 {
            break;
        }

        buffer.extend_from_slice(&temp[..n]);

        // limit request size
        if buffer.len() > 8192 {
            break;
        }

        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buffer).to_string()
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

async fn authenticate(req: &Request, client: &mut TcpStream) -> Option<String> {
    match req.headers.get("X-API-Key") {
        Some(key) => Some(key.to_string()),
        None => {
            AUTH_FAILURES.fetch_add(1, Ordering::Relaxed);

            let response =
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 15\r\n\r\nMissing API Key";

            client.write_all(response).await.ok();
            None
        }
    }
}

async fn check_api_key(
    api_key: &str,
    api_keys: &ApiKeys,
    client: &mut TcpStream,
) -> Option<f64> {

    match api_keys.get(api_key) {
        Some(limit) => Some(*limit),
        None => {
            let response =
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";

            client.write_all(response).await.ok();
            None
        }
    }
}

async fn check_rate_limit(
    api_key: &str,
    path: &str,
    limit: f64,
    limiter: &RateLimiter,
    client: &mut TcpStream,
) -> bool {

    let allowed = {
        let mut map = limiter.lock().unwrap();

        let routes = map.entry(api_key.to_string()).or_insert_with(HashMap::new);

        let bucket = routes
            .entry(path.to_string())
            .or_insert_with(|| TokenBucket::new(limit, limit));

        let allowed = bucket.allow();

        allowed
    }; // mutex dropped here

    if !allowed {
        RATE_LIMITED.fetch_add(1, Ordering::Relaxed);

        let response =
            b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n";

        client.write_all(response).await.ok();
        return true;
    }

    false
}

fn log_request(req: &Request, start: Instant) {
    if DEBUG {
        let duration = start.elapsed();

        println!(
            "REQUEST method={} path={} duration_ms={}",
            req.method,
            req.path,
            duration.as_millis()
        );
    }
}

async fn handle_client(
    mut client: TcpStream,
    limiter: RateLimiter,
    balancers: Balancers,
    api_keys: ApiKeys
    ){

    let _ip = client.peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    //new feature
    let start = Instant::now();    
    
    // read request from client
    let request = match timeout(Duration::from_secs(5), read_request(&mut client)).await {
        Ok(req) => req,
        Err(_) => {
            let response =
                b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\n\r\n";
            client.write_all(response).await.ok();
            return;
        }
    };

    REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);

    
    //parsing
    let req = parse_request(&request);

    if DEBUG {
        println!("Incoming Request: {}", req.path);
    }

    //metrics
    if handle_metrics(&req, &mut client).await {
        return;
    }

    // 🔐 API KEY AUTHENTICATION
    let api_key = match authenticate(&req, &mut client).await {
        Some(k) => k,
        None => return,
    };

    let limit = match check_api_key(&api_key, &api_keys, &mut client).await {
        Some(l) => l,
        None => return,
    };

    if check_rate_limit(&api_key, &req.path, limit, &limiter, &mut client).await {
        return;
    }

    // -------- routing --------
    let upstream_addr = {
        let mut map = balancers.lock().unwrap();

        let mut selected = None;

        for (route, lb) in map.iter_mut() {
            if req.path.starts_with(route) {
                selected = lb.next();
                break;
            }
        }

        selected
    };

    let upstream_addr = match upstream_addr {
        Some(addr) => addr,
        None => {
            let response = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
            client.write_all(response).await.ok();
            return;
        }
    };

    if DEBUG {
        println!("Routing to upstream: {}", upstream_addr);
    }

    // connect to backend with timeout
    let mut upstream: Box<dyn IoStream> =
        if upstream_addr.ends_with(":443") {

            let host = upstream_addr.split(':').next().unwrap_or("");

            let stream = match timeout(
                Duration::from_secs(3),
                TcpStream::connect(upstream_addr.clone())
            ).await {
                Ok(Ok(s)) => s,
                _ => {
                    client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await.ok();
                    return;
                }
            };

            let cx = NativeTlsConnector::builder().build().unwrap();
            let cx = TlsConnector::from(cx);

            match cx.connect(host, stream).await {
                Ok(tls_stream) => Box::new(tls_stream),
                Err(_) => {
                    client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await.ok();
                    return;
                }
            }

        } else {

            match timeout(
                Duration::from_secs(3),
                TcpStream::connect(upstream_addr.clone())
            ).await {
                Ok(Ok(stream)) => Box::new(stream),
                _ => {

                    {
                        let mut map = balancers.lock().unwrap();

                        for (route, lb) in map.iter_mut() {
                            if req.path.starts_with(route) {
                                lb.mark_unhealthy(&upstream_addr);
                                break;
                            }
                        }
                    }

                    client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await.ok();
                    return;
                    }
                }
            };

    // forward request line
    let request_line = format!("{} {} {}\r\n", req.method, req.path, req.version);
    upstream.write_all(request_line.as_bytes()).await.unwrap();

    // forward headers but override Host
    for (k, v) in &req.headers {
        if k.eq_ignore_ascii_case("Host") {
            continue;
        }

        let line = format!("{}: {}\r\n", k, v);
        upstream.write_all(line.as_bytes()).await.unwrap();
    }

    // set Host based on upstream
    let host = upstream_addr.split(':').next().unwrap_or("");
    let host_line = format!("Host: {}\r\n", host);
    upstream.write_all(host_line.as_bytes()).await.unwrap();

    // ensure upstream closes connection
    upstream.write_all(b"Connection: close\r\n").await.unwrap();

    // end headers
    upstream.write_all(b"\r\n").await.unwrap();

    // full duplex streaming
    tokio::io::copy_bidirectional(&mut client, &mut upstream)
        .await
        .ok();
    
    // close client connection cleanly
    let _ = client.shutdown().await;    

    //for request log
    log_request(&req, start);
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