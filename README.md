# AI Gateway (Rust)

AI Gateway is a lightweight infra-layer proxy for AI APIs.

Instead of calling OpenAI (or other providers) directly from your app:

App → OpenAI

You route through the gateway:

App → AI Gateway → OpenAI

This lets you enforce policies, track usage, and manage traffic centrally.

## What Problem This Solves

AI APIs introduce new challenges in production:

* API keys exposed across services
* No centralized rate limiting
* Hard to track usage & latency
* No control over traffic routing
* No visibility into failures

This gateway solves that at the infrastructure level, not the application level.

## Core Features (v1)

### 🔐 API Key Authentication
* Validate requests using X-API-Key
* Centralized access control

### 🚦 Rate Limiting (Token Bucket)
* Per API key + per route
* Prevent abuse and control costs

### 🧭 Intelligent Routing
* Route based on path (/v1, /test, etc.)
* Built-in load balancer support

### 🔁 Connection Retry
* Retries failed upstream connections
* Improves resilience under transient failures

### 📊 Observability
* Request logging with:
  * request_id
  * latency
  * upstream status
* Structured logs for debugging

### ⚡ Streaming Proxy (Chunked + JSON Safe)
* Fully supports:
  * chunked requests
  * large payloads
  * streaming responses
* No buffering → low latency

### 🔐 TLS Support
* Automatic HTTPS handling for upstream APIs

### ❤️ Health Checking
* Background health checks for upstream servers
* Automatically avoids unhealthy backends

## Architecture

The gateway separates:

### Control Plane
* Auth
* Rate limiting
* Routing
* Logging

### Data Plane
* Raw TCP streaming
* Zero-copy forwarding (minimal mutation)

Client Application
        |
        v
+------------------+
|    AI Gateway    |
|------------------|
| API Key Auth     |
| Rate Limiting    |
| Load Balancer    |
| Health Checks    |
| Metrics & Logs   |
+---------+--------+
          |
          v
+------------------+
|   AI Provider    |
|   (OpenAI API)   |
+------------------+

## Design Philosophy

* Do not modify request bodies
* Minimize header mutation
* Stream everything
* Fail fast, log clearly
* Stay infra-level, not application-level

This is not a prompt router or model orchestrator.

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
  curl -i -H "X-API-Key: user1" http://127.0.0.1:8080/test
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
  -H "X-API-Key: user1"
```
(Replace YOUR_OPENAI_API_KEY with your actual key.)
  
Expected:

- It shows a list of all models in json format.Meaning its working properly.

- It might show error due to wrong api key. And you can see the error in ur logs. 

#### 6. Check metrics

```
curl http://127.0.0.1:8080/metrics
```

Expected:

- List of metrics -total req,failures,rate limited and successful req

## Using the Gateway

You can use the gateway in two ways:

### 1. Self-Hosted (Recommended)

  Run the gateway locally or on your own VPS for full control.

  cargo build --release
  ./target/release/ai_gateaway

  The gateway will be available at:

  http://127.0.0.1:8080

  Use this option if you want:

  -full control over configuration
  -local development and testing
  -no dependency on external services

### 2. Hosted Gateway (Experimental)

A hosted instance is available for quick testing:

https://dncgateway.com/v1

⚠️ Note:
This is an experimental deployment intended for evaluation only.
It is not production-ready and may have limits or downtime.

Use this option if you want:

- Quick testing without setup
- Temporary access for experimentation

For production use, self-hosting is recommended.  

## Using the Gateway from an Application

Applications can call the gateway instead of calling the OpenAI API directly.

Depending on your setup, this can be:

- your local gateway (http://127.0.0.1:8080)

- your own deployed instance

- or the hosted gateway (https://dncgateway.com/v1)

The gateway forwards requests to the backend AI provider while applying authentication and rate limiting

Clients must include the `X-API-Key` header.

### Python Example

Using the official OpenAI Python client.

### Option 1: Self-hosted
  client = OpenAI(
      api_key=os.getenv("OPENAI_API_KEY"),
      base_url="http://127.0.0.1:8080/v1",
      default_headers={
          "X-API-Key": "user1"
      }
  )

### Option 2: Hosted  
  client = OpenAI(
      api_key=os.getenv("OPENAI_API_KEY"),
      base_url="https://dncgateway.com/v1",
      default_headers={
          "X-API-Key": "user1"
      }
  )

### JavaScript Example

### Option 1: Self-hosted
const client = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
  baseURL: "http://127.0.0.1:8080/v1",
  defaultHeaders: {
    "X-API-Key": "user1"
  }
});

### Option 2; Hosted
const client = new OpenAI({
  apiKey: process.env.OPENAI_API_KEY,
  baseURL: "https://dncgateway.com/v1",
  defaultHeaders: {
    "X-API-Key": "user1"
  }
});


## Metrics
Metrics endpoint:

curl http://127.0.0.1:8080/metrics

Example output:

requests_total 230
auth_failures 12
rate_limited 3
successful_requests 180


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

* Retry is connection-level only (not full request retry)
* No request caching
* No model-level routing
* No circuit breaker (yet)

## Future Improvements

* Config-driven routing (YAML/JSON)
* Full request retry (idempotent safe)
* Better upstream status parsing
* Docker support
* CLI interface


## License

Personal project

## Contact

For questions, feedback, or collaboration

dncsoftwarehelp@gmail.com
