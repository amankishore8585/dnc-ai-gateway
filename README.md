# DNC AI Gateway (Rust)

DNC AI Gateway is a lightweight infra-layer proxy for AI APIs, focused on control, reliability, and observability.

Instead of calling OpenAI (or other providers) directly from your app:

  App → OpenAI

You route through the gateway:

  App → AI Gateway → OpenAI

This lets you enforce policies, track usage, and manage traffic centrally.

## What Problem This Solves

AI APIs introduce new challenges in production:

* No centralized rate limiting → risk of abuse and cost spikes
* Hard to track usage and latency
* No visibility into estimated cost or per-user usage
* No control over traffic routing (retries, load balancing, multiple backends)
* No unified view of requests, latency, and failures — making debugging difficult across services

This gateway solves that at the infrastructure level, not the application level.

## Features (v1)

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

#### Streaming Proxy (Chunked + JSON Safe)
* Fully supports:
  * chunked requests
  * large payloads
  * streaming responses
* No buffering → low latency

#### TLS Support
* Automatic HTTPS handling for upstream APIs

### 📊 Aggregation & Usage Stats(/stats)

#### Per-User & Per-Route Stats
* Request counts
* Average latency
* Error rate

#### Cost Estimation
* Estimated usage cost per request
* Aggregated per user

### 📈 Observability

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


## Architecture

The gateway is structured as a layered infrastructure proxy, separating control, data flow, and observability

### Request Flow

Client → AI Gateway → AI Provider

Inside the gateway:

Incoming Request
      ↓
Authentication (API Key)
      ↓
Rate Limiting
      ↓
Routing + Load Balancing
      ↓
Upstream Connection (Retry + TLS)
      ↓
Streaming Proxy (bidirectional)
      ↓
Response to Client

### System View
```text
Client Application
        |
        v
+------------------------+
|       AI Gateway       |
|------------------------|
| Auth & Rate Limiting   |
| Routing & Load Balance |
| Retry & Health Checks  |
| Streaming Data Plane   |
| Aggregation            |
| Metrics & Logs         |
+-----------+------------+
            |
            v
+------------------------+
|      AI Provider       |
|    (OpenAI API)        |
+------------------------+
```
### Design Note (v1)

* Do not modify request bodies
* Minimize header mutation
* No upstream HTTP status parsing
* Stream everything. Streaming-first design (no buffering)
* Metrics reflect connection success, not API-level success
* Infra-level, not application-level. This is not a prompt router or model orchestrator.

### Scope

This gateway is designed as an **infrastructure layer**, not an application-layer AI proxy.

It does not:
- modify prompts or responses
- perform model selection
- handle caching or cost tracking

Instead, it focuses on:
- traffic control
- security
- reliability
- observability

This makes it a good foundation layer that can be combined with application-level tools such as LiteLLM or OpenRouter.

Example architecture:

App → LiteLLM / OpenRouter → AI Gateway → AI Provider

In this setup:
- LiteLLM/OpenRouter handle model logic, caching, and cost tracking
- AI Gateway handles authentication, rate limiting, routing, and logging

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
  -H "X-User-Id: user42"
```
(Replace YOUR_OPENAI_API_KEY with your actual key.)
  
Expected:

- It shows a list of all models in json format.Meaning its working properly.

- It might show error due to wrong api key. And you can see the error in ur logs. 

👤 Note on User Tracking

In the examples above, X-User-Id is included to demonstrate per-user tracking.
If not provided, the gateway will fall back to:
<api_key>:<client_ip>
For production usage, it is recommended to pass your application's user ID via X-User-Id.

#### 6. Check metrics

```
curl http://127.0.0.1:8080/metrics
```

Expected:

- total requests, authentication results, rate limiting, and  upstream connectivity

#### 7. Check usage stats

```
curl http://127.0.0.1:8080/stats | jq
```
(install jq for pretty JSON output if needed)

Example Output

{
  "user1:127.0.0.1": {
    "/v1/chat/completions": {
      "requests": 37,
      "avg_latency_ms": 1606,
      "errors": 0,
      "error_rate": 0.0,
      "estimated_total_cost": 0.0185
    }
  }
}

## Using the Gateway

The AI Gateway sits between your application and AI providers, giving you centralized control over traffic, security, and observability.

Instead of calling OpenAI directly:
  App → OpenAI

Route all requests through the gateway:
  App → AI Gateway → OpenAI

🧠 Typical Use Case
Example: Chatbot Startup

A backend service sending requests to OpenAI can use the gateway to:

* control API access across services
* enforce rate limits per client or user
* track usage and latency centrally
* avoid exposing provider API keys everywhere
* aggregate usage per user or route  


### 1. Self-Hosted

  Run the gateway locally or on your own VPS for full control.

  cargo build --release
  ./target/release/ai_gateaway

  The gateway will be available at:

  http://127.0.0.1:8080

  Use this option if you want:

  -full control over configuration
  -local development and testing
  -no dependency on external services

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
* controlled access and rate limiting

**Notes**
* designed for backend/service integration
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

* X-API-Key → gateway authentication
* Authorization: Bearer <OPENAI_API_KEY> → upstream provider auth
* X-User-Id → (optional) end-user identification

💡 **Important: Who sets `X-User-Id`?**

The `X-User-Id` header is **not sent by end users or frontend apps**.

It is typically added by your backend service using your own user system:

authenticated_user.id → X-User-Id → AI Gateway

This allows the gateway to track usage per user without requiring any changes in client applications.

👤 User Identification (Important)

The gateway supports per-user tracking using the X-User-Id header.

How it works
* If X-User-Id is provided:
  * stats are tracked per user
* If not provided:
  * gateway falls back to:

<api_key>:<client_ip>

How this is typically used

The X-User-Id header is set by your backend service, not by end users.
Your backend should pass your internal user identifier (e.g. database user ID) to the gateway:

your_app_user_id → X-User-Id → AI Gateway

This allows the gateway to track usage per user without requiring changes in client applications.

Example:
```
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer YOUR_OPENAI_API_KEY" \
  -H "X-API-Key: user1" \
  -H "X-User-Id: user42"
