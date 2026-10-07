// Share-link fetch. Same-origin relative path (a Vite dev proxy points `/shares` at the API in
// dev; production is same-origin), so the CSP stays `connect-src 'self'`.
//
// The GET burns a view server-side, so callers must fetch exactly once, on an explicit user action.

import { bytesToStandardB64, standardB64ToBytes } from "./base64";
import type { SecretEntry } from "./vault";

// Authed requests send the httpOnly session cookie. Same-origin (dev proxy) keeps CSP tight.
const CREDS: RequestInit = { credentials: "include" };

/**
 * The server could not be reached at all: the request never got an answer, as opposed to getting
 * an unwelcome one.
 *
 * Worth its own type because in a secrets manager the difference between those two is the whole
 * question a worried person is asking. The browser's own wording for this is "Failed to fetch",
 * which reads like the data failed rather than the connection, and someone looking at it has no
 * way to tell whether the service is down or their vault is broken. Only one of those is
 * frightening, and it is not the one that is happening.
 *
 * The reassurance is about the design, not about their machine. An earlier wording promised
 * secrets were "still encrypted on your device", which is a claim about a local copy that need
 * not exist: this fires on a fresh browser before anything has been downloaded. What is always
 * true is that the server never held anything readable, so that is what it says.
 */
export class ServerUnreachableError extends Error {
  constructor(cause: unknown) {
    super(
      "Could not reach the server. This is a connection problem and says nothing about your " +
        "data: Sotto encrypts secrets before they leave your device, so nothing readable is " +
        "stored anywhere else.",
    );
    this.name = "ServerUnreachableError";
    this.cause = cause;
  }
}

/**
 * `fetch`, with an unreachable server reported as one.
 *
 * `fetch` rejects only for network-level failures: an HTTP error, however unwelcome, resolves
 * normally. So every rejection caught here means no answer arrived rather than a bad one, and
 * the distinction needs no guessing.
 *
 * Deliberately not matched on the message, which is browser-specific and would rot: Chrome says
 * "Failed to fetch", Firefox "NetworkError when attempting to fetch resource", Node "fetch
 * failed". The type is the contract; the wording is not.
 */
/**
 * Read a response body, reporting a connection that died mid-answer as unreachable too.
 *
 * `request` below only covers the part up to the headers. A connection dropped after them
 * rejects here instead, in `json()` or `text()`, and would otherwise reach the user as the same
 * raw browser wording the wrapper exists to replace. The failure is identical from where they
 * are sitting, so the message should be as well.
 */
async function readBody<T>(read: () => Promise<T>): Promise<T> {
  try {
    return await read();
  } catch (cause) {
    if (cause instanceof Error && cause.name === "AbortError") {
      throw cause;
    }
    // A body that is present but malformed is the server misbehaving rather than the network,
    // and mislabelling it would send somebody to check their wifi over a server bug.
    if (cause instanceof SyntaxError) {
      throw new Error("The server sent a response this app could not read.");
    }
    throw new ServerUnreachableError(cause);
  }
}

async function request(path: string, init: RequestInit = CREDS): Promise<Response> {
  try {
    return await fetch(path, init);
  } catch (cause) {
    // An abort is the caller's own doing and already means something to them; only a genuine
    // network failure gets rewritten. Matched on the name alone: browsers reject with a
    // DOMException, but a polyfilled or cross-realm signal need not, and narrowing to that type
    // would relabel a deliberate cancellation as the server being unreachable.
    if (cause instanceof Error && cause.name === "AbortError") {
      throw cause;
    }
    throw new ServerUnreachableError(cause);
  }
}

async function authedJson<T>(path: string): Promise<T> {
  const resp = await request(path, CREDS);
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  return (await readBody(() => resp.json())) as T;
}

/// The current session's user, or `null` if not logged in.
export async function me(): Promise<{ userId: string } | null> {
  const resp = await request("/auth/me", CREDS);
  if (resp.status === 401) {
    return null;
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await readBody(() => resp.json())) as { user_id: string };
  return { userId: body.user_id };
}

export async function logout(): Promise<void> {
  const resp = await request("/auth/logout", { method: "POST", ...CREDS });
  if (!resp.ok) {
    // The session cookie is httpOnly, so only the server can clear it; report failure rather than
    // letting callers assume the session is gone.
    throw new Error(`server error (${resp.status})`);
  }
}

export type EligibilityState =
  | "free"
  | "pending_initial_payment"
  | "paid"
  | "renewal_recovery"
  | "export_only"
  | "expired"
  | "unavailable";

