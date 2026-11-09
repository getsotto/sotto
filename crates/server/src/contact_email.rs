//! Transactional transport for lifecycle-contact verification messages.
//!
//! The Resend endpoint is fixed to prevent credential forwarding to arbitrary hosts. Provider
//! response bodies are bounded and never logged or returned to the customer.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::contact_verification::{
    canonical_email, ContactVerificationSender, SendOutcome, VerificationMessage,
};
use crate::error::{Error, Result};
use crate::notifications::{DeliveryOutcome, NoticeDelivery, NoticeSender};

pub struct ResendSender {
    client: reqwest::Client,
    api_key: String,
    from_address: String,
    endpoint: String,
    cloud_account_url: String,
}

impl ResendSender {
    pub fn new(api_key: String, from_address: String, web_origin: String) -> Result<Self> {
        Self::with_endpoint(
            api_key,
            from_address,
            web_origin,
            "https://api.resend.com/emails".into(),
        )
    }

    pub(crate) fn with_endpoint(
        api_key: String,
        from_address: String,
        web_origin: String,
        endpoint: String,
    ) -> Result<Self> {
        if !api_key.starts_with("re_") || api_key.len() < 8 || api_key.len() > 256 {
            return Err(Error::Config("RESEND_API_KEY is invalid".into()));
        }
        if canonical_email(&from_address).is_err() {
            return Err(Error::Config(
                "SOTTO_CLOUD_CONTACT_EMAIL_FROM is invalid".into(),
            ));
        }
        let origin = url::Url::parse(&web_origin)
            .map_err(|_| Error::Config("SOTTO_WEB_ORIGIN is invalid".into()))?;
        if !(origin.scheme() == "https"
            || (origin.scheme() == "http"
                && matches!(origin.host_str(), Some("localhost" | "127.0.0.1" | "::1"))))
            || origin.host_str().is_none()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !origin.username().is_empty()
            || origin.password().is_some()
        {
            return Err(Error::Config("SOTTO_WEB_ORIGIN is invalid".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Config("contact email HTTP client could not be created".into()))?;
        Ok(Self {
            client,
            api_key,
            from_address,
            endpoint,
            cloud_account_url: format!("{}/cloud", web_origin.trim_end_matches('/')),
        })
    }
}

#[derive(Serialize)]
struct ResendEmailRequest<'a> {
    from: &'a str,
    to: [&'a str; 1],
    subject: &'static str,
    text: String,
}

#[derive(Deserialize)]
struct ResendAccepted {
    id: String,
}

#[async_trait]
impl ContactVerificationSender for ResendSender {
    async fn send(&self, message: VerificationMessage) -> SendOutcome {
        let body = ResendEmailRequest {
            from: &self.from_address,
            to: [&message.destination],
            subject: "Verify your Sotto Cloud email address",
            text: format!(
                "Use this one-time link to verify your Sotto Cloud lifecycle email contact:\n\n{}\n\nThe link expires in 30 minutes. If you did not request this, you can ignore this message.",
                message.verification_url
            ),
        };
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("Idempotency-Key", &message.message_id)
            .json(&body)
            .send()
            .await;
        let Ok(response) = response else {
            return SendOutcome::Unknown;
        };
        let status = response.status();
        if status.is_success() {
            if response
                .content_length()
                .is_some_and(|length| length > 2048)
            {
                return SendOutcome::Unknown;
            }
            let mut response = response;
            let mut response_body = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) if response_body.len().saturating_add(chunk.len()) <= 2048 => {
                        response_body.extend_from_slice(&chunk);
                    }
                    Ok(Some(_)) => return SendOutcome::Unknown,
                    Ok(None) => break,
                    Err(_) => return SendOutcome::Unknown,
                }
            }
            return match serde_json::from_slice::<ResendAccepted>(&response_body) {
                Ok(accepted) if !accepted.id.trim().is_empty() => SendOutcome::Accepted {
                    provider_message_id: accepted.id,
                },
                _ => SendOutcome::Unknown,
            };
        }
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            SendOutcome::Unknown
        } else {
            SendOutcome::Rejected
        }
    }
}