```
Why this matters

Using X-User-Id enables:

* per-user usage tracking
* cost attribution per user
* better observability
* future per-user rate limiting  

Recommended Pattern
In your backend:
  your_app_user_id → X-User-Id → AI Gateway

Example:
  user.id (database) → X-User-Id

### Example Integration

Using the official OpenAI Python client.

#### Python
```
client = OpenAI(
    api_key=os.getenv("OPENAI_API_KEY"),
    base_url="http://127.0.0.1:8080/v1",
    default_headers={
        "X-API-Key": "user1",
        "X-User-Id": "user42"
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
    "X-User-Id": "user42"
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

### Metric Definitions

🌐 Traffic
* requests_total
  Total number of incoming requests (including bots and invalid requests)

🔐 Authentication
* auth_success
  Requests with a valid API key
* auth_failures
  Requests rejected due to missing or invalid API key 

🚦 Rate Limiting
* rate_limited
  Requests rejected due to rate limiting

✅ Gateway Processing
* gateway_accepted
  Requests that passed authentication and rate limiting

🔌 Upstream Connectivity
* upstream_success
  Requests where the connection to the upstream API succeeded and data was streamed
* upstream_failures
  Requests where the upstream connection failed (timeout, TLS error, connection failure)

⚠️ Metrics Semantics (Important)
* upstream_success and upstream_failures are connection-level metrics, not HTTP-level.
* A request is considered successful if:
  * the connection to the upstream API succeeds
  * data is successfully streamed
* This means:
  * API-level errors (e.g. OpenAI returning 401/429/500) may still count as upstream_success
* Full HTTP status-based metrics will be added in a future version.

🧠 How to Interpret Metrics

You can derive useful insights:
* Bot traffic / noise
  auth_failures / requests_total

* Valid user traffic
  auth_success

* Accepted vs rejected requests
  gateway_accepted vs rate_limited

* Upstream health (connection-level)
  upstream_failures vs upstream_success

## Stats

Stats endpoint:  

curl http://127.0.0.1:8080/stats | jq

Example Output:

{
  "user1:127.0.0.1": {
    "/v1/chat/completions": {
      "requests": 37,
      "avg_latency_ms": 1606,
      "errors": 0,
      "error_rate": 0.0,
      "estimated_total_cost": 0.0185
    }
  }
}

### Stats Definitions

👤 User Scope
* Stats are grouped by:
  * user_id (if provided via X-User-Id)
  * fallback: api_key:ip

🛣️ Route Scope
Each user contains per-route metrics:
* /v1/chat/completions
* /test
 etc.

📈 Aggregated Fields
* requests
  Number of requests accepted by the gateway (after auth + rate limiting)
* avg_latency_ms
  Average upstream latency per route
* errors
  Number of upstream connection-level failures
* error_rate
  errors / requests
* estimated_total_cost
  Approximate cost based on model defaults

⚠️ Stats Semantics (Important)
* Stats are updated after requests pass authentication and rate      limiting
* Errors are connection-level, not HTTP-level:
  * API errors (401, 429, etc.) may not count as errors
* Cost estimation is:
  * approximate
  * based on static model pricing
  * not token-accurate

🧠 How to Interpret Stats

You can derive useful insights:
* Per-user usage
  requests per user

* Performance monitoring
  avg_latency_ms per route

* Reliability (connection-level)
  error_rate

* Cost tracking (approximate)
  estimated_total_cost    

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

## ⚠️ Current Limitations (v1)

* Connection-level proxying only
  * Does not parse upstream HTTP status codes
  * API-level errors (401, 429, 500) are not reflected in metrics
* Retry is connection-level only
  * Does not retry full requests
  * No idempotency awareness
* No request caching
* No model-level routing
  * Routing is path-based only
* No circuit breaker
  * Failing upstreams are retried but not dynamically isolated beyond basic health checks

## 🚀 Future Improvements
* HTTP-aware proxying
  * Upstream status code parsing
  * Accurate success/failure metrics
* Full request retry
  * Idempotent-safe retry logic
* Config-driven routing
  * YAML/JSON-based configuration
* Circuit breaker
  * Automatic isolation of failing upstreams
* Request caching layer
* Containerization
  * Docker support
* CLI interface
  * Easier local management and configuration
* Dashboard


## License

Personal project

## 💬 Early Access / Setup Help

If you're using AI APIs and want help setting up the gateway (auth, rate limiting, logging) for your app, feel free to reach out.

I'm open to helping early users get started and would love feedback from real use cases.

Contact: dncsoftwarehelp@gmail.com

