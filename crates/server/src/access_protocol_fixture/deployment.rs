use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, Request};
use axum::http::{header::CACHE_CONTROL, Uri};
use axum::response::IntoResponse;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{now, Fixture, Records, CLIENT_ID, PREFIX};
use crate::error::{Error, Result};

use crate::access_proof::{self, DeploymentProofKey, Proof};

impl Proof {
    fn consume(self, records: &mut Records) -> Result<()> {
        records.proofs.retain(|_, expiry| *expiry > now());
        if self.expires <= now() || records.proofs.contains_key(&self.nonce) {
            return Err(Error::Unauthorized);
        }
        records.proofs.insert(self.nonce, self.expires);
        Ok(())
    }
}

async fn verify(
    fixture: &Fixture,
    request: Request,
    target: Uri,
    covers_body: bool,
) -> Result<(Proof, Bytes)> {
    let deployment = DeploymentProofKey {
        origin: fixture
            .0
            .issuer
            .strip_suffix(PREFIX)
            .expect("fixture issuer prefix"),
        id: CLIENT_ID,
        key: &fixture.0.deployment_key,
    };
    access_proof::verify(&deployment, request, target, covers_body).await
}

#[derive(Deserialize)]
struct Confirm {
    attempt_id: Uuid,
}

#[derive(Serialize)]
struct Confirmation {
    attempt_id: Uuid,
    connection_id: Uuid,
}

pub(super) async fn confirm(
    Extension(fixture): Extension<Fixture>,
    OriginalUri(target): OriginalUri,
    request: Request,
) -> Result<impl IntoResponse> {
    let (proof, body) = verify(&fixture, request, target, true).await?;
    let input: Confirm = serde_json::from_slice(&body)
        .map_err(|_| Error::BadRequest("invalid fixture confirmation".into()))?;
    let mut records = fixture.0.records.lock().expect("fixture records");
    proof.consume(&mut records)?;
    let connection_id = *records
        .confirmations
        .entry(input.attempt_id)
        .or_insert_with(Uuid::new_v4);
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(Confirmation {
            attempt_id: input.attempt_id,
            connection_id,
        }),
    ))
}

pub(super) async fn status(
    Extension(fixture): Extension<Fixture>,
    OriginalUri(target): OriginalUri,
    Path(attempt_id): Path<Uuid>,
    request: Request,
) -> Result<impl IntoResponse> {
    let (proof, _) = verify(&fixture, request, target, false).await?;
    let mut records = fixture.0.records.lock().expect("fixture records");
    proof.consume(&mut records)?;
    let connection_id = *records
        .confirmations
        .get(&attempt_id)
        .ok_or_else(|| Error::NotFound("unknown fixture attempt".into()))?;
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(Confirmation {
            attempt_id,
            connection_id,
        }),
    ))
}
