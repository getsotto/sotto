# Access protocol fixture

Run the Rust side of the Access interoperability gate on an isolated development machine. The fixture reuses Sotto's mock OAuth provider and sessions. Its OpenID Connect and signed deployment endpoints exist only in a server built with the explicit fixture feature. They require loopback addresses.

## Run

Use Python 3, Docker, OpenSSL 3 and a Caddy binary with the same rate limiting plugin as the self-hosted image. The fixture was exercised with Caddy 2.11.9. To build the matching edge locally with Go, run these commands from the checkout root.

~~~sh
go run github.com/caddyserver/xcaddy/cmd/xcaddy@v0.4.7 build v2.11.9 \
  --with github.com/mholt/caddy-ratelimit@v0.1.0 --output /tmp/sotto-fixture-caddy
cargo build --locked -p sotto-server --features e2e-access-protocol
scripts/access-protocol-fixture --server target/debug/sotto-server \
  --caddy /tmp/sotto-fixture-caddy --check
~~~

The runner starts disposable PostgreSQL 16 and adapts the repository Caddyfile. It supplies a locally trusted certificate for the client and confines listeners to loopback. The route matchers and reverse proxy are the production ones. The runner suppresses Caddy request logs and changes listener, TLS and admin settings for isolation. It deletes its private temporary directory and removes its database container when it exits.

The check uses two mock identities, a valid and invalid OIDC exchange, and signed requests. It checks the ID token signature, rejects a token endpoint URL or multiple audiences in the client assertion, and tests the wrong PKCE verifier and code/proof reuse. It also checks changed target and body, expired deployment proof, repeated confirmation with a fresh proof, and a bodyless status lookup.

To verify that a normal binary has no fixture routes, build the default feature set and use the same runner.

~~~sh
cargo build --locked -p sotto-server
scripts/access-protocol-fixture --server target/debug/sotto-server \
  --caddy /tmp/sotto-fixture-caddy --check-default
~~~

## Drive from another client

Run without either check flag to keep the fixture alive until Ctrl+C or SIGTERM. Pass the client's exact loopback callback with `--callback`. The runner prints the path to a private connection file with the issuer URL, discovery URL, client ID, callback and paths to the temporary certificate and deployment signing key. Trust the supplied certificate for this fixture process. Treat the files as temporary test material.

The client can establish a Sotto cookie over the real mock OAuth routes. Start at the Sotto origin's GitHub login endpoint using that origin as the web redirect target and a local state value. Read the provider state from the redirect and call Sotto's GitHub callback directly with that state and either fixture-alice or fixture-bob as the mock code. Retain the Sotto cookie for the authorization request. The normal account lookup at /auth/me returns the Sotto user ID to compare with the ID token subject.

Use authorization code flow with PKCE S256, an OpenID scope and a nonce. Authenticate the token exchange with an ES256 private_key_jwt assertion for the registered client. Its sole audience is the fixture issuer, including the issuer path. Codes and proofs expire within two minutes. Do not follow the redirect to real GitHub.

The routes below are relative to the issuer URL in the connection file.

| Operation | Route | Fixture payload |
| --- | --- | --- |
| OIDC discovery | GET /.well-known/openid-configuration | Published token, authorization and signing key URLs |
| Confirm an attempt | POST /confirm | JSON object with attempt_id as a UUID |
| Check confirmed status | GET /connection-status/{attempt_id} | Empty body |

For the last two use RFC 9421 with the deployment key, key ID matching the client ID, and algorithm ecdsa-p256-sha256. Sign the following covered components in order: @method, @target-uri and, for the POST, content-digest. Use the external HTTPS URL as the target. Include created, expires and a fresh unique nonce. The POST digest uses RFC 9530 sha-256; the bodyless status lookup has no digest. A fresh signed confirmation of the same attempt returns the same connection ID. Repeating the earlier signature is rejected.

Protocol state is intentionally in memory, while Sotto users and sessions use the disposable database. The fixture does not create production connection credentials. The Java client remains the next interoperability check.

## Exercise the production issuer

Build with only the mock GitHub feature and start the same runner with the production issuer profile. The runner supplies temporary registration and signing keys and points Caddy at the regular issuer handlers.

```sh
cargo build --locked -p sotto-server --features e2e-mock-oauth
scripts/access-protocol-fixture --server target/debug/sotto-server \
  --caddy /tmp/sotto-fixture-caddy --production-issuer --check
```

Omit `--check` to feed the emitted connection file and fixture certificate to the Access Java sign-in test. Run only `SottoSignInIT` against the production issuer. Deployment proof operations belong to the original fixture and later production slices.

The default production check starts with an empty previous-key setting and verifies that JWKS contains only the active key. Add `--issuer-key-overlap` to publish a previous key and check both keys while new tokens still use the active key. The Sotto server CI job runs both variants.

Use `--production-issuer --connection-lifecycle --check` to exercise the owner connection routes, signed handoffs, staged verification, rotation and disconnect. This mode also checks the organisation credential validator through a route compiled only with the mock OAuth test feature.