export interface EligibilityView {
  state: EligibilityState;
  accountInitialized: boolean;
  billingAvailable: boolean;
  paidThroughEpoch: number | null;
  recoveryUntilEpoch: number | null;
  exportUntilEpoch: number | null;
  actions: { setup: boolean; billing: boolean; export: boolean; revoke: boolean };
  payer: "personal" | null;
  deploymentMode: "cloud" | "self_hosted";
}

export async function fetchEligibility(): Promise<EligibilityView> {
  const body = await authedJson<{
    state: EligibilityState;
    account_initialized: boolean;
    billing_available: boolean;
    paid_through_epoch: number | null;
    recovery_until_epoch: number | null;
    export_until_epoch: number | null;
    actions: { setup: boolean; billing: boolean; export: boolean; revoke: boolean };
    payer: "personal" | null;
    deployment_mode: "cloud" | "self_hosted";
  }>("/account/eligibility");
  return {
    state: body.state,
    accountInitialized: body.account_initialized,
    billingAvailable: body.billing_available,
    paidThroughEpoch: body.paid_through_epoch,
    recoveryUntilEpoch: body.recovery_until_epoch,
    exportUntilEpoch: body.export_until_epoch,
    actions: body.actions,
    payer: body.payer,
    deploymentMode: body.deployment_mode,
  };
}

export interface CloudNotice {
  noticeId: string;
  kind: string;
  channel: string;
  content: {
    title: string;
    detail: string;
    effectiveAtEpoch: number | null;
    deadlineEpoch: number | null;
    amountPence: number | null;
  };
  dueAtEpoch: number;
  status: string;
  lastErrorCode: string | null;
  deliveredAtEpoch: number | null;
  createdAtEpoch: number;
}

export async function fetchCloudNotices(): Promise<CloudNotice[]> {
  const body = await authedJson<Array<{
    notice_id: string;
    kind: string;
    channel: string;
    content: {
      title: string;
      detail: string;
      effective_at_epoch: number | null;
      deadline_epoch: number | null;
      amount_pence: number | null;
    };
    due_at_epoch: number;
    status: string;
    last_error_code: string | null;
    delivered_at_epoch: number | null;
    created_at_epoch: number;
  }>>(
    "/account/notices",
  );
  return body.map((notice) => ({
    noticeId: notice.notice_id,
    kind: notice.kind,
    channel: notice.channel,
    content: {
      title: notice.content.title,
      detail: notice.content.detail,
      effectiveAtEpoch: notice.content.effective_at_epoch,
      deadlineEpoch: notice.content.deadline_epoch,
      amountPence: notice.content.amount_pence,
    },
    dueAtEpoch: notice.due_at_epoch,
    status: notice.status,
    lastErrorCode: notice.last_error_code,
    deliveredAtEpoch: notice.delivered_at_epoch,
    createdAtEpoch: notice.created_at_epoch,
  }));
}

export interface PersonalQuote {
  offer: "monthly" | "annual";
  amountPence: number;
  currency: string;
  interval: string;
  taxTreatment: string;
  quoteVersion: number;
  quoteExpiresAtEpoch: number;
  founding: boolean;
  foundingRemainingPlaces: number | null;
  foundingTerm: string | null;
  nextRenewalAmountPence: number;
}

export async function fetchPersonalQuote(offer: PersonalQuote["offer"]): Promise<PersonalQuote> {
  const body = await authedJson<{
    offer: PersonalQuote["offer"];
    amount_pence: number;
    currency: string;
    interval: string;
    tax_treatment: string;
    quote_version: number;
    quote_expires_at_epoch: number;
    founding: boolean;
    founding_remaining_places: number | null;
    founding_term: string | null;
    next_renewal_amount_pence: number;
  }>(`/billing/personal/quote?offer=${encodeURIComponent(offer)}`);
  return {
    offer: body.offer,
    amountPence: body.amount_pence,
    currency: body.currency,
    interval: body.interval,
    taxTreatment: body.tax_treatment,
    quoteVersion: body.quote_version,
    quoteExpiresAtEpoch: body.quote_expires_at_epoch,
    founding: body.founding,
    foundingRemainingPlaces: body.founding_remaining_places,
    foundingTerm: body.founding_term,
    nextRenewalAmountPence: body.next_renewal_amount_pence,
  };
}

export interface PersonalCheckoutResult {
  operationId: string;
  state: string;
  checkoutUrl: string | null;
}

