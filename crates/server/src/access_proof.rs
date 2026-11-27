use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, Uri};
use base64::{engine::general_purpose::STANDARD, Engine};
use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use sfv::{Dictionary, FieldType, ListEntry, Parser, Version};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

const PROOF_LIFETIME: u64 = 120;

pub(crate) struct DeploymentProofKey<'a> {
    pub origin: &'a str,
    pub id: &'a str,
    pub key: &'a VerifyingKey,
}

pub(crate) struct Proof {
    pub nonce: String,
    pub expires: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).into_iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn signed_request(
    deployment: &DeploymentProofKey<'_>,
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
    if string("keyid")? != deployment.id
        || string("alg")? != "ecdsa-p256-sha256"
        || !(1..=256).contains(&nonce.len())
        || !covered.eq(expected.iter().copied().map(Some))
        || created > now
        || expires <= now
        || expires.saturating_sub(created) > PROOF_LIFETIME
    {
        return None;
    }
    // Use the configured external origin and Axum's original wire path. The profile accepts
    // one signature over this exact set of components and preserves wire parameter order.
    let origin = deployment.origin;
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
    deployment.key.verify(base.as_bytes(), &signature).ok()?;
    Some(Proof {
        nonce: nonce.to_owned(),
        expires,
    })
}

pub(crate) async fn verify(
    deployment: &DeploymentProofKey<'_>,
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
    let proof = signed_request(deployment, &request, &target, digest).ok_or(Error::Unauthorized)?;
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
