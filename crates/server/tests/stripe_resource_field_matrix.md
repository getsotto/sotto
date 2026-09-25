# Stripe resource field matrix

This matrix records the current resource shapes used by the dormant Stripe read operations.
The examples are synthetic loopback fixtures; they are not claims about a connected Sotto account.

## Personal invoice observation

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

## Correction reads

These operations enumerate corrections for one parent and return typed resource observations.
They do not decide whether a correction affects coverage, and a completed enumeration is not a
consistent Stripe snapshot.

The object and list references were checked with `stripe docs api` on 25 September 2026:
[the refund object](https://docs.stripe.com/api/refunds/object),
[list refunds](https://docs.stripe.com/api/refunds/list),
[the dispute object](https://docs.stripe.com/api/disputes/object) and
[list disputes](https://docs.stripe.com/api/disputes/list). The API reference describes the
current version. The changelog lists no change to these resources after the repository's
`2026-07-29.dahlia` pin, so the documented shapes are expected to hold at that pin. That is
documentation evidence only: no sandbox has returned these objects at the pinned version.

Every list request carries its parent filter and `limit=100` on every page, with `starting_after`
set to the previous page's last ID. The shared session budget applies across all reads.

| Endpoint | Required and validated | Nullable, kept as absent | Parent and mode rule |
| --- | --- | --- | --- |
| `GET /v1/refunds?payment_intent=:id` | `object` (`refund`), `id`, `amount`, `currency`, `created` | `payment_intent`, `charge`, `status` | A present `payment_intent` naming another PaymentIntent is a parent mismatch. An absent one stays `None`; the filter is not treated as proof of ownership. Refunds document no `livemode`, so none is required; a present value must match the verified account. |
| `GET /v1/disputes?payment_intent=:id` | `object` (`dispute`), `id`, `charge`, `amount`, `currency`, `created`, `status`, `livemode` | `payment_intent` | A present `payment_intent` naming another PaymentIntent is a parent mismatch; an absent one stays `None`. `livemode` is required and must match the verified account. |

Amounts and `created` are integers and must not be negative; null is rejected rather than read as
zero. Currencies must be three lowercase letters. Expandable references are accepted as IDs or as
expanded objects with an `id`, and a present but malformed reference is rejected.

Refund statuses `pending`, `requires_action`, `succeeded`, `failed` and `canceled` stay distinct.
The documented null status is kept as absent, and any other lowercase token is kept as unknown, so
neither can read as a completed or absent refund. Dispute statuses `warning_needs_response`,
`warning_under_review`, `warning_closed`, `needs_response`, `under_review`, `won`, `lost` and
`prevented` stay distinct rather than collapsing into a disputed flag; a dispute status is
required, and an undocumented token is kept as unknown. Nothing is filtered by status.

The operations do not retain metadata, descriptions, reasons, receipt numbers, destination or
payment method details, dispute evidence, or any other free text.