export async function createPersonalCheckout(input: {
  offer: PersonalQuote["offer"];
  idempotencyKey: string;
  quoteVersion: number;
  quoteExpiresAtEpoch: number;
  returnUrl: string;
}): Promise<PersonalCheckoutResult> {
  const body = await postJson<{
    operation_id: string;
    state: string;
    checkout_url: string | null;
  }>("/billing/personal/checkout", {
    offer: input.offer,
    idempotency_key: input.idempotencyKey,
    quote_version: input.quoteVersion,
    quote_expires_at_epoch: input.quoteExpiresAtEpoch,
    return_url: input.returnUrl,
  });
  return { operationId: body.operation_id, state: body.state, checkoutUrl: body.checkout_url };
}

export interface PersonalOperation {
  operationId: string;
  offer: string;
  state: string;
  checkoutUrl: string | null;
}

export async function fetchPersonalOperation(operationId: string): Promise<PersonalOperation> {
  const body = await authedJson<{
    operation_id: string;
    offer: string;
    state: string;
    checkout_url: string | null;
  }>(`/billing/personal/operations/${encodeURIComponent(operationId)}`);
  return {
    operationId: body.operation_id,
    offer: body.offer,
    state: body.state,
    checkoutUrl: body.checkout_url,
  };
}

export interface PersonalLifecycle {
  state: string;
  paidThroughDate: string | null;
  cancelAtPeriodEnd: boolean;
  portalUrl: string | null;
}

export async function createPersonalPortal(): Promise<PersonalLifecycle> {
  return parsePersonalLifecycle(await postJson<PersonalLifecycleResponse>("/billing/personal/portal", {}));
}

export async function cancelPersonalBilling(idempotencyKey: string): Promise<PersonalLifecycle> {
  return parsePersonalLifecycle(
    await postJson<PersonalLifecycleResponse>("/billing/personal/cancel", {
      idempotency_key: idempotencyKey,
    }),
  );
}

interface PersonalLifecycleResponse {
  state: string;
  paid_through_date: string | null;
  cancel_at_period_end: boolean;
  portal_url: string | null;
}

function parsePersonalLifecycle(body: PersonalLifecycleResponse): PersonalLifecycle {
  return {
    state: body.state,
    paidThroughDate: body.paid_through_date,
    cancelAtPeriodEnd: body.cancel_at_period_end,
    portalUrl: body.portal_url,
  };
}

export interface RefundRequest {
  requestId: string;
  state: string;
  reason: string;
  amountPence: number | null;
  fullRefundRequested: boolean;
  preservePaidTerm: boolean;
  effectiveAtEpoch: number | null;
}

function parseRefund(body: {
  request_id: string;
  state: string;
  reason: string;
  amount_pence: number | null;
  full_refund_requested: boolean;
  preserve_paid_term: boolean;
  effective_at_epoch: number | null;
}): RefundRequest {
  return {
    requestId: body.request_id,
    state: body.state,
    reason: body.reason,
    amountPence: body.amount_pence,
    fullRefundRequested: body.full_refund_requested,
    preservePaidTerm: body.preserve_paid_term,
    effectiveAtEpoch: body.effective_at_epoch,
  };
}

export async function requestPersonalRefund(input: {
  reason: string;
  amountPence?: number;
  fullRefund: boolean;
  idempotencyKey: string;
}): Promise<RefundRequest> {
  return parseRefund(
    await postJson("/billing/personal/refunds", {
      reason: input.reason,
      amount_pence: input.amountPence ?? null,
      full_refund: input.fullRefund,
      idempotency_key: input.idempotencyKey,
    }),
  );
}

export async function fetchPersonalRefund(requestId: string): Promise<RefundRequest> {
  return parseRefund(await authedJson(`/billing/personal/refunds/${encodeURIComponent(requestId)}`));
}

export interface SponsoredSeat {
  seatId: string;
  beneficiaryId: string;
  offer: string;
  effectiveFrom: number;
  effectiveUntil: number | null;
  state: string;
}

export async function fetchSponsoredSeats(orgId: string): Promise<SponsoredSeat[]> {
  const rows = await authedJson<Array<{
    seat_id: string; beneficiary_id: string; offer: string;
    effective_from: number; effective_until: number | null; state: string;
  }>>(`/orgs/${encodeURIComponent(orgId)}/billing/sponsored/seats`);
  return rows.map((row) => ({ seatId: row.seat_id, beneficiaryId: row.beneficiary_id, offer: row.offer, effectiveFrom: row.effective_from, effectiveUntil: row.effective_until, state: row.state }));
}

export interface SponsoredQuote {
  action: string;
  seatCount: number;
  amountPence: number;
  currency: string;
  interval: string;
  quoteVersion: number;
  quoteExpiresAtEpoch: number;
}