#[async_trait]
impl NoticeSender for ResendSender {
    async fn send(&mut self, delivery: NoticeDelivery) -> DeliveryOutcome {
        if delivery.message_id.trim().is_empty() || delivery.message_id.len() > 256 {
            return DeliveryOutcome::PermanentFailure {
                code: "message_identity_invalid".into(),
            };
        }
        let Some(subject) = notice_subject(&delivery.kind) else {
            return DeliveryOutcome::PermanentFailure {
                code: "notice_kind_unsupported".into(),
            };
        };
        let mut text = format!(
            "{}\n\n{}\n\nManage your Cloud account: {}",
            delivery.content.title, delivery.content.detail, self.cloud_account_url
        );
        if let Some(epoch) = delivery.content.effective_at_epoch {
            text.push_str(&format!(
                "\n\nEffective date (UTC): {}",
                format_utc_epoch(epoch)
            ));
        }
        if let Some(epoch) = delivery.content.deadline_epoch {
            text.push_str(&format!("\nDeadline (UTC): {}", format_utc_epoch(epoch)));
        }
        if let Some(amount) = delivery.content.amount_pence {
            text.push_str(&format!("\nAmount: £{}.{:02}", amount / 100, amount % 100));
        }
        let body = ResendEmailRequest {
            from: &self.from_address,
            to: [&delivery.destination],
            subject,
            text,
        };
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("Idempotency-Key", &delivery.message_id)
            .json(&body)
            .send()
            .await;
        let Ok(response) = response else {
            return DeliveryOutcome::Unknown {
                code: "provider_outcome_unknown".into(),
            };
        };
        let status = response.status();
        if status.is_success() {
            if response
                .content_length()
                .is_some_and(|length| length > 2048)
            {
                return DeliveryOutcome::Unknown {
                    code: "provider_response_invalid".into(),
                };
            }
            let mut response = response;
            let mut response_body = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) if response_body.len().saturating_add(chunk.len()) <= 2048 => {
                        response_body.extend_from_slice(&chunk);
                    }
                    Ok(Some(_)) | Err(_) => {
                        return DeliveryOutcome::Unknown {
                            code: "provider_response_invalid".into(),
                        }
                    }
                    Ok(None) => break,
                }
            }
            return match serde_json::from_slice::<ResendAccepted>(&response_body) {
                Ok(accepted) if !accepted.id.trim().is_empty() && accepted.id.len() <= 256 => {
                    DeliveryOutcome::Accepted {
                        provider_message_id: accepted.id,
                    }
                }
                _ => DeliveryOutcome::Unknown {
                    code: "provider_response_invalid".into(),
                },
            };
        }
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            DeliveryOutcome::Retry {
                code: "provider_temporarily_unavailable".into(),
            }
        } else {
            DeliveryOutcome::PermanentFailure {
                code: "provider_rejected_request".into(),
            }
        }
    }
}

