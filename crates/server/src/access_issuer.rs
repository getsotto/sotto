//! Self-hosted OpenID Connect issuer for the registered Sotto Access deployment.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, FromRequestParts, Query, Request, State};
use axum::http::{header, StatusCode};
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
use url::{Host, Url};

use crate::auth::{session::hash_token, AuthUser};
use crate::config::{DeploymentMode, OAuthConfig};
use crate::error::{Error, Result};
use crate::state::AppState;

const PREFIX: &str = "/integrations/access/oidc";
const LIFETIME: u64 = 120;
const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
const CLIENT_ID: &str = "SOTTO_ACCESS_CLIENT_ID";
const CALLBACK: &str = "SOTTO_ACCESS_CALLBACK_URL";
const DEPLOYMENT_KEY: &str = "SOTTO_ACCESS_DEPLOYMENT_PUBLIC_KEY_FILE";
const ISSUER_KEY: &str = "SOTTO_ACCESS_ISSUER_PRIVATE_KEY_FILE";
const SETUP_CALLBACK: &str = "SOTTO_ACCESS_SETUP_CALLBACK_URL";
const PREVIOUS_KEY: &str = "SOTTO_ACCESS_ISSUER_PREVIOUS_PUBLIC_KEY_FILE";

pub struct AccessIssuer {
    pub(crate) origin: Url,
    issuer: Url,
    pub(crate) client_id: String,
    callback: Url,
    pub(crate) client_key: VerifyingKey,
    signing_key: SigningKey,
    previous_key: Option<VerifyingKey>,
    key_id: String,
    registration: Vec<u8>,
    pub(crate) setup_callback: Option<Url>,
}

impl AccessIssuer {
    pub fn from_env(mode: DeploymentMode, oauth: Option<&OAuthConfig>) -> Result<Option<Self>> {
        let previous_path = std::env::var_os(PREVIOUS_KEY).filter(|path| !path.is_empty());
        let setup_path = std::env::var_os(SETUP_CALLBACK).filter(|value| !value.is_empty());
        if previous_path.is_none()
            && setup_path.is_none()
            && [CLIENT_ID, CALLBACK, DEPLOYMENT_KEY, ISSUER_KEY]
                .iter()
                .all(|name| std::env::var_os(name).is_none())
        {
            return Ok(None);
        }
        if mode != DeploymentMode::SelfHosted {
            return Err(Error::Config(
                "the Access issuer requires self-hosted mode".into(),
            ));
        }
        let oauth =
            oauth.ok_or_else(|| Error::Config("the Access issuer requires GitHub OAuth".into()))?;
        let origin = configured_url("SOTTO_PUBLIC_URL")?;
        if origin.scheme() != "https" || origin.path() != "/" {
            return Err(Error::Config(
                "the Access issuer requires an HTTPS public origin".into(),
            ));
        }
        let web = configured_url("SOTTO_WEB_ORIGIN")?;
        if web != origin || oauth.web_origin.is_none() {
            return Err(Error::Config(
                "the Access issuer requires the Sotto web and public origins to match".into(),
            ));
        }
        let issuer = origin.join(PREFIX).expect("static issuer path");
        let callback = configured_url(CALLBACK)?;
        let setup_callback = setup_path
            .map(|_| configured_url(SETUP_CALLBACK))
            .transpose()?;
        if setup_callback
            .as_ref()
            .is_some_and(|setup| setup.origin() != callback.origin() || setup == &callback)
        {
            return Err(Error::Config(
                "Access setup requires a separate callback on the registered Access origin".into(),
            ));
        }
        let client_id = required(CLIENT_ID)?;
        if client_id.len() > 128
            || !client_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
        {
            return Err(Error::Config("invalid Access client ID".into()));
        }
        let signing_key = SigningKey::from_pkcs8_pem(&key_file(ISSUER_KEY)?)
            .map_err(|_| Error::Config("invalid Access issuer signing key".into()))?;
        let client_key = public_key(DEPLOYMENT_KEY)?;
        let previous_key = previous_path
            .map(|_| public_key(PREVIOUS_KEY))
            .transpose()?;
        if signing_key.verifying_key() == &client_key
            || previous_key
                .as_ref()
                .is_some_and(|key| key == &client_key || key == signing_key.verifying_key())
        {
            return Err(Error::Config(
                "Access requires distinct deployment and issuer keys".into(),
            ));
        }
        let key_id = key_id(signing_key.verifying_key());
        let registration = hash_token(&format!(
            "{issuer}\n{client_id}\n{callback}\n{}",
            self::key_id(&client_key)
        ));
        Ok(Some(Self {
            origin,
            issuer,
            client_id,
            callback,
            client_key,
            signing_key,
            previous_key,
            key_id,
            registration,
            setup_callback,
        }))
    }