export async function fetchSponsoredQuote(orgId: string, input: { action: string; offer: string; beneficiaryIds: string[] }): Promise<SponsoredQuote> {
  const body = await postJson<{
    action: string; seat_count: number; amount_pence: number; currency: string; interval: string;
    quote_version: number; quote_expires_at_epoch: number;
  }>(`/orgs/${encodeURIComponent(orgId)}/billing/sponsored/quote`, {
    action: input.action,
    offer: input.offer,
    beneficiary_ids: input.beneficiaryIds,
  });
  return { action: body.action, seatCount: body.seat_count, amountPence: body.amount_pence, currency: body.currency, interval: body.interval, quoteVersion: body.quote_version, quoteExpiresAtEpoch: body.quote_expires_at_epoch };
}

export async function createSponsoredCheckout(orgId: string, input: {
  action: string; offer: string; beneficiaryId: string; replacementBeneficiaryId?: string;
  quoteVersion: number; quoteExpiresAtEpoch: number; effectiveFrom: number; effectiveUntil?: number;
  idempotencyKey: string; returnUrl: string;
}): Promise<{ operationId: string; state: string; checkoutUrl: string | null }> {
  const body = await postJson<{ operation_id: string; state: string; provider_checkout_url: string | null }>(`/orgs/${encodeURIComponent(orgId)}/billing/sponsored/checkout`, {
    action: input.action, offer: input.offer, beneficiary_id: input.beneficiaryId, replacement_beneficiary_id: input.replacementBeneficiaryId ?? null,
    quote_version: input.quoteVersion, quote_expires_at_epoch: input.quoteExpiresAtEpoch, effective_from: input.effectiveFrom,
    effective_until: input.effectiveUntil ?? null, idempotency_key: input.idempotencyKey, return_url: input.returnUrl,
  });
  return { operationId: body.operation_id, state: body.state, checkoutUrl: body.provider_checkout_url };
}

export interface ExportManifest {
  version: number;
  exportId: string;
  manifestHash: string;
  expiresAt: string;
  totalChunks: number;
  complete: boolean;
  projects: unknown[];
  environments: unknown[];
  notSharedEnvironmentIds: string[];
  omittedEnvironmentCount: number;
}

export async function startCloudExport(): Promise<ExportManifest> {
  const resp = await request("/account/export", { method: "POST", ...CREDS });
  if (!resp.ok) throw new Error(`export could not be started (server error ${resp.status})`);
  const body = await readBody(() => resp.json()) as {
    version: number; export_id: string; manifest_hash: string; expires_at: string;
    total_chunks: number; complete: boolean; projects: unknown[]; environments: unknown[];
    not_shared_environment_ids: string[]; omitted_environment_count: number;
  };
  return { version: body.version, exportId: body.export_id, manifestHash: body.manifest_hash,
    expiresAt: body.expires_at, totalChunks: body.total_chunks, complete: body.complete,
    projects: body.projects, environments: body.environments,
    notSharedEnvironmentIds: body.not_shared_environment_ids,
    omittedEnvironmentCount: body.omitted_environment_count };
}

export async function fetchCloudExportChunk(exportId: string, index: number): Promise<unknown> {
  return authedJson(`/account/export/${encodeURIComponent(exportId)}/chunks/${index}`);
}

async function postJson<T>(path: string, value: unknown): Promise<T> {
  const resp = await request(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(value),
    ...CREDS,
  });
  if (!resp.ok) throw new Error(`server error (${resp.status})`);
  return readBody(() => resp.json()) as Promise<T>;
}

export interface Account {
  /// KDF salt, needed to derive the master key.
  salt: Uint8Array;
  /// The account's X25519 private keys, sealed under the master key. Opening a vault-key grant
  /// needs the recovered private key, so the browser fetches this ciphertext alongside the salt.
  encPrivateKeys: Uint8Array;
}

/// The account's KDF salt + master-sealed private keys, or `null` if the account isn't set up.
export async function fetchAccount(): Promise<Account | null> {
  const resp = await request("/account", CREDS);
  if (resp.status === 404) {
    return null;
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await readBody(() => resp.json())) as { kdf_params: string; enc_private_keys: string };
  const kdf = JSON.parse(new TextDecoder().decode(standardB64ToBytes(body.kdf_params))) as {
    salt: number[];
  };
  return {
    salt: new Uint8Array(kdf.salt),
    encPrivateKeys: standardB64ToBytes(body.enc_private_keys),
  };
}

