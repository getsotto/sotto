//! A mock [`OAuthProvider`] compiled in only under the `e2e-mock-oauth` feature, so an external
//! browser (Playwright, driving the real `sotto-server` binary) can complete a login without
//! touching real GitHub.
//!
//! There is no server-side identity table to seed: the authorisation `code` a caller sends *is*
//! the desired subject, so any caller (Playwright's intercepted redirect, or the seed fixture's
//! direct HTTP calls) picks its test identity just by choosing a code, e.g. `"e2e-owner"`. Two
//! logins with the same code resolve to the same user, since [`upsert_user`]'s stable key is
//! `(oauth_provider, oauth_subject)` - matching the idempotent-rerun pattern
//! `crates/server/tests/auth.rs` already uses.
//!
//! [`upsert_user`]: crate::auth::routes

use async_trait::async_trait;

use crate::auth::oauth::{Identity, OAuthProvider};
use crate::error::Result;

pub struct MockOAuth;

#[async_trait]
impl OAuthProvider for MockOAuth {
    async fn exchange_code(&self, code: &str) -> Result<Identity> {
        Ok(Identity {
            provider: "github".into(),
            subject: code.to_string(),
            email: Some(format!("{code}@e2e.sotto.test")),
        })
    }
}
