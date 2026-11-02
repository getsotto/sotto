# Add an email contact for Cloud notices

Open `/cloud` on your Sotto server, sign in, and find **Lifecycle email**.
You do not need to unlock your vault.

Adding an email contact is optional. It does not change your vault access or
billing rights, and it cannot recover your decryption keys. Keep your Emergency Kit.

## Add and verify an address

1. Enter your address in **Email address** and select **Send verification link**.
2. Check your inbox and follow the link in the verification email.
3. On the Cloud page, look for **Email address verified.** and **Verified contact**
   with a masked version of your address.

The message "If this address can be used, a verification message will arrive
shortly" only acknowledges the request. It does not mean your address is verified.

If the link is invalid or has expired, return to **Lifecycle email**, enter your
address again, and request a fresh link. The button may say **Resend verification
link** while a request is pending.

## Remove a contact

Select **Remove verified email**. After it succeeds, the page says **Lifecycle
email contact removed.** Removing the contact does not delete your account or
cancel your subscription.

## Check email status

The page may show the latest lifecycle email's status. **Queued** means it is
waiting to be sent. **Accepted** means the email service accepted it, not that
delivery is confirmed. A delivery confirmation is shown separately.

Delivery can also be unknown, failed or paused. Check your inbox if the status is
unknown. If the address cannot accept messages, the page may ask you to verify a
different address. Do not rely on a notice arriving at a particular time.

If setup is unavailable, the server may be too old or email enrolment may be
disabled. Ask your instance operator; the [operator guide](deploy/README.md#cloud-lifecycle-email-contacts)
covers configuration. If settings fail to load, use **Retry** on the page.
