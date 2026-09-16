# Server authorisation assurance

The server assurance binary runs production Axum handlers against a migrated PostgreSQL database:

```sh
DATABASE_URL='postgres://sotto:sotto@localhost:5432/sotto' \
SOTTO_RUN_DB_TESTS=1 \
cargo test -p sotto-server --test authorisation_transactions -- \
  --test-threads=1 --nocapture
```

`SOTTO_RUN_DB_TESTS=1` is required for acceptance. In that mode `DATABASE_URL` must be present,
reachable, and point at a loopback database. Migrations are applied before the test starts. Without
the opt-in, a local run prints an explicit skip so an ordinary workspace test cannot write to a
database accidentally. Use a fresh disposable database for each run.

The binary currently records seven completed scenarios before printing
`SERVER_ASSURANCE_DONE N`: the access matrix, sequential authorisation rechecks after membership
changes, stale authorisation after locked membership changes, forced lifecycle rechecks, competing
batch revisions, lifecycle and revision conflicts, and atomic removal or malformed batch behaviour.
Assertions use the real router and also inspect persisted membership, grant, token, revision, and
secret state. A missing completion line, zero scenarios, a failed assertion, or a database error
fails the job.

These seven scenarios are the current executable slice, rather than the complete S1-S10 programme.
Paired grant or rotation races, personal and rotation revision contenders, owner-to-owner races,
late audit-failure rollback injection, token and rotation ordering, and both machine read endpoints
remain outstanding and must not be inferred from the cases above.

The required CI and coverage jobs invoke this test target explicitly. This suite does not claim
cryptographic correctness, client or WebAssembly behaviour, hostile-workflow isolation, or release
gating.
