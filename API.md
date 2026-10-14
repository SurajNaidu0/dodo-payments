# API Reference

Base URL: `http://localhost:8080`. All bodies are JSON.

## Authentication

Every endpoint except `/health` needs an API key:

```
Authorization: Bearer sk_live_...
```

A key only sees its own business's data. Another business's resources return 404.

## Errors

Every error has the same shape:

```json
{
  "error": {
    "code": "invalid_state_transition",
    "message": "Cannot void an invoice in state 'paid'.",
    "details": { "current_state": "paid", "action": "void" }
  }
}
```

`details` is present only when there is something useful to add.

| HTTP | code | When |
|---|---|---|
| 400 | `invalid_body`, `invalid_query`, `invalid_path`, `invalid_request` | Malformed or invalid input, including fractional amounts |
| 400 | `idempotency_key_required` | `POST /pay` without an `Idempotency-Key` header |
| 400 | `unsupported_currency` | Currency other than USD |
| 401 | `unauthorized` | Missing, wrong or revoked API key |
| 402 | `payment_failed` | The PSP declined the card. `details.decline_code` says why. |
| 404 | `resource_not_found`, `route_not_found` | Unknown resource or route |
| 409 | `customer_already_exists` | `POST /customers` with an email (case-insensitive) the business already uses. `details.customer_id` is the existing customer. |
| 409 | `invalid_state_transition` | The action is not allowed from the invoice's current state |
| 409 | `payment_in_progress` | The invoice is `locked` by an in-flight payment. Retry later. |
| 409 | `invoice_already_paid` | `POST /pay` on a paid invoice |
| 409 | `request_in_progress` | Another request with the same `Idempotency-Key` is still running |
| 422 | `idempotency_key_reused` | The key was used before with a different request |
| 422 | `amount_too_large` | The total does not fit in 64-bit cents |
| 500 | `internal_error` | A bug or outage on our side |
| 502 | `psp_unavailable` | The PSP never received the charge. No money moved. Retry with a new key. |

## Lists

List endpoints return:

```json
{ "object": "list", "data": [ ... ], "has_more": true }
```

Use `?limit=` (1 to 100, default 20) and `?starting_after=<last id you saw>`. Newest items come first.

---

## Customers

### POST /customers

Request:
```json
{ "name": "Ada Lovelace", "email": "ada@example.com" }
```

Response `201`:
```json
{ "id": "uuid", "object": "customer", "name": "Ada Lovelace", "email": "ada@example.com", "created_at": "2026-10-13T12:00:00Z" }
```

Emails are unique per business, ignoring case. A duplicate returns `409 customer_already_exists`.

### GET /customers/{id}

Returns `200` with the customer object.

### GET /customers

Returns `200` with a list of customers.

---

## Invoices

### Invoice object

```json
{
  "id": "uuid",
  "object": "invoice",
  "customer_id": "uuid",
  "state": "open",
  "total_cents": 5999,
  "currency": "USD",
  "due_date": "2026-11-30",
  "line_items": [
    { "id": "uuid", "description": "Pro plan", "quantity": 2, "unit_amount_cents": 2500, "amount_cents": 5000 }
  ],
  "created_at": "...",
  "updated_at": "..."
}
```

`state` is one of:

| State | Meaning |
|---|---|
| `open` | Waiting for payment |
| `locked` | A payment attempt is in flight. Void, mark uncollectible and new payments all return 409. |
| `uncollectible` | Written off by the business. It can still be paid or voided. |
| `paid` | Terminal |
| `void` | Terminal |

### POST /invoices

Request:
```json
{
  "customer_id": "uuid",
  "due_date": "2026-11-30",
  "currency": "USD",
  "line_items": [
    { "description": "Pro plan", "quantity": 2, "unit_amount_cents": 2500 }
  ]
}
```

- `currency` is optional and must be `USD`.
- `quantity` must be a positive integer. `unit_amount_cents` must be a non-negative integer.
- The server computes each `amount_cents` and the `total_cents`. Any total in the request is ignored.

