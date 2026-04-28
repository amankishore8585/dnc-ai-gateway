# DNC AI Gateway (Rust)

DNC AI Gateway is a lightweight infrastructure-layer proxy for AI APIs, focused on control, reliability, observability, and cost efficiency.

Instead of calling OpenAI (or other providers) directly from your app:

  App → OpenAI

You route through the gateway:

  App → AI Gateway → OpenAI

This enables:

  • centralized authentication and rate limiting  
  • intelligent response caching (reducing latency and cost)  
  • token-level usage tracking and cost estimation  
  • structured logging and observability  
  • controlled and reliable traffic routing  

All without changing your application logic.

## What Problem This Solves

AI APIs introduce new challenges in production:

* No centralized rate limiting → risk of abuse and cost spikes  
* Hard to track usage, latency, and token consumption  
* No visibility into real cost or per-user usage  
* Repeated identical requests → unnecessary latency and cost  
* No control over traffic routing (retries, load balancing, multiple backends)  
* No unified view of requests, latency, cache behavior, and failures — making debugging difficult across services  

This gateway solves these problems at the infrastructure layer, without requiring changes to application logic.

## Features (v2)

### 🔐 Backend Security & Control

#### API Key Authentication
* Validate requests using X-API-Key
* Centralized access control

#### Rate Limiting (Token Bucket)
* Per API key + per route
* Prevent abuse and control costs

#### Intelligent Routing
* Route based on path (/v1, /test, etc.)
* Built-in load balancer support

### ⚙️ Reliability & Resilience

#### Connection Retry
* Retries failed upstream connections
* Improves resilience under transient failures

#### Health Checking
* Background health checks for upstream servers
* Automatically avoids unhealthy backends

### ⚡ Full Protocol Support (Data Plane)

#### Streaming Proxy (Chunked Support)
* Supports chunked transfer encoding
* Handles large payloads safely
* Selective buffering for response parsing (token extraction & caching)

#### TLS Support
* Automatic HTTPS handling for upstream APIs

### 📊 Observability & Usage Insights

#### Per-User & Per-Route Stats
* Request counts
* Average latency
* Error rate

#### Cost Tracking (Token-Based)
* Cost calculated using actual token usage from responses
* Aggregated per user, route, and model

#### Structured Logging
* request_id
* latency
* upstream status (connection-level)
* user_id and api_key tracing

#### Metrics Endpoint (/metrics)
* requests_total
* auth_failures
* auth_success
* rate_limited
* gateway_accepted
* upstream_success (connection-level)
* upstream_failures (connection-level)

#### Cache Metrics
* Cache hits
* Cache hit rate
* Reduced latency for repeated requests

### ⚡ Intelligent Caching Layer

#### Response Caching (Prompt-Based)

* Caches full OpenAI responses based on:
    model (normalized) + normalized prompt
* Eliminates duplicate requests to upstream APIs
* Reduces latency and cost significantly

#### Cache Hit Optimization

* Cache hits return instantly (no upstream call)
* Transparent to client applications

#### Database-backed Cache

* Persistent storage using PostgreSQL
* Survives restarts

### 💰 Token Usage Tracking

#### Token Usage Extraction

* Parses OpenAI JSON responses
* Extracts:
  * prompt_tokens
  * completion_tokens
  * total_tokens

#### Accurate Cost Calculation

* Cost calculated per request using model pricing
* Aggregated per user and route

#### Stored in Database

* Usage logs persisted for analytics and billing

### Database Integration

#### PostgreSQL Integration

* Stores:
  * prompt_cache (cached responses)
  * usage_logs (token usage + cost)

* Enables:
  * Persistent caching
  * Historical usage tracking
  * Foundation for analytics & billing 

## Architecture

The gateway is structured as a layered infrastructure proxy, separating control, data flow, and observability



### Request Flow

Client → AI Gateway → AI Provider