    pub fn router(self, state: AppState) -> Router {
        let connections = if self.setup_callback.is_some() {
            crate::access_connections::router()
        } else {
            Router::new()
        };
        Router::new()
            .merge(connections)
            .nest(
                PREFIX,
                Router::new()
                    .route("/.well-known/openid-configuration", get(discovery))
                    .route("/jwks", get(jwks))
                    .route("/authorize", get(authorise))
                    .route("/token", post(token)),
            )
            .layer(DefaultBodyLimit::max(8192))
            .layer(Extension(Arc::new(self)))
            .with_state(state)
    }

    pub(crate) fn connection_registration(&self) -> Vec<u8> {
        let mut material = self.registration.clone();
        if let Some(callback) = &self.setup_callback {
            material.extend_from_slice(callback.as_str().as_bytes());
        }
        Sha256::digest(material).to_vec()
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{path}", self.issuer)
    }
}

fn required(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Config(format!("{name} is required for the Access issuer")))
}

fn configured_url(name: &str) -> Result<Url> {
    let value = required(name)?;
    let url = Url::parse(&value).map_err(|_| Error::Config(format!("invalid {name} URL")))?;
    let local = cfg!(feature = "e2e-mock-oauth")
        && match url.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            Some(Host::Domain("localhost")) => true,
            _ => false,
        };
    if !(url.scheme() == "https" || url.scheme() == "http" && local)
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Config(format!(
            "{name} requires an HTTPS URL without credentials, query or fragment"
        )));
    }
    if matches!(name, CALLBACK | SETUP_CALLBACK) && value != url.as_str() {
        return Err(Error::Config(
            "the Access callback must use its canonical URL form".into(),
        ));
    }
    Ok(url)
}

fn key_file(name: &str) -> Result<String> {
    std::fs::read_to_string(required(name)?)
        .map_err(|_| Error::Config(format!("could not read {name}")))
}

fn public_key(name: &str) -> Result<VerifyingKey> {
    VerifyingKey::from_public_key_pem(&key_file(name)?)
        .map_err(|_| Error::Config(format!("invalid key in {name}")))
}

fn key_id(key: &VerifyingKey) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(key.to_sec1_point(false).as_bytes()))
}

fn jwk(key: &VerifyingKey) -> Value {
    let point = key.to_sec1_point(false);
    json!({
        "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": key_id(key),
        "x": URL_SAFE_NO_PAD.encode(point.x().expect("uncompressed point")),
        "y": URL_SAFE_NO_PAD.encode(point.y().expect("uncompressed point")),
    })
}

async fn jwks(Extension(issuer): Extension<Arc<AccessIssuer>>) -> Json<Value> {
    let keys: Vec<_> = std::iter::once(issuer.signing_key.verifying_key())
        .chain(issuer.previous_key.iter())
        .map(jwk)
        .collect();
    Json(json!({"keys": keys}))
}

