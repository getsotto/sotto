//! Loopback fixture for the Access protocol gate. Production routing never includes it.

mod deployment;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::Query;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::{
    signature::{Signer, Verifier},
    Signature, SigningKey, VerifyingKey,
};
use p256::pkcs8::{DecodePrivateKey, DecodePublicKey};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::{Error, Result};
use crate::state::AppState;

// This prefix reaches the API through the existing Caddy OAuth matcher.
const PREFIX: &str = "/auth/github/access-fixture";
const CLIENT_ID: &str = "sotto-access-fixture";
const KEY_LIFETIME: u64 = 120;
const ISSUER_KEY_ID: &str = "fixture-issuer";

#[derive(Clone)]
pub struct Fixture(Arc<FixtureData>);

struct FixtureData {
    issuer: String,
    callback: Url,
    issuer_key: SigningKey,
    deployment_key: VerifyingKey,
    records: Mutex<Records>,
}

#[derive(Default)]
struct Records {
    codes: HashMap<String, Code>,
    assertions: HashMap<String, u64>,
    proofs: HashMap<String, u64>,
    confirmations: HashMap<Uuid, Uuid>,
}

struct Code {
    user_id: String,
    challenge: String,
    nonce: String,
    expires: u64,
}

impl Fixture {
    pub fn from_env(bind: &str) -> Result<Self> {
        if !bind
            .parse::<SocketAddr>()
            .is_ok_and(|address| address.ip().is_loopback())
        {
            return Err(Error::Config(
                "access fixture requires a loopback bind".into(),
            ));
        }
        let origin = loopback_url("SOTTO_PUBLIC_URL")?;
        if origin.path() != "/" || origin.scheme() != "https" {
            return Err(Error::Config(
                "access fixture requires a loopback HTTPS origin".into(),
            ));
        }
        let callback = loopback_url("SOTTO_E2E_ACCESS_CALLBACK")?;
        let issuer = origin.join(PREFIX).expect("fixed fixture path").to_string();
        let issuer_key = SigningKey::from_pkcs8_pem(&key_file("SOTTO_E2E_ACCESS_ISSUER_KEY")?)
            .map_err(|_| Error::Config("invalid fixture issuer key".into()))?;
        let deployment_key =
            VerifyingKey::from_public_key_pem(&key_file("SOTTO_E2E_ACCESS_DEPLOYMENT_KEY")?)
                .map_err(|_| Error::Config("invalid fixture deployment key".into()))?;
        Ok(Self(Arc::new(FixtureData {
            issuer,
            callback,
            issuer_key,
            deployment_key,
            records: Mutex::new(Records::default()),
        })))
    }

    pub fn router(self, state: AppState) -> Router {
        let routes = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/authorize", get(authorise))
            .route("/token", post(token))
            .route("/confirm", post(deployment::confirm))
            .route("/connection-status/{attempt}", get(deployment::status));
        Router::new()
            .nest(PREFIX, routes)
            .layer(Extension(self))
            .with_state(state)
    }

    fn url(&self, suffix: &str) -> String {
        format!("{}/{suffix}", self.0.issuer)
    }
}

fn env(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| Error::Config(format!("{name} is required for the fixture")))
}

fn loopback_url(name: &str) -> Result<Url> {
    let url = Url::parse(&env(name)?)
        .map_err(|_| Error::Config(format!("{name} must be a loopback URL")))?;
    let loopback = match url.host() {
        Some(url::Host::Domain("localhost")) => true,
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if !loopback
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Config(format!(
            "{name} must be a loopback URL without credentials"
        )));
    }
    Ok(url)
}

fn key_file(name: &str) -> Result<String> {
    std::fs::read_to_string(env(name)?)
        .map_err(|_| Error::Config(format!("could not read the key for {name}")))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("fixture clock after epoch")
        .as_secs()
}

