# Configure the Sotto Access issuer

The Access issuer is an opt-in feature for a self-hosted Sotto installation. Configure GitHub OAuth and use the same HTTPS origin for `SOTTO_PUBLIC_URL` and `SOTTO_WEB_ORIGIN`. Set these variables in the Sotto server environment.

| Variable | Value |
| --- | --- |
| `SOTTO_ACCESS_CLIENT_ID` | The identifier assigned to the Access deployment |
| `SOTTO_ACCESS_CALLBACK_URL` | The exact HTTPS callback served by that deployment, with no query or fragment |
| `SOTTO_ACCESS_DEPLOYMENT_PUBLIC_KEY_FILE` | Path inside the server to the deployment's P-256 public key in PEM format |
| `SOTTO_ACCESS_ISSUER_PRIVATE_KEY_FILE` | Path inside the server to Sotto's separate PKCS#8 P-256 private key in PEM format |
| `SOTTO_ACCESS_ISSUER_PREVIOUS_PUBLIC_KEY_FILE` | Optional path to the previous issuer's P-256 public key in PEM format |

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

## Contract

The [version 1 OpenAPI document](../contracts/access/v1.openapi.yaml) is the canonical placeholder for the JSON integration surface. Add implemented operations and executable contract checks together. Publish a supported version when its operations and compatibility have been verified.