Inside the gateway:
```text
Incoming Request
      ↓
Authentication (API Key)
      ↓
Rate Limiting
      ↓
Request Normalization (extract model + prompt)
      ↓
Cache Check (prompt-based)
      ↓
Routing + Load Balancing
      ↓
Upstream Connection (Retry + TLS)
      ↓
Upstream Response Received
      ↓
Response Normalization (dechunk / decode if needed)
      ↓
Token Extraction (usage + model)
      ↓
Cache Store (async)
      ↓
Response to Client
```
### System View
```text
Client Application
        |
        v
+-----------------------------+
|        AI Gateway           |
|-----------------------------|
| Auth & Rate Limiting        |
| Request Normalization       |
| Cache Layer (DB-backed)     |
| Routing & Load Balancing    |
| Retry & Health Checks       |
| Response Processing         |
|  - Dechunk / Decode         |
|  - Token Extraction         |
| Metrics & Logs              |
+-------------+---------------+
            |
            v
+-----------------------------+
|       AI Provider           |
|      (OpenAI API)           |
+-----------------------------+
```
### Design Note (v2)

* Do not modify request bodies or prompts
* Minimize header mutation (only required fields like Host)
* Selectively parse upstream responses (only for /chat/completions)
* Streaming-first design with selective buffering (for parsing and caching)
* Metrics include both connection-level and token-level insights
* Infra-level system — not a prompt router or model orchestrator
* Caching and usage tracking are transparent to client applications

### Scope

### Scope

This gateway is designed as an **infrastructure layer**, not an application-layer AI proxy.

It does not:
- modify prompts or responses
- perform model selection or orchestration

It does provide:
- response caching (transparent, prompt-based)
- token-level usage tracking and cost calculation

Its primary focus remains:
- traffic control
- security
- reliability
- observability

This makes it a strong foundation layer that can be combined with application-level tools such as LiteLLM or OpenRouter.

Example architecture:

App → LiteLLM / OpenRouter → AI Gateway → AI Provider

In this setup:
- LiteLLM/OpenRouter handle model logic, advanced routing, and application-level policies
- AI Gateway handles authentication, rate limiting, caching, usage tracking, routing, and logging

## Quick Start (Local)

### 1. Clone the repo

```
git clone https://github.com/amankishore8585/ai-gateway.git
```
```
cd ai-gateway
```

### 2. Add API keys

Create a file:

```
api_keys.json
```

Example:

```
{
  "user1": 2
}
```

### 3. Run the gateway

```
cargo run
```

Gateway will start on:

```
http://127.0.0.1:8080
```

---

### 4. Test the Gateway

#### 1. Missing API Key (should return 401)
```bash
curl -i http://127.0.0.1:8080/test
```

Expected:

- HTTP 401 Unauthorized

- Response includes X-Request-ID

- Logs show missing_api_key


#### 2. Invalid API Key (should return 403)
```bash  
  curl -i -H "X-API-Key: wrong_key" http://127.0.0.1:8080/test
```

Expected:

- HTTP 403 Forbidden

- Logs show invalid_api_key    


#### 3. Valid API Key (should succeed)
```bash  
  curl -i \
    -H "X-API-Key: user1" \
    -H "X-User-Id: user42" \
    http://127.0.0.1:8080/test
```

Expected:

- HTTP 200 OK or you may see a 502 error if no backend server is running. Run a simple server in another 2 other terminals 
```bash  
python3 -m http.server 9002
```    
```
python3 -m http.server 9003
```
Now retry

- Request is routed to backend


#### 4. Rate Limiting (should return 429)

```bash
  for i in {1..10}; do
    curl -s -o /dev/null -w "%{http_code}\n" \
    -H "X-API-Key: user1" http://127.0.0.1:8080/test
  done
```

Expected:

- Some requests return 429 Too Many Requests 
  (might need to start local backend at - python3 -m http.server 9002)

- Logs show rate_limited    


#### 5. OpenAI request through gateway:
```bash
  curl -i http://127.0.0.1:8080/v1/models \
  -H "Authorization: Bearer YOUR_OPENAI_API_KEY" \
  -H "X-API-Key: user1" \
  -H "X-User-Id: user42" \
  -H "X-App-Id: support"

```
(Replace YOUR_OPENAI_API_KEY with your actual key.)
  
Expected:

- It shows a list of all models in json format.Meaning its working properly.