async fn discovery(Extension(issuer): Extension<Arc<AccessIssuer>>) -> Json<Value> {
    Json(json!({
        "issuer": issuer.issuer.as_str(),
        "authorization_endpoint": issuer.endpoint("authorize"),
        "token_endpoint": issuer.endpoint("token"),
        "jwks_uri": issuer.endpoint("jwks"),
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
    State(app): State<AppState>,
    Extension(issuer): Extension<Arc<AccessIssuer>>,
    Query(request): Query<Authorise>,
    raw: Request,
) -> Result<impl IntoResponse> {
    if request.client_id != issuer.client_id || request.redirect_uri != issuer.callback.as_str() {
        return Err(Error::BadRequest(
            "unregistered Access client or callback".into(),
        ));
    }
    if request.response_type != "code"
        || request.scope != "openid"
        || !(1..=512).contains(&request.state.len())
        || !(1..=512).contains(&request.nonce.len())
        || request.code_challenge_method != "S256"
        || request.code_challenge.len() != 43
        || !URL_SAFE_NO_PAD
            .decode(&request.code_challenge)
            .is_ok_and(|value| value.len() == 32)
    {
        return Err(Error::BadRequest(
            "invalid Access authorisation request".into(),
        ));
    }
    let (mut parts, _) = raw.into_parts();
    parts.headers.remove(header::AUTHORIZATION);
    let user = match AuthUser::from_request_parts(&mut parts, &app).await {
        Ok(user) => user,
        Err(Error::Unauthorized) => {
            // Use the existing same-origin web login. Its callback appends the Access state, so
            // keep it out of the return URI to avoid a duplicate state parameter.
            let mut resume = Url::parse(&issuer.endpoint("authorize")).expect("configured issuer");
            resume
                .query_pairs_mut()
                .append_pair("client_id", &request.client_id)
                .append_pair("redirect_uri", &request.redirect_uri)
                .append_pair("response_type", &request.response_type)
                .append_pair("scope", &request.scope)
                .append_pair("nonce", &request.nonce)
                .append_pair("code_challenge", &request.code_challenge)
                .append_pair("code_challenge_method", &request.code_challenge_method);
            let mut login = issuer
                .origin
                .join("/auth/github/login")
                .expect("fixed login path");
            login
                .query_pairs_mut()
                .append_pair("redirect_uri", resume.as_str())
                .append_pair("state", &request.state);
            return Ok((
                [
                    (header::CACHE_CONTROL, "no-store"),
                    (header::REFERRER_POLICY, "no-referrer"),
                ],
                Redirect::to(login.as_str()),
            ));
        }
        Err(error) => return Err(error),
    };
    let mut random = [0u8; 32];
    dryoc::rng::copy_randombytes(&mut random);
    let code = URL_SAFE_NO_PAD.encode(random);
    sqlx::query("DELETE FROM access_authorisation_codes WHERE expires_at <= now()")
        .execute(&app.pool)
        .await?;
    sqlx::query("INSERT INTO access_authorisation_codes (code_hash, registration, user_id, challenge, nonce, expires_at)
                 VALUES ($1, $2, $3, $4, $5, now() + $6::integer * interval '1 second')")
        .bind(hash_token(&code)).bind(&issuer.registration).bind(user.user_id)
        .bind(request.code_challenge).bind(request.nonce).bind(LIFETIME as i32)
        .execute(&app.pool).await?;
    let mut callback = issuer.callback.clone();
    callback
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &request.state);
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Redirect::to(callback.as_str()),
    ))
}

#[derive(Deserialize)]
struct Token {
    grant_type: String,
    code: String,
    redirect_uri: String,
    code_verifier: String,
    client_assertion_type: String,
    client_assertion: String,
    client_id: Option<String>,
}

#[derive(Deserialize)]
struct Assertion {
    iss: String,
    sub: String,
    aud: Value,
    iat: u64,
    exp: u64,
    nbf: Option<u64>,
    jti: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

fn verify_assertion(raw: &str, issuer: &AccessIssuer, now: u64) -> Option<Assertion> {
    let (signed, signature) = raw.rsplit_once('.')?;
    let (header, claims) = signed.split_once('.')?;
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).ok()?).ok()?;
    if header["alg"] != "ES256" || header.get("crit").is_some() {
        return None;
    }
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).ok()?).ok()?;
    issuer
        .client_key
        .verify(signed.as_bytes(), &signature)
        .ok()?;
    let claims: Assertion = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).ok()?).ok()?;
    let expected = issuer.issuer.as_str();
    let matches = claims.aud == expected
        || claims
            .aud
            .as_array()
            .is_some_and(|items| items.len() == 1 && items[0] == expected);
    (matches
        && claims.iss == issuer.client_id
        && claims.sub == issuer.client_id
        && claims.iat <= now
        && claims.exp > now
        && claims.exp.saturating_sub(claims.iat) <= LIFETIME
        && claims.nbf.is_none_or(|nbf| nbf <= now)
        && (1..=256).contains(&claims.jti.len()))
    .then_some(claims)
}

