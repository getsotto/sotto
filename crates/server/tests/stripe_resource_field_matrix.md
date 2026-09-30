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
| `GET /v1/invoices/:id` | `id`, `customer`, `parent.type`, `parent.subscription_details.subscription`, `status`, `currency`, `amount_paid`, `amount_due`, `amount_overpaid`, `amount_paid_off_stripe`, `livemode`, `metadata.sotto_allocation_reference` | The path ID, nested subscription parent, paid status, mode, currency, full positive amount and trusted allocation claim are checked. Missing financial, parent or metadata fields fail closed. The deprecated top-level `subscription` field cannot establish association. |
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
[the dispute object](https://docs.stripe.com/api/disputes/object),
[list disputes](https://docs.stripe.com/api/disputes/list),
[the credit note object](https://docs.stripe.com/api/credit_notes/object) and
[list credit notes](https://docs.stripe.com/api/credit_notes/list). The API reference describes the
current version. The changelog lists no change to these resources after the repository's
`2026-07-29.dahlia` pin, so the documented shapes are expected to hold at that pin. That is
documentation evidence only: no sandbox has returned these objects at the pinned version.

Every list request carries its parent filter and `limit=100` on every page, with `starting_after`
set to the previous page's last ID. The shared session budget applies across all reads.

| Endpoint | Required and validated | Nullable, kept as absent | Parent and mode rule |
| --- | --- | --- | --- |
| `GET /v1/refunds?payment_intent=:id` | `object` (`refund`), `id`, `amount`, `currency`, `created` | `payment_intent`, `charge`, `status` | A present `payment_intent` naming another PaymentIntent is a parent mismatch. An absent one stays `None`; the filter is not treated as proof of ownership. Refunds document no `livemode`, so none is required; a present value must match the verified account. |
| `GET /v1/disputes?payment_intent=:id` | `object` (`dispute`), `id`, `charge`, `amount`, `currency`, `created`, `status`, `livemode` | `payment_intent` | A present `payment_intent` naming another PaymentIntent is a parent mismatch; an absent one stays `None`. `livemode` is required and must match the verified account. |
| `GET /v1/credit_notes?invoice=:id` | `object` (`credit_note`), `id`, `invoice`, `customer`, `amount`, `pre_payment_amount`, `post_payment_amount`, `currency`, `created`, `status`, `type`, `livemode` | none of the retained fields | `invoice` is required, so any value other than the requested invoice is a parent mismatch. `customer` is kept as evidence, not checked against a binding. `livemode` is required and must match the verified account. |

Amounts and `created` are integers and must not be negative; null is rejected rather than read as
zero. Currencies must be three lowercase letters. Expandable references are accepted as IDs or as
expanded objects with an `id`, and a present but malformed reference is rejected.

Refund statuses `pending`, `requires_action`, `succeeded`, `failed` and `canceled` stay distinct.
The documented null status is kept as absent, and any other lowercase token is kept as unknown, so
neither can read as a completed or absent refund. Dispute statuses `warning_needs_response`,
`warning_under_review`, `warning_closed`, `needs_response`, `under_review`, `won`, `lost` and
`prevented` stay distinct rather than collapsing into a disputed flag; a dispute status is
required, and an undocumented token is kept as unknown. Credit note statuses `issued` and `void`
stay distinct. The credit note `type` prose names `pre_payment` and `post_payment`, but its enum
also documents `mixed`; all three are kept, and any other token is unknown. Nothing is filtered by
status or type.

A credit note is not treated as a cash refund. Its pre-payment and post-payment amounts are kept
separately and are not added to refund amounts. The embedded `lines` and `refunds` on a credit
note are previews rather than complete lists, so they are not retained. Line enumeration, credit
allocation, charge reconciliation and deduplication between refunds and credit notes belong to a
later boundary.

The operations do not retain metadata, descriptions, reasons, receipt numbers, destination or
payment method details, dispute evidence, credit note memos, numbers or PDF links, or any other
free text.

## Personal invoice history

The bounded history operation lists `/v1/invoices` with both `subscription` and `customer` filters
on every page, then accounts for every returned invoice under the same request, page, record, byte
and deadline budgets. A paid invoice is reread before its correction evidence is assembled; the
listed and assembled headers must match. Draft, open, void and uncollectible invoices are retained
as non-entitlement observations. No invoice status establishes a failed renewal in this slice.

Stripe introduced `invoice.parent` in the Basil API family and deprecated the top-level subscription
fields. The operation requires `parent.type=subscription_details` and the nested subscription ID,
accepting an expanded object ID. Missing or unknown parent shapes become unresolved evidence, and
contradictory present IDs fail closed. Synthetic loopback fixtures exercise this contract; no
sandbox response has been observed at the repository's pinned API version.

## Personal renewal failure snapshot

The pure renewal decoder consumes the original signed `invoice.payment_failed` bytes and the
sealed history returned by the bounded reader. It makes no additional Stripe requests. The
snapshot fixture is synthetic and the inbound API-version allowlist is not evidence that every
shape is supported.

| Snapshot path | Required fields used | Provenance and rejection rule |
| --- | --- | --- |
| `event` | `id`, `created`, `api_version`, `type`, `livemode`, `data.object` | Signature is verified before JSON interpretation. The event must be `invoice.payment_failed`, have a nonnegative creation time, an allowlisted inbound version, direct operator context, and the configured mode. Account or Connect context is rejected. |
| `data.object` (invoice) | `object`, `id`, `customer`, `parent.type`, `parent.subscription_details.subscription`, `billing_reason`, `collection_method`, `status`, `currency`, `amount_due`, `amount_remaining`, `amount_paid`, `amount_overpaid`, `amount_paid_off_stripe`, `livemode`, `metadata.sotto_allocation_reference` | Requires an open, automatically collected `subscription_cycle` invoice in GBP with a positive amount due and remaining amount, no settlement or off-Stripe amount, and remaining equal to due. Nested parent, customer and metadata must match the trusted binding; legacy top-level subscription cannot fill a missing nested parent. |
| `data.object.lines` | `object`, `has_more`, `data[0]` | The embedded list must be complete (`has_more=false`) and contain exactly one line. A truncated list returns NeedsEvidence rather than selecting a partial result. |
| `data.object.lines.data[0]` | `id`, `invoice`, `livemode`, `quantity`, `parent.type`, `parent.subscription_item_details.subscription`, `parent.subscription_item_details.subscription_item`, `parent.subscription_item_details.proration`, `pricing.type`, `pricing.price_details.price`, `period.start`, `period.end` | The line must be a non-prorated subscription-item line with quantity one, a configured monthly or annual price, positive service duration, matching invoice and mode, and exact binding ownership. Missing proration proof is NeedsEvidence; true proration is unsupported. The line period, not invoice header timing, establishes the renewal boundary. |

The linked result preserves the failed invoice and line, the exact paid predecessor and its original
interval, provider account and mode, and a stable renewal identity. Event IDs remain separate so
duplicate retry deliveries can be recognised without changing renewal identity. A missing or
ambiguous exact predecessor returns NeedsEvidence; the decoder never claims current payment state
or converts the failure into coverage.
