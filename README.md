# Invoice & Payment Service

A small invoicing and payment backend in Rust (Axum, sqlx, PostgreSQL).
Businesses create invoices for customers, customers pay them through a mock
payment processor (PSP), and businesses receive signed webhooks.

- **Design:** see `DESIGN.md`.
- **API reference:** see `API.md`.
- **AI usage:** see `AI_USAGE.md`.

## Run it

Requires Docker.

```bash
docker compose up --build
```

This starts three containers. Migrations run automatically when the API starts.

| Service | Port | What it is |
|---|---|---|
| `api` | 8080 | The invoice service, plus its reconciler and webhook dispatcher |
| `mock-psp` | 4000 | Mock payment processor, plus `/webhook-sink`, a demo webhook receiver |
| `postgres` | 5432 | PostgreSQL 16 |

A demo business is created at startup. Its API key is:

```
sk_live_demo_4f9a2c7e1b8d3f6a0c5e9b2d7f1a4c8e
```

To start from an empty database, run `docker compose down -v`.

## Curl examples

```bash
export KEY=sk_live_demo_4f9a2c7e1b8d3f6a0c5e9b2d7f1a4c8e
export API=http://localhost:8080
```

**1. Create a customer**

```bash
curl -s -X POST $API/customers \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"name": "Ada Lovelace", "email": "ada@example.com"}'
```

Copy the `id` from the response: `export CUSTOMER=<id>`.

**2. Create an invoice.** The server computes the total. Any total you send is ignored.

```bash
curl -s -X POST $API/invoices \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{
        "customer_id": "'$CUSTOMER'",
        "due_date": "2026-11-30",
        "line_items": [
          {"description": "Pro plan", "quantity": 2, "unit_amount_cents": 2500},
          {"description": "Setup fee", "quantity": 1, "unit_amount_cents": 999}
        ]
      }'
```

The response has `"total_cents": 5999` and `"state": "open"`. Run `export INVOICE=<id>`.

**3. Pay it successfully**

```bash
curl -s -X POST $API/invoices/$INVOICE/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-$INVOICE-1" \
  -d '{"card_token": "tok_success"}'
```

You get HTTP 200 and the invoice is `paid`. Sending the same command again
returns the identical response without charging again.

**4. A failed payment.** Create a second invoice as in step 2, then:

```bash
curl -s -X POST $API/invoices/$INVOICE2/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-$INVOICE2-1" \
  -d '{"card_token": "tok_card_declined"}'
```

You get HTTP 402 with `"code": "payment_failed"` and `"decline_code": "card_declined"`.
The invoice goes back to `open`, so the customer can retry with another card and a new key.

### Card tokens

| Token | Result |
|---|---|
| `tok_success` | 200, invoice paid |
| `tok_card_declined` / `tok_insufficient_funds` | 402, invoice back to its previous state |
| `tok_timeout` | 202 after 5 s, invoice `locked`. The reconciler marks it paid about a minute later. |
| `tok_network_error` | 202, invoice `locked`. The reconciler finds no charge and releases it. |

## Testing

### Automated tests

The tests need a running PostgreSQL. Each test creates its own fresh database
and deletes it afterwards, so they never touch your demo data.

```bash
docker compose up -d postgres
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/dodo
cargo test
```

| File | What it proves |
|---|---|
| `tests/concurrency.rs` | 25 concurrent `POST /pay` calls on one invoice: exactly one 200, the rest 409, one succeeded attempt, one PSP charge. |
| `tests/idempotency.rs` | Same key returns the identical response with no second PSP call. A reused key with a different body gives 422. A paid invoice gives 409. |
| `tests/psp_failure.rs` | `tok_timeout` answers 202 in about 1 s, stays locked, then the reconciler marks it paid. `tok_network_error` is released back to `open`. A declined late payment keeps an invoice `uncollectible`. |
| `tests/webhooks.rs` | Events are delivered by the dispatcher, not the request. Signatures verify. A failing endpoint is scheduled for retry. |
| unit tests in `src/` | State machine rules, overflow-checked money math, signing, retry schedule. |

Run one file with `cargo test --test concurrency`.
Tests use a 1 s PSP client timeout and a 3 s mock delay so they run quickly.
They call the reconciler and dispatcher directly instead of waiting for timers.

### Manual checks against the running stack

**Webhooks.** Register the mock PSP's sink as an endpoint, then make some payments:

```bash
curl -s -X POST $API/webhook_endpoints \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"url": "http://mock-psp:4000/webhook-sink"}'
```

Save the `secret` from the response. It is shown only once.

```bash
curl -s $API/webhook_deliveries -H "Authorization: Bearer $KEY"   # what we sent and how it went
curl -s http://localhost:4000/webhook-sink                        # what the receiver got
```

**Verify a signature yourself.** Take `t`, `v1` and `raw_body` from one webhook in
`/webhook-sink`. Save the raw body to `body.json` exactly as received. Then:

```bash
(printf '%s.' "$T"; cat body.json) | openssl dgst -sha256 -hmac "$SECRET"
```

The output must equal `v1`.

**Concurrent payments.** Create an invoice, then fire 20 payments at once:

```bash
seq 1 20 | xargs -P 20 -I{} curl -s -o /dev/null -w '%{http_code}\n' \
  -X POST $API/invoices/$INVOICE/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: race-{}" -d '{"card_token": "tok_success"}' | sort | uniq -c
```

Expect one `200` and nineteen `409`. `curl -s http://localhost:4000/stats` shows the
PSP charge count went up by exactly one.

**Timeout.** Pay a new invoice with `tok_timeout`. The call returns 202 after 5 seconds and
the invoice is `locked`. Within about 75 seconds it becomes `paid`.
Watch it happen with `docker compose logs -f api`.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | `postgres://postgres:postgres@localhost:5432/dodo` | Postgres connection |
| `PSP_URL` | `http://localhost:4000` | Mock PSP base URL |
| `PSP_TIMEOUT_MS` | `5000` | Give up on a PSP call after this long |
| `RECONCILE_INTERVAL_SECS` | `15` | How often the reconciler runs |
| `RECONCILE_AFTER_SECS` | `60` | How long an invoice must be locked before the reconciler checks it |
| `DISPATCH_INTERVAL_MS` | `1000` | How often the webhook dispatcher polls |
| `DEMO_API_KEY` | unset | Creates a demo business with this key at startup |
| `MOCK_TIMEOUT_SECS` | `30` | How long the mock PSP sleeps for `tok_timeout` |

## Project layout

```
src/
  bin/api.rs, bin/mock_psp.rs   entry points
  routes/                       HTTP handlers
  state_machine.rs              allowed transitions
  money.rs                      overflow-checked totals
  idempotency.rs                Idempotency-Key handling
  settlement.rs                 moving an invoice out of `locked`
  psp_client.rs                 calls to the PSP
  events.rs                     transactional outbox
  workers/reconciler.rs         resolves stuck payments
  workers/webhook_dispatcher.rs sends and retries webhooks
  webhook_signing.rs            HMAC-SHA256 signatures
  mock_psp/                     the mock PSP and webhook sink
migrations/0001_init.sql
tests/
```

## Demo Video

_Add your Loom or Drive link here. It must open without a login._