export interface Project {
  id: string;
  encName: Uint8Array;
  /// Owning organisation, or `null` for a personal project (team actions apply only when set).
  orgId: string | null;
}

export async function fetchProjects(): Promise<Project[]> {
  const rows = await authedJson<{ id: string; enc_name: string; org_id: string | null }[]>(
    "/projects",
  );
  return rows.map((r) => ({
    id: r.id,
    encName: standardB64ToBytes(r.enc_name),
    orgId: r.org_id,
  }));
}

export interface Environment {
  id: string;
  encName: Uint8Array;
  /// The caller's OWN vault-key grant, or `null` if they hold none for this environment.
  encVaultKey: Uint8Array | null;
}

export async function fetchEnvironments(projectId: string): Promise<Environment[]> {
  const rows = await authedJson<
    { id: string; enc_name: string; enc_vault_key: string | null }[]
  >(`/projects/${encodeURIComponent(projectId)}/environments`);
  return rows.map((r) => ({
    id: r.id,
    encName: standardB64ToBytes(r.enc_name),
    encVaultKey: r.enc_vault_key ? standardB64ToBytes(r.enc_vault_key) : null,
  }));
}

/// The caller's own vault-key grant for an environment, or `null` if they have none (access
/// without a key: the org lets them see ciphertext, but nobody granted them the vault key).
export async function fetchMyGrant(envId: string): Promise<Uint8Array | null> {
  const resp = await request(`/environments/${encodeURIComponent(envId)}/grant`, CREDS);
  if (resp.status === 404) {
    return null;
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await readBody(() => resp.json())) as { enc_vault_key: string };
  return standardB64ToBytes(body.enc_vault_key);
}

export interface Org {
  id: string;
  encName: Uint8Array;
  /// The caller's own role in this org.
  role: string;
  /// The org key sealed to the caller, or `null` if not granted (names fall back to ids).
  encOrgKey: Uint8Array | null;
}

export async function fetchOrgs(): Promise<Org[]> {
  const rows = await authedJson<
    { id: string; enc_name: string; role: string; enc_org_key: string | null }[]
  >("/orgs");
  return rows.map((r) => ({
    id: r.id,
    encName: standardB64ToBytes(r.enc_name),
    role: r.role,
    encOrgKey: r.enc_org_key ? standardB64ToBytes(r.enc_org_key) : null,
  }));
}

export type OrganisationDeletionState =
  | "requested"
  | "cancelling_billing"
  | "retention"
  | "purging"
  | "recovering"
  | "failed"
  | "cancelled"
  | "completed";

export interface OrganisationDeletionStatus {
  state: OrganisationDeletionState;
  requestedAt: string;
  recoverableUntil: string;
  managedBackupExpiryBy: string | null;
  nextRetryAt: string | null;
  error: "billing_unavailable" | "billing_unknown" | "purge_failed" | null;
}

/// The client gate. It is deliberately separate from the server's own gate rather than derived
/// from it: the server answers 404 when deletion is off, which is indistinguishable from an
/// unknown organisation, so the client needs its own signal to explain the state rather than
/// report an error. Vite leaves this false when the deployment does not opt in explicitly.
export const organisationDeletionEnabled =
  import.meta.env.VITE_ORGANISATION_DELETION_ENABLED === "true";

function parseOrganisationDeletionStatus(body: {
  state: OrganisationDeletionState;
  requested_at: string;
  recoverable_until: string;
  managed_backup_expiry_by: string | null;
  next_retry_at: string | null;
  error: OrganisationDeletionStatus["error"];
}): OrganisationDeletionStatus {
  return {
    state: body.state,
    requestedAt: body.requested_at,
    recoverableUntil: body.recoverable_until,
    managedBackupExpiryBy: body.managed_backup_expiry_by,
    nextRetryAt: body.next_retry_at,
    error: body.error,
  };
}

function deletionResponseError(resp: Response, fallback: string): Error {
  const messages: Record<number, string> = {
    400: "Check the deletion confirmation and try again.",
    401: "Your session has expired. Sign in again.",
    403: "Only an organisation owner can manage deletion.",
    404: "The organisation deletion operation was not found.",
    409: "The organisation deletion cannot be changed in its current state.",
    503: "Deletion billing is not available yet. Try again later.",
  };
  return new Error(messages[resp.status] ?? fallback);
}