fn oauth_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(body),
    )
        .into_response()
}

async fn token(
    State(app): State<AppState>,
    Extension(issuer): Extension<Arc<AccessIssuer>>,
    form: std::result::Result<Form<Token>, axum::extract::rejection::FormRejection>,
) -> Result<Response> {
    let Ok(Form(request)) = form else {
        return Ok(oauth_response(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid_request"}),
        ));
    };
    let time = now();
    let Some(assertion) = verify_assertion(&request.client_assertion, &issuer, time).filter(|_| {
        request.client_assertion_type == ASSERTION_TYPE
            && request
                .client_id
                .as_ref()
                .is_none_or(|client_id| client_id == &issuer.client_id)
    }) else {
        return Ok(oauth_response(
            StatusCode::UNAUTHORIZED,
            json!({"error": "invalid_client"}),
        ));
    };
    let mut tx = app.pool.begin().await?;
    sqlx::query("DELETE FROM access_client_assertions WHERE expires_at <= now()")
        .execute(&mut *tx)
        .await?;
    let inserted = sqlx::query(
        "INSERT INTO access_client_assertions (assertion_hash, expires_at)
                                VALUES ($1, to_timestamp($2)) ON CONFLICT DO NOTHING",
    )
    .bind(hash_token(&format!(
        "{}\n{}",
        issuer.client_id, assertion.jti
    )))
    .bind(assertion.exp as f64)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Ok(oauth_response(
            StatusCode::UNAUTHORIZED,
            json!({"error": "invalid_client"}),
        ));
    }
    let verifier = &request.code_verifier;
    let valid_verifier = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte));
    let code: Option<(String, String)> = if request.grant_type == "authorization_code"
        && request.redirect_uri == issuer.callback.as_str()
        && valid_verifier
    {
        sqlx::query_as(
            "DELETE FROM access_authorisation_codes WHERE code_hash = $1
                        AND registration = $2 AND challenge = $3 AND expires_at > now()
                        RETURNING user_id, nonce",
        )
        .bind(hash_token(&request.code))
        .bind(&issuer.registration)
        .bind(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
        .fetch_optional(&mut *tx)
        .await?
    } else {
        None
    };
    tx.commit().await?;
    let Some((user_id, nonce)) = code else {
        return Ok(oauth_response(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid_grant"}),
        ));
    };
    let header = json!({"alg": "ES256", "typ": "JWT", "kid": issuer.key_id});
    let claims = json!({"iss": issuer.issuer.as_str(), "sub": user_id, "aud": issuer.client_id,
                        "iat": time, "exp": time + LIFETIME, "nonce": nonce});
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature: Signature = issuer.signing_key.sign(signed.as_bytes());
    let id_token = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    let mut token = [0u8; 32];
    dryoc::rng::copy_randombytes(&mut token);
    Ok(oauth_response(
        StatusCode::OK,
        json!({"id_token": id_token, "access_token": URL_SAFE_NO_PAD.encode(token),
        "token_type": "Bearer", "expires_in": LIFETIME, "scope": "openid"}),
    ))
}
