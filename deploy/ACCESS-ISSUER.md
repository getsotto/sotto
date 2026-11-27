# Configure the Sotto Access issuer

The Access issuer is an opt-in feature for a self-hosted Sotto installation. Configure GitHub OAuth and use the same HTTPS origin for `SOTTO_PUBLIC_URL` and `SOTTO_WEB_ORIGIN`. Set these variables in the Sotto server environment.

| Variable | Value |
| --- | --- |
| `SOTTO_ACCESS_CLIENT_ID` | The identifier assigned to the Access deployment |
| `SOTTO_ACCESS_CALLBACK_URL` | The exact HTTPS callback served by that deployment, with no query or fragment |
| `SOTTO_ACCESS_DEPLOYMENT_PUBLIC_KEY_FILE` | Path inside the server to the deployment's P-256 public key in PEM format |
| `SOTTO_ACCESS_ISSUER_PRIVATE_KEY_FILE` | Path inside the server to Sotto's separate PKCS#8 P-256 private key in PEM format |
| `SOTTO_ACCESS_ISSUER_PREVIOUS_PUBLIC_KEY_FILE` | Optional path to the previous issuer's P-256 public key in PEM format |
| `SOTTO_ACCESS_SETUP_CALLBACK_URL` | Enables connection management. Set a separate exact HTTPS URL on the registered Access application's origin for the setup handoff. |

Mount keys as read-only files and keep private keys readable only by the service that uses them. The Access service holds the deployment private key. Sotto holds the issuer private key. Restart Sotto after changing the registration or key files.

For Docker Compose, add an override alongside `docker-compose.prod.yml`. Keep the registration settings in a local `access-issuer.env` file and use paths under `/run/sotto-access` for the public and private key variables.

```yaml
services:
  server:
    env_file:
      - access-issuer.env
    volumes:
      - ./access-issuer-keys:/run/sotto-access:ro
```

Include both Compose files when starting or updating the deployment.

The issuer is `SOTTO_PUBLIC_URL` followed by `/integrations/access/oidc`. Append `/.well-known/openid-configuration` to retrieve discovery. Caddy forwards discovery, JWKS, authorisation and token requests through the existing unauthenticated rate limit.

The registered client requests the `openid` scope with S256 PKCE, a nonce and browser state. Access verifies the returned state against its browser session before exchanging the code. Codes expire after 120 seconds and are consumed atomically. Authenticate the token exchange with ES256 `private_key_jwt` using the issuer URL as the sole audience. Use an assertion lifetime of at most 120 seconds and a unique assertion ID. The returned identity subject is the existing Sotto user ID. The required OIDC access token is opaque and grants no Sotto API access. [OpenID Connect Code Flow](https://openid.net/specs/openid-connect-core-1_0.html#CodeFlowAuth)

## Change keys

During a planned issuer rotation, keep the old public key published in JWKS with the previous-key setting while switching signing to the new private key. Publish the old public key until existing ID tokens have expired and allow for the clients' clock tolerance and key caching. Remove the previous-key setting and restart to withdraw that key. Revoke a compromised issuer key immediately and coordinate session revocation with Access.

For deployment key rotation, coordinate a switch of Access's private key and Sotto's registered public key. Sotto accepts one deployment public key at a time. The previous deployment key and authorisation codes tied to its registration cease to work after the switch.

## Manage an organisation connection

With a setup callback configured, a current owner can inspect the connection or start, rotate or disconnect it through the version 1 API. A start or rotation returns a URL with a one-time code in the fragment. The Access setup page can read the fragment, clear it from the address bar and pass the code to its own backend. The backend signs the redemption request and receives a credential that stays staged for up to ten minutes. An unused handoff code also expires after ten minutes.

After storing the credential, Access verifies it with Sotto, then signs a confirmation containing the same credential. Sotto activates the credential only after verification and confirmation. After a lost response, sign a new confirmation for the same attempt to retrieve the current receipt. A revoked, superseded or expired pending attempt requires a fresh owner handoff.

Routine rotation leaves the existing credential active until the new credential is confirmed. Disconnect revokes active and staged credentials directly in Sotto. The owner status reports an expired pending attempt so the owner can restart setup or rotation. A change to the registered deployment key or callback invalidates credentials bound to the previous registration. An owner can rotate or disconnect that connection.

## Contract

The [version 1 OpenAPI document](../contracts/access/v1.openapi.yaml) specifies the implemented owner and setup operations. Organisation metadata and deletion status are separate slices.