/// Read the current deletion operation. A missing operation is normal before the owner confirms.
export async function fetchOrganisationDeletionStatus(
  orgId: string,
): Promise<OrganisationDeletionStatus | null> {
  const resp = await request(`/orgs/${encodeURIComponent(orgId)}/deletion`, CREDS);
  if (resp.status === 404) {
    return null;
  }
  if (!resp.ok) {
    throw deletionResponseError(resp, "The deletion status could not be loaded. Try again.");
  }
  return parseOrganisationDeletionStatus(await readBody(() => resp.json()));
}

/// Submit the exact-id and subscription-cancellation confirmation for an organisation.
export async function requestOrganisationDeletion(
  orgId: string,
): Promise<OrganisationDeletionStatus> {
  const resp = await request(`/orgs/${encodeURIComponent(orgId)}/deletion`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      confirm_org_id: orgId,
      acknowledge_subscription_cancellation: true,
    }),
    ...CREDS,
  });
  if (!resp.ok) {
    throw deletionResponseError(resp, "The deletion request could not be completed. Try again.");
  }
  return parseOrganisationDeletionStatus(await readBody(() => resp.json()));
}

/// Ask an owner to recover an active deletion before purge begins.
export async function cancelOrganisationDeletion(
  orgId: string,
): Promise<OrganisationDeletionStatus> {
  const resp = await request(`/orgs/${encodeURIComponent(orgId)}/deletion/cancel`, {
    method: "POST",
    ...CREDS,
  });
  if (!resp.ok) {
    throw deletionResponseError(resp, "The deletion could not be cancelled. Try again.");
  }
  return parseOrganisationDeletionStatus(await readBody(() => resp.json()));
}

export interface Entitlements {
  tier: string;
  effectiveTier: string;
  trialEndsAt: string | null;
  limits: { maxMembers: number; maxOrgProjects: number } | null;
  /// Whether configured billing can still manage an existing subscription.
  billingEnabled: boolean;
  /// Whether new hosted purchases are enabled; falls back for servers that predate this field.
  purchasesEnabled: boolean;
}

/// The org's plan (tier, trial, limits), visible to any member.
export async function fetchEntitlements(orgId: string): Promise<Entitlements> {
  const r = await authedJson<{
    tier: string;
    effective_tier: string;
    trial_ends_at: string | null;
    limits: { max_members: number; max_org_projects: number } | null;
    billing_enabled: boolean;
    purchases_enabled?: boolean;
  }>(`/orgs/${encodeURIComponent(orgId)}/entitlements`);
  return {
    tier: r.tier,
    effectiveTier: r.effective_tier,
    trialEndsAt: r.trial_ends_at,
    limits: r.limits
      ? { maxMembers: r.limits.max_members, maxOrgProjects: r.limits.max_org_projects }
      : null,
    billingEnabled: r.billing_enabled,
    purchasesEnabled: r.purchases_enabled ?? r.billing_enabled,
  };
}

/// Start a Team subscription checkout (admin/owner); returns the Stripe Checkout page URL for the
/// browser to navigate to. The tier itself flips when the webhook confirms payment.
export async function createCheckout(orgId: string): Promise<string> {
  const resp = await request(`/orgs/${encodeURIComponent(orgId)}/billing/checkout`, {
    method: "POST",
    ...CREDS,
  });
  if (resp.status === 503) {
    throw new Error("billing is not configured on this server");
  }
  if (resp.status === 403) {
    throw new Error("managing billing requires the admin or owner role");
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await readBody(() => resp.json())) as { url: string };
  return body.url;
}

/// Open Stripe's customer portal (admin/owner) to manage or cancel the subscription; returns the
/// portal URL for the browser to navigate to.
export async function createPortal(orgId: string): Promise<string> {
  const resp = await request(`/orgs/${encodeURIComponent(orgId)}/billing/portal`, {
    method: "POST",
    ...CREDS,
  });
  if (resp.status === 503) {
    throw new Error("billing is not configured on this server");
  }
  if (resp.status === 403) {
    throw new Error("managing billing requires the admin or owner role");
  }
  if (resp.status === 400) {
    throw new Error("this organisation has no billing account yet");
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await readBody(() => resp.json())) as { url: string };
  return body.url;
}

export interface AuditEvent {
  id: number;
  actor: string;
  action: string;
  target: string | null;
  envId: string | null;
  detail: string | null;
  at: string;
}

/// The org's audit events, newest first (admins/owners only).
export async function fetchAudit(orgId: string, limit = 50): Promise<AuditEvent[]> {
  const rows = await authedJson<
    {
      id: number;
      actor: string;
      action: string;
      target: string | null;
      env_id: string | null;
      detail: string | null;
      at: string;
    }[]
  >(`/orgs/${encodeURIComponent(orgId)}/audit?limit=${limit}`);
  return rows.map((r) => ({
    id: r.id,
    actor: r.actor,
    action: r.action,
    target: r.target,
    envId: r.env_id,
    detail: r.detail,
    at: r.at,
  }));
}

