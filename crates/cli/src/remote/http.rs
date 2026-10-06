//! The reqwest (blocking) implementation of [`SyncApi`].
//!
//! Every request carries the session as a bearer token. Status handling: 2xx parse; 304/404 map to
//! `None` where meaningful; 409/412 → [`Error::Conflict`] (the engine re-pulls); 403 →
//! [`Error::Forbidden`]; other non-2xx → [`Error::Server`]; transport failures → [`Error::Network`].

use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_TYPE, IF_NONE_MATCH};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

use super::api::{
    AccountBundle, BatchRequest, BatchResponse, CreatedMachineToken, CreatedShare, EnvironmentInfo,
    ExportChunk, ExportManifest, GrantView, Invited, MachineTokenInfo, Me, MemberInfo,
    NewEnvironment, NewOrg, NewProject, NewShare, OrgInfo, RemovalReceipt, RotateRequest,
    RotateResponse, Snapshot, SyncApi,
};

/// Row shapes for the two "list of ids" endpoints (each returns `[{ "user_id"|"env_id": ... }]`).
#[derive(serde::Deserialize)]
struct HolderRow {
    user_id: String,
}
#[derive(serde::Deserialize)]
struct EnvRefRow {
    env_id: String,
}

pub struct HttpClient {
    base_url: String,
    token: String,
    http: Client,
}

impl HttpClient {
    pub fn new(base_url: String, token: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client with static config builds");
        Self {
            base_url,
            token,
            http,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Discover hosted capability metadata without making local commands depend on it.
    pub fn server_info(&self) -> Result<Option<super::api::ServerInfo>> {
        let resp = self
            .http
            .get(self.url("/server/info"))
            .send()
            .map_err(net)?;
        parse_optional_json(resp)
    }

    /// Read the account's Cloud state. A 404 is the supported fallback for older servers.
    pub fn eligibility(&self) -> Result<Option<super::api::EligibilityView>> {
        let resp = self
            .http
            .get(self.url("/account/eligibility"))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse_optional_json(resp)
    }
}

fn net(e: reqwest::Error) -> Error {
    Error::Network(e.to_string())
}

/// Map a non-success response to an error, distinguishing concurrency conflicts and auth.
fn classify_status(status: StatusCode, code: Option<&str>, body: String) -> Error {
    let fallback = |message: &str| {
        if body.trim().is_empty() {
            message.to_owned()
        } else {
            body.clone()
        }
    };
    match status {
        StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => {
            Error::Conflict(format!("{status}: {body}"))
        }
        StatusCode::UNAUTHORIZED => Error::Server("unauthorised - run `sotto login`".into()),
        StatusCode::FORBIDDEN => Error::Forbidden(format!("{status}: {body}")),
        StatusCode::PAYMENT_REQUIRED if code == Some("cloud_eligibility_required") => {
            Error::CloudEligibility(fallback("hosted Cloud eligibility is required"))
        }
        StatusCode::PAYMENT_REQUIRED if code == Some("quota") => {
            Error::Quota(fallback("quota exceeded"))
        }
        StatusCode::PAYMENT_REQUIRED => Error::Input(fallback("payment required")),
        StatusCode::TOO_MANY_REQUESTS => Error::RateLimited(fallback("try again later")),
        StatusCode::SERVICE_UNAVAILABLE => Error::Unavailable(fallback("server unavailable")),
        _ => Error::Server(format!("{status}: {body}")),
    }
}

fn server_error(resp: Response) -> Error {
    let status = resp.status();
    let code = resp
        .headers()
        .get("x-sotto-error-code")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = resp.text().unwrap_or_default();
    classify_status(status, code.as_deref(), body)
}

/// Parse a successful body, or turn a non-2xx response into an error.
fn parse<T: DeserializeOwned>(resp: Response) -> Result<T> {
    if resp.status().is_success() {
        resp.json().map_err(|e| Error::Server(e.to_string()))
    } else {
        Err(server_error(resp))
    }
}

/// Parse an optional discovery response. Older reverse proxies can serve the SPA document with a
/// successful status for an unknown API route; that is the same compatibility case as a 404.
fn parse_optional_json<T: DeserializeOwned>(resp: Response) -> Result<Option<T>> {
    if !resp.status().is_success() {
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        return Err(server_error(resp));
    }

    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = resp.text().map_err(|e| Error::Server(e.to_string()))?;
    let trimmed = body.trim_start();
    if content_type
        .as_deref()
        .is_some_and(|value| !value.to_ascii_lowercase().contains("json"))
        || trimmed.starts_with('<')
    {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|e| Error::Server(e.to_string()))
}

/// Expect a 2xx with no body of interest.
fn ok(resp: Response) -> Result<()> {
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(server_error(resp))
    }
}

impl SyncApi for HttpClient {
    fn me(&self) -> Result<Me> {
        let resp = self
            .http
            .get(self.url("/auth/me"))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn put_account(&self, bundle: &AccountBundle) -> Result<()> {
        let resp = self
            .http
            .put(self.url("/account"))
            .bearer_auth(&self.token)
            .json(bundle)
            .send()
            .map_err(net)?;
        ok(resp)
    }

    fn reset_account(&self, bundle: &AccountBundle) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/account/reset"))
            .bearer_auth(&self.token)
            .json(bundle)
            .send()
            .map_err(net)?;
        ok(resp)
    }

