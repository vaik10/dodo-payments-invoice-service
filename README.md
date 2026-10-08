# Dodo Payments - Invoice & Payment Service

A minimal Invoice and Payment Service built in Rust (Axum), backed by PostgreSQL. It provides a robust, idempotency-guaranteed payment flow via a simulated Payment Service Provider (PSP).

## Demo Video Link:
- https://www.loom.com/share/72cf4131b0e741daa5a0c8302d76f508

## Features
- **API Key Authentication**: Secure, hashed API keys scoped to businesses.
- **Idempotency**: Prevents duplicate processing of payments using deterministic request fingerprinting.
- **Strict Concurrency**: Utilizes PostgreSQL row-level locks (`SELECT ... FOR UPDATE`) to guarantee zero double-charges during concurrent payment attempts.
- **Webhook Delivery**: Transactional outbox pattern ensures reliable event generation, processed by a background worker with exponential backoff.
- **Mock PSP**: Included mock payment processor validating external network failures and timeouts.

---

## Getting Started

### Prerequisites
- Docker & Docker Compose

### Run the Service
Bring up the entire stack (Postgres, Mock PSP, and Invoice Service) with a single command:

```bash
docker compose up --build
```

The services will be available at:
- **Invoice Service**: `http://localhost:8080`
- **Mock PSP**: `http://localhost:9090`
- **Database**: `postgres://dodo:dodo_secret@localhost:5432/dodo_payments`

---

## API cURL Examples

### 1. Bootstrapping a Business & API Key
Before creating resources, you need a business and an API Key.
*(Run these in a separate terminal while `docker compose up` is running).*

```bash
# Create a Business
curl -X POST http://localhost:8080/businesses \
  -H "Content-Type: application/json" \
  -d '{"name": "Acme Corp"}'

# Take the 'id' from the response above and generate an API key
curl -X POST http://localhost:8080/businesses/<BUSINESS_ID>/api-keys
```
*Note: Copy the `raw_key` from the response. You will use it as the Bearer token below.*

### 2. Create a Customer
```bash
curl -X POST http://localhost:8080/customers \
  -H "Authorization: Bearer <YOUR_RAW_KEY>" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Jane Doe",
    "email": "jane@example.com"
  }'
```

### 3. Create an Invoice
```bash
curl -X POST http://localhost:8080/invoices \
  -H "Authorization: Bearer <YOUR_RAW_KEY>" \
  -H "Content-Type: application/json" \
  -d '{
    "customer_id": "<CUSTOMER_ID>",
    "due_date": "2030-12-31",
    "line_items": [
      {
        "description": "Software License",
        "quantity": 1,
        "unit_amount_cents": 5000
      }
    ]
  }'
```
*(This returns an invoice in `draft` state. To pay it, you must finalize it first.)*

### 4. Finalize the Invoice
```bash
curl -X POST http://localhost:8080/invoices/<INVOICE_ID>/finalize \
  -H "Authorization: Bearer <YOUR_RAW_KEY>"
```
*(The invoice state is now `issued`.)*

### 5. Attempt a Successful Payment
```bash
curl -X POST http://localhost:8080/invoices/<INVOICE_ID>/pay \
  -H "Authorization: Bearer <YOUR_RAW_KEY>" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: req-12345" \
  -d '{
    "card_token": "tok_success"
  }'
```
*(This will return a `succeeded` status and transition the invoice to `paid`.)*

### 6. Attempt a Failing Payment (Declined)
*(Create and finalize a new invoice first)*
```bash
curl -X POST http://localhost:8080/invoices/<NEW_INVOICE_ID>/pay \
  -H "Authorization: Bearer <YOUR_RAW_KEY>" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: req-67890" \
  -d '{
    "card_token": "tok_card_declined"
  }'
```
*(This will return a `failed` status, and the invoice remains `issued`.)*

### 7. PSP Timeout (Outcome Unknown)
```bash
curl -X POST http://localhost:8080/invoices/<ANOTHER_INVOICE_ID>/pay \
  -H "Authorization: Bearer <YOUR_RAW_KEY>" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: req-timeout-1" \
  -d '{"card_token": "tok_timeout"}'
```
*(Returns `504` with `error: psp_timeout` and a `payment_attempt_id` after ~5s instead of hanging for the PSP's 30s. The invoice stays `issued`. `tok_network_error` behaves the same, but immediately.)*

### 8. Register a Webhook Endpoint
```bash
curl -X POST http://localhost:8080/webhook-endpoints \
  -H "Authorization: Bearer <YOUR_RAW_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"url": "https://webhook.site/<your-id>"}'
```
*(Save the `signing_secret`. It verifies the `Dodo-Signature` header on deliveries.)*

Full request/response shapes and the error format are in [openapi.yaml](./openapi.yaml).

---

## Running the Tests

The three mandatory tests (concurrency, idempotency, PSP failure) are in [`invoice-service/tests/payment_tests.rs`](./invoice-service/tests/payment_tests.rs). They need only Postgres. The PSP is replaced by a `wiremock` server, which lets the tests assert exactly how many charge calls reached it.

```bash
docker compose up -d postgres
DATABASE_URL=postgres://dodo:dodo_secret@localhost:5432/dodo_payments cargo test -p invoice-service
```

`#[sqlx::test]` creates a throwaway database per test and runs the migrations into it.

| Test | Asserts |
| --- | --- |
| `concurrent_payments_charge_exactly_once` | 8 concurrent `/pay` calls with distinct keys produce one `200` and seven `409`s, one `succeeded` attempt, an invoice in `paid`, and exactly one PSP call |
| `retried_payment_with_same_key_replays_response` | A same-key retry returns an identical body with no second PSP call; the same key with a different body returns `422 idempotency_key_mismatch` |
| `psp_timeout_leaves_invoice_issued` | A slow PSP returns `504` quickly; the invoice stays `issued` and the attempt is `pending_resolution` |
| `psp_network_error_leaves_invoice_issued` | Same as above for a PSP `500` |

---

## Demo Video

[▶️ Watch the Architecture & Demo Walkthrough](#) *(Placeholder for Loom video link)*

---

## Design and Architecture
Please see [DESIGN.md](./DESIGN.md) for a comprehensive breakdown of the database schema, invoice state machine, concurrency mechanisms, and failure-mode handling.

## AI Usage
Please see [AI_USAGE.md](./AI_USAGE.md) for transparency regarding AI assistance during this project.