Response `201`: the invoice. Emits `invoice.created`.

### GET /invoices/{id}

Returns `200` with the invoice.

### GET /invoices

Optional filter `?state=open`. Returns `200` with a list of invoices.

### POST /invoices/{id}/void

Allowed from `open` and `uncollectible`. Returns `200` with the invoice. Emits `invoice.voided`.

### POST /invoices/{id}/mark_uncollectible

Allowed from `open`. Returns `200` with the invoice. Emits `invoice.marked_uncollectible`.

---

## Payments

### POST /invoices/{id}/pay

Headers: `Idempotency-Key: <1 to 255 chars>` (required).

Request:
```json
{ "card_token": "tok_success" }
```

Allowed from `open` and `uncollectible`.

| Response | Meaning | Body |
|---|---|---|
| `200` | Paid | `{ "payment_attempt": {...}, "invoice": {...} }` |
| `202` | The PSP did not answer in time. The outcome is not known yet. The invoice stays `locked`. | `{ "payment_attempt": { "status": "pending", ... }, "invoice": {...} }` |
| `402` | Declined. The invoice returns to its previous state. | error `payment_failed` with `details.payment_attempt` |
| `409` | Wrong state or a payment already in flight | error |
| `502` | The PSP never received the charge | error `psp_unavailable` |

After a `202`, find out the result in any of these ways:
- `GET /payment_attempts/{id}`,
- the `invoice.paid` or `invoice.payment_failed` webhook,
- retrying the same request with the same `Idempotency-Key`.

**Idempotency.** The same key with the same path and body returns the same result
and never calls the PSP twice. The same key with a different request returns `422`.

### Payment attempt object

```json
{
  "id": "uuid",
  "object": "payment_attempt",
  "invoice_id": "uuid",
  "amount_cents": 5999,
  "status": "succeeded",
  "failure_code": null,
  "psp_ref": "uuid",
  "created_at": "...",
  "updated_at": "..."
}
```

`status` is `pending`, `succeeded`, `failed` or `abandoned`.
`abandoned` means the PSP never received the charge.

### GET /payment_attempts/{id}

Returns `200` with the payment attempt.

---

## Webhooks

### POST /webhook_endpoints

Request:
```json
{ "url": "https://example.com/hooks/dodo" }
```

Response `201`:
```json
{ "id": "uuid", "object": "webhook_endpoint", "url": "...", "enabled": true, "secret": "whsec_...", "created_at": "..." }
```

The `secret` is returned only in this response.

### GET /webhook_endpoints

Returns `200` with a list of endpoints, without secrets.

### GET /webhook_deliveries

Optional `?status=pending|succeeded|dead`. Each item shows the event type, URL,
status, attempt count, next attempt time, and the last response or error.

### GET /events

Optional `?after=<event id>`. Returns events oldest first. Use this to catch up on
webhooks you missed: store the last event id you processed and page forward from it.

### Event types

`invoice.created`, `invoice.paid`, `invoice.payment_failed`, `invoice.voided`, `invoice.marked_uncollectible`.

### Delivery format

```
POST <your url>
Content-Type: application/json
Dodo-Signature: t=1760360000,v1=<hex>
Dodo-Event-Id: <event id>
Dodo-Event-Type: invoice.paid
```

```json
{ "id": "event uuid", "type": "invoice.paid", "created_at": "...", "data": { "invoice": {...}, "payment_attempt": {...} } }
```

**Verifying.** Compute `HMAC-SHA256(secret, "<t>.<raw body>")` as hex and compare it to `v1`
in constant time. Reject the webhook if `t` is more than 5 minutes old. Deduplicate on `Dodo-Event-Id`.

**Retries.** Any non-2xx response or a timeout of 10 seconds counts as a failure. The retry delays are
30 s, 2 m, 10 m, 30 m, 1 h, 3 h, 6 h and 12 h, each with ±10% jitter. That is 9 attempts over about
23 hours. After that the delivery is marked `dead`. The event can still be fetched from `GET /events`.
