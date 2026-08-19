import { useEffect, useRef, useState } from "react";

import {
  cancelOrganisationDeletion,
  fetchOrganisationDeletionStatus,
  organisationDeletionEnabled,
  requestOrganisationDeletion,
  type OrganisationDeletionState,
  type OrganisationDeletionStatus,
} from "./api";

interface OrganisationDeletionPanelProps {
  orgId: string;
  orgName: string;
  onActiveChange: (active: boolean) => void;
}

const RECOVERABLE_STATES: OrganisationDeletionState[] = [
  "requested",
  "cancelling_billing",
  "retention",
  "failed",
];

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function readableDate(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? value : date.toLocaleString();
}

function stateLabel(state: OrganisationDeletionState): string {
  switch (state) {
    case "cancelling_billing":
      return "cancelling billing";
    case "purging":
      return "purging organisation data";
    case "cancelled":
      return "recovered";
    default:
      return state;
  }
}

function errorLabel(error: OrganisationDeletionStatus["error"]): string | null {
  switch (error) {
    case "billing_unavailable":
      return "Billing could not be confirmed. Your organisation remains protected and no data has been purged.";
    case "billing_unknown":
      return "Billing returned an unknown status. The deletion remains paused until it can be confirmed safely.";
    case "purge_failed":
      return "The final purge could not complete. Your organisation remains protected while the operation is reviewed.";
    default:
      return null;
  }
}

function isActiveStatus(status: OrganisationDeletionStatus | null): boolean {
  return status !== null && status.state !== "cancelled" && status.state !== "completed";
}

export function OrganisationDeletionPanel({
  orgId,
  orgName,
  onActiveChange,
}: OrganisationDeletionPanelProps) {
  const [status, setStatus] = useState<OrganisationDeletionStatus | null>(null);
  const [statusLoading, setStatusLoading] = useState(organisationDeletionEnabled);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [confirmation, setConfirmation] = useState("");
  const [acknowledged, setAcknowledged] = useState(false);
  const [busy, setBusy] = useState(false);
  const lastKnownStatus = useRef<OrganisationDeletionStatus | null>(null);

  useEffect(() => {
    if (!organisationDeletionEnabled) {
      return;
    }
    let current = true;
    lastKnownStatus.current = null;
    setStatus(null);
    setStatusLoading(true);
    onActiveChange(true);
    void fetchOrganisationDeletionStatus(orgId)
      .then((next) => {
        if (current) {
          lastKnownStatus.current = next;
          setStatus(next);
          setStatusError(null);
          onActiveChange(isActiveStatus(next));
        }
      })
      .catch((error: unknown) => {
        if (current) {
          setStatusError(message(error));
          onActiveChange(isActiveStatus(lastKnownStatus.current));
        }
      })
      .finally(() => {
        if (current) {
          setStatusLoading(false);
        }
      });
    return () => {
      current = false;
    };
  }, [onActiveChange, orgId]);

  async function requestDeletion() {
    if (confirmation !== orgId || !acknowledged) {
      return;
    }
    setBusy(true);
    setStatusError(null);
    try {
      const next = await requestOrganisationDeletion(orgId);
      lastKnownStatus.current = next;
      setStatus(next);
      onActiveChange(isActiveStatus(next));
      setConfirming(false);
      setConfirmation("");
      setAcknowledged(false);
    } catch (error) {
      setStatusError(message(error));
      onActiveChange(isActiveStatus(lastKnownStatus.current));
    } finally {
      setBusy(false);
    }
  }

  async function cancelDeletion() {
    setBusy(true);
    setStatusError(null);
    try {
      const next = await cancelOrganisationDeletion(orgId);
      lastKnownStatus.current = next;
      setStatus(next);
      onActiveChange(isActiveStatus(next));
    } catch (error) {
      setStatusError(message(error));
      onActiveChange(isActiveStatus(lastKnownStatus.current));
    } finally {
      setBusy(false);
    }
  }

  async function refreshStatus() {
    setStatusLoading(true);
    setStatusError(null);
    try {
      const next = await fetchOrganisationDeletionStatus(orgId);
      lastKnownStatus.current = next;
      setStatus(next);
      onActiveChange(isActiveStatus(next));
    } catch (error) {
      setStatusError(message(error));
      onActiveChange(isActiveStatus(lastKnownStatus.current));
    } finally {
      setStatusLoading(false);
    }
  }

  return (
    <section className="deletion-panel" aria-labelledby={`delete-${orgId}`}>
      <h3 id={`delete-${orgId}`}>Delete organisation</h3>
      {!organisationDeletionEnabled ? (
        <p className="muted">Deletion controls are not enabled on this server yet.</p>
      ) : statusLoading ? (
        <p className="muted">Checking deletion status…</p>
      ) : statusError !== null && status === null ? (
        <div role="alert">
          <p>{statusError}</p>
          <button type="button" className="ghost" onClick={() => void refreshStatus()}>
            Retry status
          </button>
        </div>
      ) : status === null ? (
        <>
          <p>
            Deleting <strong>{orgName}</strong> freezes organisation writes during a 30-day
            recovery window. Reads and exports remain available until purge begins.
          </p>
          {!confirming ? (
            <button className="danger" onClick={() => setConfirming(true)}>
              Request deletion
            </button>
          ) : (
            <form
              className="stack deletion-confirmation"
              onSubmit={(event) => {
                event.preventDefault();
                void requestDeletion();
              }}
            >
              <p>
                This cancels any active subscription immediately. There is no final invoice,
                automatic credit, or refund for unused time. Stripe stops automatic collection of
                already-finalised invoices, and recovery is not possible after purge begins.
              </p>
              <p>
                Sotto purges its local organisation data, but Stripe retains billing records. This
                is not personal-data erasure. Downloaded data and keys, and existing unattributed
                share links, remain outside this deletion boundary.
              </p>
              <label>
                Type this organisation ID exactly to confirm: <code>{orgId}</code>
                <input
                  value={confirmation}
                  onChange={(event) => setConfirmation(event.target.value)}
                  placeholder={orgId}
                  autoComplete="off"
                />
              </label>
              <label className="check-label">
                <input
                  type="checkbox"
                  checked={acknowledged}
                  onChange={(event) => setAcknowledged(event.target.checked)}
                />
                I understand that subscription cancellation is immediate.
              </label>
              <div className="row">
                <button
                  className="danger"
                  type="submit"
                  disabled={busy || confirmation !== orgId || !acknowledged}
                >
                  {busy ? "Requesting deletion…" : "Confirm deletion"}
                </button>
                <button type="button" className="ghost" onClick={() => setConfirming(false)}>
                  Keep organisation
                </button>
              </div>
            </form>
          )}
        </>
      ) : (
        <DeletionStatusView
          status={status}
          busy={busy}
          onCancel={() => void cancelDeletion()}
          onRefresh={() => void refreshStatus()}
        />
      )}
      {statusError !== null && status !== null && <p role="alert">{statusError}</p>}
    </section>
  );
}

