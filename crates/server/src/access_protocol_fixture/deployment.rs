use axum::body::{to_bytes, Bytes};
use axum::extract::{OriginalUri, Path, Request};
use axum::http::{header::CACHE_CONTROL, HeaderMap, Uri};
use axum::response::IntoResponse;
use axum::{Extension, Json};
use base64::{engine::general_purpose::STANDARD, Engine};
use p256::ecdsa::{signature::Verifier, Signature};
use serde::{Deserialize, Serialize};
use sfv::{Dictionary, FieldType, ListEntry, Parser, Version};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{now, Fixture, Records, CLIENT_ID, KEY_LIFETIME, PREFIX};
use crate::error::{Error, Result};

struct Proof {
    nonce: String,
    expires: u64,
}

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

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).into_iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn signed_request(
    fixture: &Fixture,
    request: &Request,
    target: &Uri,
    digest: Option<&str>,
) -> Option<Proof> {
    let headers = request.headers();
    let parse = |name| -> Option<Dictionary> {
        Parser::new(single_header(headers, name)?)
            .with_version(Version::Rfc8941)
            .parse()
            .ok()
    };
    let inputs = parse("signature-input")?;
    let signatures = parse("signature")?;
    let (label, ListEntry::InnerList(input)) = inputs.first()? else {
        return None;
    };
    let ListEntry::Item(signature) = signatures.get(label)? else {
        return None;
    };
    if inputs.len() != 1 || signatures.len() != 1 || !signature.params.is_empty() {
        return None;
    }
    let expected = if digest.is_some() {
        &["@method", "@target-uri", "content-digest"][..]
    } else {
        &["@method", "@target-uri"][..]
    };
    let covered = input.items.iter().map(|item| {
        item.params
            .is_empty()
            .then(|| item.bare_item.as_string().map(sfv::StringRef::as_str))
            .flatten()
    });
    let now = now();
    let string = |name| {
        input
            .params
            .get(name)?
            .as_string()
            .map(sfv::StringRef::as_str)
    };
    let timestamp = |name| u64::try_from(i64::from(input.params.get(name)?.as_integer()?)).ok();
    let created = timestamp("created")?;
    let expires = timestamp("expires")?;
    let nonce = string("nonce")?;
    if string("keyid")? != CLIENT_ID
        || string("alg")? != "ecdsa-p256-sha256"
        || nonce.is_empty()
        || !covered.eq(expected.iter().copied().map(Some))
        || created > now
        || expires <= now
        || expires.saturating_sub(created) > KEY_LIFETIME
    {
        return None;
    }
    // Use the configured external origin and Axum's original wire path. The fixture accepts
    // one signature over this exact set of components and preserves wire parameter order.
    let origin = fixture.0.issuer.strip_suffix(PREFIX)?;
    let path = target.path_and_query()?;
    let mut base = format!(
        "\"@method\": {}\n\"@target-uri\": {origin}{path}",
        request.method()
    );
    if let Some(digest) = digest {
        base.push_str(&format!("\n\"content-digest\": {digest}"));
    }
    let serialised = inputs.serialize()?;
    let (_, params) = serialised.split_once('=')?;
    base.push_str(&format!("\n\"@signature-params\": {params}"));
    let signature = Signature::from_slice(signature.bare_item.as_byte_sequence()?).ok()?;
    fixture
        .0
        .deployment_key
        .verify(base.as_bytes(), &signature)
        .ok()?;
    Some(Proof {
        nonce: nonce.to_owned(),
        expires,
    })
}

async fn verify(
    fixture: &Fixture,
    request: Request,
    target: Uri,
    covers_body: bool,
) -> Result<(Proof, Bytes)> {
    let digest = if covers_body {
        Some(single_header(request.headers(), "content-digest").ok_or(Error::Unauthorized)?)
    } else if request.headers().contains_key("content-digest") {
        return Err(Error::Unauthorized);
    } else {
        None
    };
    let proof = signed_request(fixture, &request, &target, digest).ok_or(Error::Unauthorized)?;
    let digest = digest.map(str::to_owned);
    let bytes = to_bytes(request.into_body(), 4096)
        .await
        .map_err(|_| Error::Unauthorized)?;
    if let Some(expected) = digest {
        let actual = format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(&bytes)));
        if expected != actual {
            return Err(Error::Unauthorized);
        }
    } else if !bytes.is_empty() {
        return Err(Error::Unauthorized);
    }
    Ok((proof, bytes))
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