- It might show error due to wrong api key. And you can see the error in ur logs. 

👤 User & Application Tracking

The gateway supports both user-level and application-level tracking:

🔹 X-User-Id
Represents the end user in your application.

If not provided, it falls back to:
```<api_key>:<client_ip>```

🔹 X-App-Id

Represents the application, service, or feature making the request.

Example:

- support → customer support chatbot
- search → internal search assistant
- analytics → reporting system

If not provided, the gateway falls back to:
```default```

#### 6. Check metrics

```
curl http://127.0.0.1:8080/metrics
```

Expected:

- total requests, authentication results, rate limiting, and  upstream connectivity

#### 7. In-Memory Stats

```
curl http://127.0.0.1:8080/stats | jq
```
(install jq for pretty JSON output if needed)

Example Output
```
{
  "user1:127.0.0.1:support": {
    "/v1/chat/completions": {
      "requests": 37,
      "avg_latency_ms": 1606,
      "errors": 0,
      "error_rate": 0.0,
      "cache hit rate": 10,
      "token usage" : 1024,
      "cost": 0.0024
    }
  }
}
```

#### 8. Database Stats

Basic
```
curl http://127.0.0.1:8080/stats-db
```

Filter by user
```
curl "http://localhost:8080/stats-db?user=user1:127.0.0.1"
```
Time ranges
```
curl "http://localhost:8080/stats-db?range=24h"
curl "http://localhost:8080/stats-db?range=7d"
curl "http://localhost:8080/stats-db?range=all"
```

Combined filters
```
curl "http://localhost:8080/stats-db?user=user1:127.0.0.1&range=24h"
```

## Using the Gateway

The AI Gateway sits between your application and AI providers, giving you centralized control over traffic, security, and observability.

Instead of calling OpenAI directly:
```App → OpenAI```

Route all requests through the gateway:
```App → AI Gateway → OpenAI```

**🧠 Typical Use Case**
Example: Chatbot Startup

A backend service sending requests to OpenAI can use the gateway to:

* control API access across services  
* enforce rate limits per client, user, or application  
* track usage, latency, and token consumption centrally  
* reduce cost using intelligent response caching  
* avoid exposing provider API keys across services  
* aggregate usage per user, route, and application  


### 1. Self-Hosted

  Run the gateway locally or on your own VPS for full control.
  ```
  cargo build --release
  ./target/release/ai_gateaway
  ```
  The gateway will be available at:

  http://127.0.0.1:8080

  Use this option if you want:

  - full control over configuration
  - local development and testing
  - persistent caching and usage tracking via your database
  - no dependency on external services

### 2. Managed Gateway (Early Access)

A managed version of the gateway is available for teams who want centralized control without managing infrastructure

📩 Access & onboarding:

Currently available via direct onboarding.
Contact:
```
dncsoftwarehelp@gmail.com
```
**What this includes**
* managed deployment
* centralized configuration
* usage tracking and observability
* response caching for cost and latency optimization
* controlled access and rate limiting

**Notes**
* designed for backend/service integration
* supports multi-user and multi-application tracking
* access is provisioned per team
* usage policies may apply


For production systems, self-hosting or managed gateway (via onboarding)  

## 🔌 Integration with Your Application

Applications use the gateway as a drop-in replacement for OpenAI endpoints.

Instead of:

https://api.openai.com/v1

Use:

http://your-gateway/v1

### 🔑 Required Headers

Each request must include:

* `X-API-Key` → gateway authentication  
* `Authorization: Bearer <OPENAI_API_KEY>` → upstream provider auth  

Optional but recommended:

* `X-User-Id` → end-user identification  
* `X-App-Id` → application / service identification  

#### 👤 User & Application Identification (Important)

The gateway supports **multi-dimensional tracking** using:

- `X-User-Id` → identifies the end user  
- `X-App-Id` → identifies the application or feature  

---

**X-User-Id (User Tracking)**

Represents the end user in your system.

How it works:

* If provided:
  * usage, latency, and cost are tracked per user  
* If not provided:
  * gateway falls back to:

``` id="int2"
<api_key>:<client_ip>
```
**X-App-Id (Application Tracking)**

