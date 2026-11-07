# Move personal billing to an organisation

Sotto Cloud lets an organisation take over the billing for your personal account. This guide
covers that one journey: you currently pay personally, and an organisation should pay instead.
It describes what you see in the web client and does not cover self-hosted servers, which apply
no hosted billing.

The names below (Example Studio Ltd, Priya Shah, alex@example.com) are fictional.

## Before you start

- Sign in to Sotto Cloud and open the **Cloud account** page at `/cloud` (for example
  `https://getsotto.co.uk/cloud`).
- You must belong to the organisation that should pay. You do not need to be an owner or admin
  to start the request.
- Someone has to respond for the organisation: an owner or admin. If you are one yourself, see
  step 3 below.
- The **Change who pays** section only appears when you are the personal payer and your account
  is paid or in renewal recovery. If you do not see it, nothing is wrong with your account; the
  option is simply not available in its current state.
- Payer transfers also depend on the server. If the server does not support them, the page says
  "Payer transfers are not available on this server."

## 1. Start the request

1. On the **Cloud account** page, find **Change who pays**. It explains that billing moves to an
   organisation at your next renewal, that the current payer remains responsible through the paid
   term, and that no mid-term credit is issued.
2. Under the organisation list, choose **Choose an organisation you belong to** and pick the
   organisation that should pay.
3. In the next list, pick the owner or admin who will respond to the request. If you also
   administer the organisation, the list includes **I also administer this organisation**, so you
   can choose that instead of naming another person. A second person is therefore not needed in
   every case.
4. Select **Review payer transfer**. Sotto loads a quote that reads like this: "At renewal on
   (date), the organisation would pay (amount) per (interval). No mid-term credit is due." If a
   founding price applies at that renewal, the quote says so. Nothing has been sent at this point;
   select **Cancel** to go back.
5. Select **Send request for acceptance**.

Example: Alex (alex@example.com) chooses Example Studio Ltd as the organisation and Priya Shah, an
admin of that organisation, as the person to respond. Alex reviews the quote and sends the request.

## 2. The administrator responds

The chosen administrator opens the **Cloud account** page and finds the **Payer transfers**
section. Each request is labelled **Incoming request** (for the administrator being asked) or
**Outgoing request** (for the person who started it).

- On an incoming request that is still waiting, the administrator can choose **Accept transfer
  request** or **Decline request**.
- On an outgoing request that is still waiting, the person who started it can choose **Withdraw
  request**.

These buttons only appear while the request is waiting for a response and its invitation has not
expired. Once it has been accepted, declined, withdrawn or has expired, there is nothing left to
choose.

## 3. What each status means

| Stage | What the page says | What it means |
| --- | --- | --- |
| Request sent | "Transfer request sent. The other payer must accept it; no billing change has happened yet." Later: "Waiting for the other payer to respond." | The invitation is waiting for a response. Nothing about billing has changed. |
| Accepted, preparing | "Accepted. The billing change is being prepared; it is not complete yet." Then: "The change is being prepared for the renewal date." and "Payment is recorded and the payer handover is being verified." | The administrator has agreed. Sotto is preparing and verifying the change, but the payer has not changed yet. |
| Completed | "Payer change completed after provider verification." | The organisation is now the payer. |

Acceptance is not completion. The page states the timing in the same way each time:

- The change takes effect at renewal.
- The current payer remains responsible through the paid term.
- No mid-term credit is issued by this transfer flow.

Other messages you may see:

- "This request ended without changing the payer." The request did not go ahead and billing is
  unchanged.
- "This invitation expired without changing the payer." The invitation was not answered in time.
- "The previous payer is being restored after a payment issue." The change hit a payment problem
  and billing is going back to the previous payer.

## 4. Checking progress and fixing problems

- Select **Refresh transfers** in the **Payer transfers** section to see the latest state.
- Each invitation has a response deadline, and the administrator needs to respond before it passes.
  If it expires, the page says "This invitation expired at the renewal boundary. Ask the initiator
  to start a new request." Start again from **Change who pays** to send a fresh invitation.
- If the page says "The billing terms changed. Refresh the page and request a new quote.", reload
  the page and review a new quote before sending.
- If the page says "The transfer service is temporarily unavailable. Try again shortly.", wait and
  try again.
- If the page says "This transfer needs support review. It will not continue automatically.",
  contact Sotto support before making another billing change.
- This guide gives no completion time, because none is guaranteed. If a transfer stays in a
  preparing state, refresh again later, and contact Sotto support if it does not move.