    fn get_account(&self) -> Result<Option<AccountBundle>> {
        let resp = self
            .http
            .get(self.url("/account"))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        parse(resp).map(Some)
    }

    fn start_export(&self) -> Result<ExportManifest> {
        let resp = self
            .http
            .post(self.url("/account/export"))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn export_chunk(&self, export_id: &str, index: usize) -> Result<ExportChunk> {
        let resp = self
            .http
            .get(self.url(&format!("/account/export/{export_id}/chunks/{index}")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn create_project(&self, project: &NewProject) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/projects"))
            .bearer_auth(&self.token)
            .json(project)
            .send()
            .map_err(net)?;
        ok(resp)
    }

    fn create_environment(&self, project_id: &str, env: &NewEnvironment) -> Result<()> {
        let resp = self
            .http
            .post(self.url(&format!("/projects/{project_id}/environments")))
            .bearer_auth(&self.token)
            .json(env)
            .send()
            .map_err(net)?;
        ok(resp)
    }

    fn list_environments(&self, project_id: &str) -> Result<Vec<EnvironmentInfo>> {
        let resp = self
            .http
            .get(self.url(&format!("/projects/{project_id}/environments")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn snapshot(&self, env_id: &str, if_none_match: Option<i64>) -> Result<Option<Snapshot>> {
        let mut req = self
            .http
            .get(self.url(&format!("/environments/{env_id}/secrets")))
            .bearer_auth(&self.token);
        if let Some(rev) = if_none_match {
            req = req.header(IF_NONE_MATCH, format!("\"{rev}\""));
        }
        let resp = req.send().map_err(net)?;
        if resp.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        parse(resp).map(Some)
    }

    fn write_secrets(&self, env_id: &str, batch: &BatchRequest) -> Result<BatchResponse> {
        let resp = self
            .http
            .post(self.url(&format!("/environments/{env_id}/secrets")))
            .bearer_auth(&self.token)
            .json(batch)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn create_share(&self, share: &NewShare) -> Result<CreatedShare> {
        let resp = self
            .http
            .post(self.url("/shares"))
            .bearer_auth(&self.token)
            .json(share)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn list_shares(&self) -> Result<super::api::ShareList> {
        let resp = self
            .http
            .get(self.url("/shares"))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn revoke_share(&self, token: &str) -> Result<()> {
        let resp = self
            .http
            .delete(self.url(&format!("/shares/{token}")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(Error::NotFound(format!("share `{token}`")));
        }
        ok(resp)
    }

    fn create_org(&self, org: &NewOrg) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/orgs"))
            .bearer_auth(&self.token)
            .json(org)
            .send()
            .map_err(net)?;
        ok(resp)
    }

    fn list_orgs(&self) -> Result<Vec<OrgInfo>> {
        let resp = self
            .http
            .get(self.url("/orgs"))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn invite_member(&self, org_id: &str, email: &str) -> Result<Invited> {
        let resp = self
            .http
            .post(self.url(&format!("/orgs/{org_id}/invites")))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "email": email }))
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn list_members(&self, org_id: &str) -> Result<Vec<MemberInfo>> {
        let resp = self
            .http
            .get(self.url(&format!("/orgs/{org_id}/members")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn create_grant(&self, env_id: &str, user_id: &str, enc_vault_key: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url(&format!("/environments/{env_id}/grants")))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "user_id": user_id, "enc_vault_key": enc_vault_key }))
            .send()
            .map_err(net)?;
        ok(resp)
    }

    fn list_grant_holders(&self, env_id: &str) -> Result<Vec<String>> {
        let resp = self
            .http
            .get(self.url(&format!("/environments/{env_id}/grants")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        let holders: Vec<HolderRow> = parse(resp)?;
        Ok(holders.into_iter().map(|h| h.user_id).collect())
    }

    fn member_env_grants(&self, org_id: &str, user_id: &str) -> Result<Vec<String>> {
        let resp = self
            .http
            .get(self.url(&format!("/orgs/{org_id}/members/{user_id}/grants")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        let envs: Vec<EnvRefRow> = parse(resp)?;
        Ok(envs.into_iter().map(|e| e.env_id).collect())
    }

    fn org_entitlements(&self, org_id: &str) -> Result<super::api::Entitlements> {
        let resp = self
            .http
            .get(self.url(&format!("/orgs/{org_id}/entitlements")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn org_audit(&self, org_id: &str, limit: Option<i64>) -> Result<Vec<super::api::AuditEvent>> {
        let mut url = self.url(&format!("/orgs/{org_id}/audit"));
        if let Some(limit) = limit {
            url.push_str(&format!("?limit={limit}"));
        }
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn list_history(&self, env_id: &str) -> Result<Vec<super::api::HistoryRow>> {
        let resp = self
            .http
            .get(self.url(&format!("/environments/{env_id}/history")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn rotate(&self, env_id: &str, req: &RotateRequest) -> Result<RotateResponse> {
        let resp = self
            .http
            .post(self.url(&format!("/environments/{env_id}/rotate")))
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn remove_member(&self, org_id: &str, user_id: &str) -> Result<RemovalReceipt> {
        let resp = self
            .http
            .delete(self.url(&format!("/orgs/{org_id}/members/{user_id}")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        // A server older than the removal receipt answers `204` with no body. The member is gone
        // and that server revoked nothing, so that is an empty receipt, not a parse failure to
        // report after the removal has already happened.
        if resp.status() == StatusCode::NO_CONTENT {
            return Ok(RemovalReceipt {
                revoked_tokens: Vec::new(),
                grants_deleted: 0,
            });
        }
        parse(resp)
    }

    fn grant_org_key(&self, org_id: &str, user_id: &str, enc_org_key: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url(&format!("/orgs/{org_id}/members/{user_id}/org-key")))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "enc_org_key": enc_org_key }))
            .send()
            .map_err(net)?;
        ok(resp)
    }

    fn create_machine_token(
        &self,
        env_id: &str,
        name: &str,
        public_key: &str,
        enc_vault_key: &str,
        expires_in_days: Option<u32>,
    ) -> Result<CreatedMachineToken> {
        let mut body = serde_json::json!({
            "name": name,
            "public_key": public_key,
            "enc_vault_key": enc_vault_key,
        });
        // Only when asked: the lifetime policy (default and range) lives on the server.
        if let Some(days) = expires_in_days {
            body["expires_in_days"] = days.into();
        }
        let resp = self
            .http
            .post(self.url(&format!("/environments/{env_id}/tokens")))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn list_machine_tokens(&self, env_id: &str) -> Result<Vec<MachineTokenInfo>> {
        let resp = self
            .http
            .get(self.url(&format!("/environments/{env_id}/tokens")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        parse(resp)
    }

    fn revoke_machine_token(&self, env_id: &str, token_id: &str) -> Result<()> {
        let resp = self
            .http
            .delete(self.url(&format!("/environments/{env_id}/tokens/{token_id}")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        ok(resp)
    }

    fn get_grant(&self, env_id: &str) -> Result<Option<String>> {
        let resp = self
            .http
            .get(self.url(&format!("/environments/{env_id}/grant")))
            .bearer_auth(&self.token)
            .send()
            .map_err(net)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        parse::<GrantView>(resp).map(|g| Some(g.enc_vault_key))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use super::*;

    /// Answer exactly one request with `response`, verbatim, and return the base URL to reach it.
    fn serve_once(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Drain the request head first, or closing early can reset the client mid-send.
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).expect("read request") <= 2 {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).expect("write");
        });
        format!("http://{addr}")
    }

    #[test]
    fn a_204_removal_from_an_older_server_is_an_empty_receipt() {
        let base = serve_once("HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        let receipt = HttpClient::new(base, "session".into())
            .remove_member("org", "user")
            .expect("a 204 is a completed removal, not a parse failure");
        assert!(receipt.revoked_tokens.is_empty());
        assert_eq!(receipt.grants_deleted, 0);
    }

    #[test]
    fn cloud_error_codes_have_stable_cli_categories() {
        assert!(matches!(
            classify_status(
                StatusCode::PAYMENT_REQUIRED,
                Some("cloud_eligibility_required"),
                "renewal needed".into()
            ),
            Error::CloudEligibility(message) if message == "renewal needed"
        ));
        assert!(matches!(
            classify_status(StatusCode::PAYMENT_REQUIRED, Some("quota"), "limit".into()),
            Error::Quota(message) if message == "limit"
        ));
        assert!(matches!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, None, "slow down".into()),
            Error::RateLimited(message) if message == "slow down"
        ));
        assert!(matches!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE, Some("unavailable"), "offline".into()),
            Error::Unavailable(message) if message == "offline"
        ));
        assert!(matches!(
            classify_status(StatusCode::PAYMENT_REQUIRED, None, "payment required".into()),
            Error::Input(message) if message == "payment required"
        ));
        assert!(matches!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE, None, String::new()),
            Error::Unavailable(message) if message == "server unavailable"
        ));
    }

    #[test]
    fn discovery_treats_an_older_spa_fallback_as_missing() {
        let base = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<html>old app</html>",
        );
        assert!(HttpClient::new(base, "session".into())
            .server_info()
            .expect("SPA fallback should not be an error")
            .is_none());
    }

    #[test]
    fn revoking_an_unknown_share_is_not_found() {
        let base = serve_once(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nmissing",
        );
        let result = HttpClient::new(base, "session".into()).revoke_share("typo");
        assert!(matches!(result, Err(Error::NotFound(message)) if message == "share `typo`"));
    }
}