Represents the application, service, or feature making the request.

Examples:

- support → customer support chatbot
- search → internal assistant
- analytics → reporting system

How it works:

-If provided:
 - stats are tracked per application
- If not provided:
 - gateway falls back to:
```default```

Example:
```
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer YOUR_OPENAI_API_KEY" \
  -H "X-API-Key: user1" \
  -H "X-User-Id: user42" \
  -H "X-App-Id: support"
```
**Why this matters**

Using X-User-Id and X-App-Id enables:

* per-user usage tracking
* per-application usage tracking
* accurate cost attribution
* better observability and debugging
* foundation for future rate limiting and policies


### Example Integration

Using the official OpenAI Python client.

#### Python
```
client = OpenAI(
    api_key=os.getenv("OPENAI_API_KEY"),
    base_url="http://127.0.0.1:8080/v1",
    default_headers={
        "X-API-Key": "user1",
        "X-User-Id": "user42",
        "X-App-Id": "support"
    }
)
```

#### JavaScript  
```
const client = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
  baseURL: "http://127.0.0.1:8080/v1",
  defaultHeaders: {
    "X-API-Key": "user1",
    "X-User-Id": "user42",
    "X-App-Id": "support"
  }
});
```


## Metrics
Metrics endpoint:

curl http://127.0.0.1:8080/metrics

Example output:

requests_total 458
auth_failures 415
auth_success 43
rate_limited 0
gateway_accepted 37
upstream_success 35
upstream_failures 2


## In Memory Stats

Stats endpoint:  

curl http://127.0.0.1:8080/stats | jq

Example Output:

{
  "user1:127.0.0.1": {
    "/v1/chat/completions": {
      "gpt-4o-mini": {
        "requests": 37,
        "avg_latency_ms": 1606,
        "errors": 0,
        "error_rate": 0.0,
        "cache_hits": 12,
        "cache_hit_rate": 0.32,
        "total_tokens": 1040,
        "total_cost": 0.0185
      }
    }
  }
}
    

## Benchmark

Example benchmark using `wrk`:

```
wrk -t4 -c100 -d30s \
-H "X-API-Key: user1" \
http://127.0.0.1:8080/test
```

Example result:

```
Requests/sec: ~60,000
```

---

## ⚠️ Current Limitations (v2)

* Partial HTTP-aware proxying
  * Selectively parses responses (for token usage and model extraction)
  * Full upstream status code handling is not yet integrated into metrics

* Retry is connection-level only
  * Does not retry full requests
  * No idempotency awareness

* Basic caching implementation
  * No TTL (time-based expiration)
  * No cache invalidation
  * Cache key does not include all parameters (e.g. temperature, top_p)

* No model-level routing
  * Routing is still path-based only

* No circuit breaker
  * Failing upstreams are retried but not dynamically isolated beyond basic health checks

* Limited streaming support
  * Streaming responses are not fully supported when parsing is enabled
  * Current design favors full-response processing (for caching and token extraction)

* No request deduplication (in-flight)
  * Identical concurrent requests are not coalesced

## 🚀 Future Improvements
* Full HTTP-aware proxying
  * Upstream status code parsing
  * Accurate success/failure metrics

* Full request retry
  * Idempotent-safe retry logic

* Advanced caching layer
  * TTL-based expiration
  * Parameter-aware cache keys
  * Cache invalidation strategies
  * In-flight request deduplication

* Model-aware routing
  * Route based on model, cost, or policy

* Circuit breaker
  * Automatic isolation of failing upstreams

* Streaming support (true passthrough mode)
  * Optional mode to bypass parsing for real-time streaming use cases

* Config-driven routing
  * YAML/JSON-based configuration

* Containerization
  * Docker support

* CLI interface
  * Easier local management and configuration

* Dashboard
  * Visualization of usage, cost, and performance


## License

Free for personal use
Commercial use requires permission

## 💬 Early Access / Setup Help

If you're building with AI APIs and want help integrating the gateway (auth, rate limiting, caching, usage tracking), feel free to reach out.

I'm open to helping early users get started and would love feedback from real-world use cases.

Contact: dncsoftwarehelp@gmail.com