function DeletionStatusView({
  status,
  busy,
  onCancel,
  onRefresh,
}: {
  status: OrganisationDeletionStatus;
  busy: boolean;
  onCancel: () => void;
  onRefresh: () => void;
}) {
  const recoverable = RECOVERABLE_STATES.includes(status.state);
  const failure = errorLabel(status.error);

  return (
    <div className="deletion-status" role="status" aria-live="polite">
      <p>
        State: <strong>{stateLabel(status.state)}</strong>
      </p>
      <dl>
        <div>
          <dt>Recovery available until</dt>
          <dd>
            <time dateTime={status.recoverableUntil}>{readableDate(status.recoverableUntil)}</time>
          </dd>
        </div>
        <div>
          <dt>Managed backup expiry</dt>
          <dd>
            {status.managedBackupExpiryBy === null
              ? "Not configured yet"
              : (
                  <time dateTime={status.managedBackupExpiryBy}>
                    {readableDate(status.managedBackupExpiryBy)}
                  </time>
                )}
          </dd>
        </div>
        {status.nextRetryAt !== null && (
          <div>
            <dt>Next worker attempt</dt>
            <dd>
              <time dateTime={status.nextRetryAt}>{readableDate(status.nextRetryAt)}</time>
            </dd>
          </div>
        )}
      </dl>
      {status.state === "recovering" && (
        <p className="muted">
          Recovery is being checked. Writes remain frozen until billing is reconciled.
        </p>
      )}
      {status.state === "purging" && (
        <p className="muted">Purge has started. Recovery is no longer available.</p>
      )}
      {status.state === "cancelled" && (
        <p className="notice">Deletion cancelled. Organisation access is restored.</p>
      )}
      {failure !== null && <p role="alert">{failure}</p>}
      {recoverable && (
        <div className="row">
          <button disabled={busy} onClick={onCancel}>
            {busy ? "Recovering…" : "Cancel deletion"}
          </button>
          <button type="button" className="ghost" disabled={busy} onClick={onRefresh}>
            Refresh status
          </button>
        </div>
      )}
      {!recoverable && status.state !== "completed" && (
        <button type="button" className="ghost" disabled={busy} onClick={onRefresh}>
          Refresh status
        </button>
      )}
    </div>
  );
}
