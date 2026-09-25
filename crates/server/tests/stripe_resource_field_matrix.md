# Stripe invoice observation field matrix

This matrix records the current resource shapes used by the dormant personal invoice operation.
The examples are synthetic loopback fixtures; they are not claims about a connected Sotto account.

The field names and endpoint relationships were checked against the Stripe API reference with the
repository's Stripe documentation workflow on 24 September 2026. The client pins the outbound
version separately in `billing::STRIPE_API_VERSION`; this document does not change that pin.
The references used were [retrieve an invoice](https://docs.stripe.com/api/invoices/retrieve),
[retrieve invoice line items](https://docs.stripe.com/api/invoice-line-item/retrieve), and [list
invoice payments](https://docs.stripe.com/api/invoice-payment/list).

| Resource | Required fields used | Provenance and rejection rule |
| --- | --- | --- |
| `GET /v1/account` | `id`, `livemode` | Must equal the configured operator account and environment before resource reads continue. |
| `GET /v1/invoices/:id` | `id`, `customer`, `status`, `currency`, `amount_paid`, `amount_due`, `amount_overpaid`, `amount_paid_off_stripe`, `livemode`, `metadata.sotto_allocation_reference` | The path ID, paid status, mode, currency, full positive amount and trusted allocation claim are checked. Missing financial or metadata fields fail closed. |
| `GET /v1/invoices/:id/lines` | `id`, `quantity`, `livemode`, `parent.type`, `parent.subscription_item_details.subscription`, `parent.subscription_item_details.subscription_item`, `pricing.type`, `pricing.price_details.price`, `period.start`, `period.end` | The complete paginated list must contain exactly one subscription-item line with quantity one and a configured standard price. Invoice-item and other line types are unsupported. |
| `GET /v1/invoice_payments?invoice=:id` | `id`, `invoice`, `status`, `amount_paid`, `amount_requested`, `currency`, `livemode`, `payment.type`, `payment.payment_intent` | The complete list must contain exactly one paid PaymentIntent settlement. Open, canceled, multiple or mismatched records are ambiguous or unsupported. |

Stripe's current invoice line response includes `livemode`; the transport preserves it and the
assembly operation validates it against the authenticated environment. A missing line mode is a
malformed observation rather than an assumed test-mode value. The invoice payment list documents
`paid`, `open` and `canceled` statuses; the operation deliberately does not filter the list to
select a paid record.

The API reference shows invoice line items can be `invoice_item_details` as well as
`subscription_item_details`, and pricing has a type discriminator. Only the latter, with
`price_details`, is in this PR's supported personal-seat contract.