/// Store (or replace) a member's sealed copy of the org key.
export async function grantOrgKey(
  orgId: string,
  userId: string,
  encOrgKey: Uint8Array,
): Promise<void> {
  const resp = await request(
    `/orgs/${encodeURIComponent(orgId)}/members/${encodeURIComponent(userId)}/org-key`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ enc_org_key: bytesToStandardB64(encOrgKey) }),
      ...CREDS,
    },
  );
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
}

export interface Member {
  userId: string;
  role: string;
  /// The member's public key (base64 kept raw for sealing), or `null` if not set up yet.
  publicKey: Uint8Array | null;
}

export async function fetchMembers(orgId: string): Promise<Member[]> {
  const rows = await authedJson<{ user_id: string; role: string; public_key: string | null }[]>(
    `/orgs/${encodeURIComponent(orgId)}/members`,
  );
  return rows.map((r) => ({
    userId: r.user_id,
    role: r.role,
    publicKey: r.public_key ? standardB64ToBytes(r.public_key) : null,
  }));
}

export interface InvitedMember {
  userId: string;
  /// Their public key (for sealing the org key to them), or `null` if they haven't set up yet.
  publicKey: Uint8Array | null;
}

/// Invite an existing Sotto user into an org by email.
export async function inviteMember(orgId: string, email: string): Promise<InvitedMember> {
  const resp = await request(`/orgs/${encodeURIComponent(orgId)}/invites`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email }),
    ...CREDS,
  });
  if (resp.status === 404) {
    throw new Error("no Sotto user with that email - they must sign up first");
  }
  if (resp.status === 409) {
    throw new Error("that user is already a member");
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await readBody(() => resp.json())) as { user_id: string; public_key: string | null };
  return {
    userId: body.user_id,
    publicKey: body.public_key ? standardB64ToBytes(body.public_key) : null,
  };
}

/// Store a member's vault-key grant for an environment (sharing).
export async function createGrant(
  envId: string,
  userId: string,
  encVaultKey: Uint8Array,
): Promise<void> {
  const resp = await request(`/environments/${encodeURIComponent(envId)}/grants`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ user_id: userId, enc_vault_key: bytesToStandardB64(encVaultKey) }),
    ...CREDS,
  });
  if (resp.status === 403) {
    throw new Error("only an admin or owner can share this environment");
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
}

export interface Snapshot {
  revision: number;
  secrets: SecretEntry[];
}

/// The full snapshot including its revision (rotation writes against this as `base_revision`).
export async function fetchSnapshot(envId: string): Promise<Snapshot> {
  const snap = await authedJson<{
    revision: number;
    secrets: {
      id: string;
      enc_name: string;
      enc_value: string;
      enc_data_key: string;
      version: number;
      deleted: boolean;
    }[];
  }>(`/environments/${encodeURIComponent(envId)}/secrets`);
  return {
    revision: snap.revision,
    secrets: snap.secrets.map((s) => ({
      id: s.id,
      encName: standardB64ToBytes(s.enc_name),
      encValue: standardB64ToBytes(s.enc_value),
      encDataKey: standardB64ToBytes(s.enc_data_key),
      version: s.version,
      deleted: s.deleted,
    })),
  };
}

export async function fetchSecrets(envId: string): Promise<SecretEntry[]> {
  return (await fetchSnapshot(envId)).secrets;
}

export interface HistoryRow {
  secretId: string;
  version: number;
  encDataKey: Uint8Array;
}

/// Every retained history version's (secret, version, data key) - rotation must rewrap them all.
export async function fetchHistory(envId: string): Promise<HistoryRow[]> {
  const rows = await authedJson<{ secret_id: string; version: number; enc_data_key: string }[]>(
    `/environments/${encodeURIComponent(envId)}/history`,
  );
  return rows.map((r) => ({
    secretId: r.secret_id,
    version: r.version,
    encDataKey: standardB64ToBytes(r.enc_data_key),
  }));
}

/// The user ids currently granted an environment (rotation re-grants exactly these).
export async function fetchGrantHolders(envId: string): Promise<string[]> {
  const rows = await authedJson<{ user_id: string }[]>(
    `/environments/${encodeURIComponent(envId)}/grants`,
  );
  return rows.map((r) => r.user_id);
}