async fn discovery(Extension(fixture): Extension<Fixture>) -> Json<Value> {
    Json(json!({
        "issuer": fixture.0.issuer,
        "authorization_endpoint": fixture.url("authorize"),
        "token_endpoint": fixture.url("token"),
        "jwks_uri": fixture.url("jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "grant_types_supported": ["authorization_code"],
        "id_token_signing_alg_values_supported": ["ES256"],
        "token_endpoint_auth_methods_supported": ["private_key_jwt"],
        "token_endpoint_auth_signing_alg_values_supported": ["ES256"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": ["openid"],
    }))
}

async fn jwks(Extension(fixture): Extension<Fixture>) -> Json<Value> {
    let key = fixture.0.issuer_key.verifying_key().to_encoded_point(false);
    let x = URL_SAFE_NO_PAD.encode(key.x().expect("uncompressed issuer key"));
    let y = URL_SAFE_NO_PAD.encode(key.y().expect("uncompressed issuer key"));
    Json(json!({"keys": [{
        "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256",
        "kid": ISSUER_KEY_ID, "x": x, "y": y,
    }]}))
}

#[derive(Deserialize)]
struct Authorise {
    client_id: String,
    redirect_uri: String,
    response_type: String,
    scope: String,
    state: String,
    nonce: String,
    code_challenge: String,
    code_challenge_method: String,
}

async fn authorise(
    user: AuthUser,
    Extension(fixture): Extension<Fixture>,
    Query(request): Query<Authorise>,
) -> Result<Redirect> {
    if request.client_id != CLIENT_ID
        || request.redirect_uri != fixture.0.callback.as_str()
        || request.response_type != "code"
        || request.scope != "openid"
        || request.state.is_empty()
        || request.nonce.is_empty()
        || request.code_challenge_method != "S256"
        || !URL_SAFE_NO_PAD
            .decode(&request.code_challenge)
            .is_ok_and(|challenge| challenge.len() == 32)
    {
        return Err(Error::BadRequest(
            "invalid fixture authorisation request".into(),
        ));
    }
    let now = now();
    let code = Uuid::new_v4().to_string();
    let mut records = fixture.0.records.lock().expect("fixture records");
    records.codes.retain(|_, code| code.expires > now);
    records.codes.insert(
        code.clone(),
        Code {
            user_id: user.user_id,
            challenge: request.code_challenge,
            nonce: request.nonce,
            expires: now + KEY_LIFETIME,
        },
    );
    let mut callback = fixture.0.callback.clone();
    callback
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &request.state);
    Ok(Redirect::to(callback.as_str()))
}

#[derive(Deserialize)]
struct Token {
    grant_type: String,
    code: String,
    redirect_uri: String,
    code_verifier: String,
    client_assertion_type: String,
    client_assertion: String,
}

#[derive(Deserialize)]
struct Assertion {
    iss: String,
    sub: String,
    aud: Value,
    iat: u64,
    exp: u64,
    jti: String,
}

fn assertion(raw: &str, fixture: &Fixture, now: u64) -> Option<Assertion> {
    let (signed, signature) = raw.rsplit_once('.')?;
    let (header, claims) = signed.split_once('.')?;
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).ok()?).ok()?;
    if header["alg"] != "ES256" {
        return None;
    }
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).ok()?).ok()?;
    fixture
        .0
        .deployment_key
        .verify(signed.as_bytes(), &signature)
        .ok()?;
    let claims: Assertion = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).ok()?).ok()?;
    let issuer = fixture.0.issuer.as_str();
    let correct_audience = claims.aud == issuer
        || claims
            .aud
            .as_array()
            .is_some_and(|values| values.len() == 1 && values[0] == issuer);
    if claims.iss != CLIENT_ID
        || claims.sub != CLIENT_ID
        || !correct_audience
        || claims.iat > now
        || claims.exp <= now
        || claims.exp.saturating_sub(claims.iat) > KEY_LIFETIME
        || claims.jti.is_empty()
    {
        return None;
    }
    Some(claims)
}

fn oauth_error(status: axum::http::StatusCode, code: &str) -> Response {
    (
        status,
        [(CACHE_CONTROL, "no-store")],
        Json(json!({"error": code})),
    )
        .into_response()
}

async fn token(Extension(fixture): Extension<Fixture>, Form(request): Form<Token>) -> Response {
    use axum::http::StatusCode;

    let now = now();
    let Some(assertion) = assertion(&request.client_assertion, &fixture, now).filter(|_| {
        request.client_assertion_type == "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"
    }) else {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
    };
    let mut records = fixture.0.records.lock().expect("fixture records");
    records.assertions.retain(|_, expires| *expires > now);
    if records.assertions.contains_key(&assertion.jti) {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
    }
    records.assertions.insert(assertion.jti, assertion.exp);
    let verifier = &request.code_verifier;
    let valid_verifier = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte));
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let valid_code = records
        .codes
        .get(&request.code)
        .is_some_and(|code| code.expires > now && code.challenge == challenge);
    if request.grant_type != "authorization_code"
        || request.redirect_uri != fixture.0.callback.as_str()
        || !valid_verifier
        || !valid_code
    {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let code = records
        .codes
        .remove(&request.code)
        .expect("validated fixture code");
    drop(records);
    let payload = json!({
        "iss": fixture.0.issuer, "sub": code.user_id, "aud": CLIENT_ID,
        "iat": now, "exp": now + KEY_LIFETIME, "nonce": code.nonce,
    });
    let header = json!({"alg": "ES256", "typ": "JWT", "kid": ISSUER_KEY_ID});
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(payload.to_string())
    );
    let signature: Signature = fixture.0.issuer_key.sign(signed.as_bytes());
    let id_token = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    (
        [(CACHE_CONTROL, "no-store")],
        Json(json!({
            "access_token": Uuid::new_v4().to_string(), "token_type": "Bearer",
            "expires_in": KEY_LIFETIME, "id_token": id_token, "scope": "openid",
        })),
    )
        .into_response()
}