fn format_utc_epoch(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let shifted_days = days + 719_468;
    let era = shifted_days.div_euclid(146_097);
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

fn notice_subject(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "checkout_outcome" => "Sotto Cloud checkout update",
        "failed_renewal" | "sponsored_renewal_failure" | "sponsored_renewal_action" => {
            "Action needed for Sotto Cloud billing"
        }
        "recovery_end" => "Your Sotto Cloud recovery period is ending soon",
        "export_deadline" => "Your Sotto Cloud export deadline",
        "sponsorship_change" => "Your Sotto Cloud sponsorship changed",
        "payer_transfer_invitation" => "Sotto Cloud payer transfer request",
        "payer_transfer_outcome" => "Sotto Cloud payer transfer update",
        "price_change" | "founding_expiry" => "A Sotto Cloud price change is scheduled",
        "recovery_resolved" => "Your Sotto Cloud renewal is up to date",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_resend(
        status: u16,
        payload: String,
        expected_idempotency_key: &'static str,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let address = listener.local_addr().expect("mock address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept provider request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 2048];
            let mut body_start = None;
            let mut body_len = 0;
            loop {
                let count = stream
                    .read(&mut chunk)
                    .await
                    .expect("read provider request");
                assert_ne!(
                    count, 0,
                    "provider client closed before sending full request"
                );
                request.extend_from_slice(&chunk[..count]);
                if body_start.is_none() {
                    if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        body_start = Some(index + 4);
                        let headers =
                            String::from_utf8_lossy(&request[..index]).to_ascii_lowercase();
                        body_len = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .and_then(|value| value.parse::<usize>().ok())
                            .expect("content length header");
                    }
                }
                if body_start.is_some_and(|start| request.len() >= start + body_len) {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with("POST /emails HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer re_test_api_key"));
            assert!(request
                .to_ascii_lowercase()
                .contains(&format!("idempotency-key: {expected_idempotency_key}")));
            assert!(request.contains("to@example.test"));
            assert!(request.contains("contact_token="));
            let reason = if (200..300).contains(&status) {
                "OK"
            } else {
                "Service Error"
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{address}/emails")
    }

    #[tokio::test]
    async fn resend_uses_durable_message_id_and_classifies_provider_responses() {
        let endpoint = mock_resend(
            200,
            r#"{"id":"provider-message-1"}"#.into(),
            "contact-verification:test",
        )
        .await;
        let sender = ResendSender::with_endpoint(
            "re_test_api_key".into(),
            "no-reply@example.test".into(),
            "https://app.example.test".into(),
            endpoint,
        )
        .expect("sender config");
        assert_eq!(
            sender
                .send(VerificationMessage {
                    message_id: "contact-verification:test".into(),
                    destination: "to@example.test".into(),
                    verification_url: "https://app.example.test/cloud#contact_token=token-value"
                        .into(),
                    expires_at_epoch: 1_900_000_000,
                })
                .await,
            SendOutcome::Accepted {
                provider_message_id: "provider-message-1".into()
            }
        );

        let endpoint = mock_resend(
            401,
            r#"{"message":"secret provider detail"}"#.into(),
            "contact-verification:rejected",
        )
        .await;
        let sender = ResendSender::with_endpoint(
            "re_test_api_key".into(),
            "no-reply@example.test".into(),
            "https://app.example.test".into(),
            endpoint,
        )
        .expect("sender config");
        let rejected = sender
            .send(VerificationMessage {
                message_id: "contact-verification:rejected".into(),
                destination: "to@example.test".into(),
                verification_url: "https://app.example.test/cloud#contact_token=other-token".into(),
                expires_at_epoch: 1_900_000_000,
            })
            .await;
        assert_eq!(rejected, SendOutcome::Rejected);
        assert!(!format!("{rejected:?}").contains("secret provider detail"));

        let endpoint = mock_resend(
            429,
            r#"{"message":"limited"}"#.into(),
            "contact-verification:retry",
        )
        .await;
        let sender = ResendSender::with_endpoint(
            "re_test_api_key".into(),
            "no-reply@example.test".into(),
            "https://app.example.test".into(),
            endpoint,
        )
        .expect("sender config");
        assert_eq!(
            sender
                .send(VerificationMessage {
                    message_id: "contact-verification:retry".into(),
                    destination: "to@example.test".into(),
                    verification_url: "https://app.example.test/cloud#contact_token=retry-token"
                        .into(),
                    expires_at_epoch: 1_900_000_000,
                })
                .await,
            SendOutcome::Unknown
        );

        let endpoint = mock_resend(200, "x".repeat(2049), "contact-verification:oversize").await;
        let sender = ResendSender::with_endpoint(
            "re_test_api_key".into(),
            "no-reply@example.test".into(),
            "https://app.example.test".into(),
            endpoint,
        )
        .expect("sender config");
        assert_eq!(
            sender
                .send(VerificationMessage {
                    message_id: "contact-verification:oversize".into(),
                    destination: "to@example.test".into(),
                    verification_url: "https://app.example.test/cloud#contact_token=oversize"
                        .into(),
                    expires_at_epoch: 1_900_000_000,
                })
                .await,
            SendOutcome::Unknown
        );
    }

    #[tokio::test]
    async fn lifecycle_notice_uses_stable_idempotency_key() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let address = listener.local_addr().expect("mock address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            let mut body_start = None;
            let mut body_length = 0;
            loop {
                let count = stream.read(&mut buffer).await.expect("read request");
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
                if body_start.is_none() {
                    if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        body_start = Some(index + 4);
                        let headers =
                            String::from_utf8_lossy(&request[..index]).to_ascii_lowercase();
                        body_length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .and_then(|length| length.parse::<usize>().ok())
                            .expect("content length");
                    }
                }
                if body_start.is_some_and(|start| request.len() >= start + body_length) {
                    break;
                }
            }
            let body_start = body_start.expect("body start");
            let headers = String::from_utf8_lossy(&request[..body_start]).to_ascii_lowercase();
            assert!(headers.contains("idempotency-key: notice-1"));
            let body: serde_json::Value =
                serde_json::from_slice(&request[body_start..body_start + body_length])
                    .expect("request body");
            assert_eq!(body["to"][0], "to@example.test");
            assert!(body["text"]
                .as_str()
                .unwrap()
                .contains("Recovery is available"));
            assert!(body["text"]
                .as_str()
                .unwrap()
                .contains("https://app.example.test/cloud"));
            assert!(body["text"]
                .as_str()
                .unwrap()
                .contains("Effective date (UTC): 2027-01-15 08:00"));
            let payload = r#"{"id":"provider-msg-1"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        let mut sender = ResendSender::with_endpoint(
            "re_test_api_key".into(),
            "no-reply@example.test".into(),
            "https://app.example.test".into(),
            format!("http://{address}/emails"),
        )
        .expect("sender config");
        assert_eq!(
            NoticeSender::send(
                &mut sender,
                NoticeDelivery {
                    message_id: "notice-1".into(),
                    destination: "to@example.test".into(),
                    kind: "failed_renewal".into(),
                    content: crate::notifications::NoticeContent {
                        title: "Renewal failed".into(),
                        detail: "Recovery is available until the displayed deadline.".into(),
                        effective_at_epoch: Some(1_800_000_000),
                        deadline_epoch: Some(1_800_100_000),
                        amount_pence: None,
                        price_interval: None,
                    },
                },
            )
            .await,
            DeliveryOutcome::Accepted {
                provider_message_id: "provider-msg-1".into()
            }
        );
        server.await.expect("mock server completes");
    }

    #[test]
    fn lifecycle_dates_are_rendered_as_utc_calendar_dates() {
        assert_eq!(format_utc_epoch(0), "1970-01-01 00:00");
        assert_eq!(format_utc_epoch(1_800_000_000), "2027-01-15 08:00");
    }
}