export interface MachineTokenInfo {
  tokenId: string;
  name: string;
  publicKey: Uint8Array;
}

/// The environment's active machine tokens (rotation re-seals the new key to each).
export async function fetchMachineTokens(envId: string): Promise<MachineTokenInfo[]> {
  const rows = await authedJson<{ token_id: string; name: string; public_key: string }[]>(
    `/environments/${encodeURIComponent(envId)}/tokens`,
  );
  return rows.map((r) => ({
    tokenId: r.token_id,
    name: r.name,
    publicKey: standardB64ToBytes(r.public_key),
  }));
}

export interface RotatePayload {
  baseRevision: number;
  grants: { userId: string; encVaultKey: Uint8Array }[];
  dataKeys: { secretId: string; encDataKey: Uint8Array }[];
  machineGrants: { tokenId: string; encVaultKey: Uint8Array }[];
  historyKeys: { secretId: string; version: number; encDataKey: Uint8Array }[];
}

/// Apply a key rotation (rewrapped keys + the replacement grant set) at a base revision.
export async function postRotate(envId: string, payload: RotatePayload): Promise<void> {
  const resp = await request(`/environments/${encodeURIComponent(envId)}/rotate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      base_revision: payload.baseRevision,
      grants: payload.grants.map((g) => ({
        user_id: g.userId,
        enc_vault_key: bytesToStandardB64(g.encVaultKey),
      })),
      data_keys: payload.dataKeys.map((d) => ({
        secret_id: d.secretId,
        enc_data_key: bytesToStandardB64(d.encDataKey),
      })),
      machine_grants: payload.machineGrants.map((m) => ({
        token_id: m.tokenId,
        enc_vault_key: bytesToStandardB64(m.encVaultKey),
      })),
      history_keys: payload.historyKeys.map((h) => ({
        secret_id: h.secretId,
        version: h.version,
        enc_data_key: bytesToStandardB64(h.encDataKey),
      })),
    }),
    ...CREDS,
  });
  if (resp.status === 412) {
    throw new Error("the environment changed while rotating - try again");
  }
  if (resp.status === 403) {
    throw new Error("only an admin or owner can rotate this environment");
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
}

/// Create a share link (session required); returns the public token.
export async function createShare(encBlob: Uint8Array, maxViews: number): Promise<string> {
  const resp = await request("/shares", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ enc_blob: bytesToStandardB64(encBlob), max_views: maxViews }),
    ...CREDS,
  });
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await readBody(() => resp.json())) as { token: string };
  return body.token;
}

export interface Share {
  encBlob: Uint8Array;
  passphraseSalt: Uint8Array | null;
}

/// Thrown when the link is unusable (invalid, expired, revoked, or already viewed → 404).
export class ShareUnavailable extends Error {}

interface ShareResponse {
  enc_blob: string;
  passphrase_salt: string | null;
}

export async function fetchShare(token: string): Promise<Share> {
  const resp = await request(`/shares/${encodeURIComponent(token)}`);
  if (resp.status === 404) {
    throw new ShareUnavailable(
      "This link is invalid, expired, revoked, or has already been viewed.",
    );
  }
  if (!resp.ok) {
    throw new Error(`server error (${resp.status})`);
  }
  const body = (await readBody(() => resp.json())) as ShareResponse;
  return {
    encBlob: standardB64ToBytes(body.enc_blob),
    passphraseSalt: body.passphrase_salt ? standardB64ToBytes(body.passphrase_salt) : null,
  };
}

export interface CommunityContributor {
  login: string;
  htmlUrl: string;
  contributions: number;
}

export interface Community {
  stars: number;
  forks: number;
  repoUrl: string;
  contributorCount: number;
  contributors: CommunityContributor[];
}

/// Public GitHub snapshot for the landing page. Same-origin (`/community`); returns `null` when
/// the server cannot reach GitHub and has nothing cached - the page then hides the counts.
export async function fetchCommunity(): Promise<Community | null> {
  try {
    const resp = await request("/community");
    if (!resp.ok) {
      return null;
    }
    const body = (await readBody(() => resp.json())) as {
      stars: number;
      forks: number;
      repo_url: string;
      contributor_count: number;
      contributors: { login: string; html_url: string; contributions: number }[];
    };
    return {
      stars: body.stars,
      forks: body.forks,
      repoUrl: body.repo_url,
      contributorCount: body.contributor_count,
      contributors: body.contributors.map((c) => ({
        login: c.login,
        htmlUrl: c.html_url,
        contributions: c.contributions,
      })),
    };
  } catch {
    return null;
  }
}
