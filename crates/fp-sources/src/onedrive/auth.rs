//! Signing in to a Microsoft account with a code (OAuth 2.0 device
//! authorization grant), renewing the sign-in, and naming the account.
//!
//! The headset shows a code; the user enters it at
//! [`VERIFICATION_URL`] on a phone or PC and signs in there, while the
//! headset asks every few seconds whether that happened. The tokens it gets
//! read the user's files (`Files.Read`) and renew themselves
//! (`offline_access`). These calls block and belong on job threads.

use crate::error::{Error, Result};
use crate::http::{HttpClient, HttpOptions};
use crate::urlutil::encode_form_value;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LOGIN: &str = "https://login.microsoftonline.com";
const GRAPH: &str = "https://graph.microsoft.com/v1.0";
/// Where the code is entered, as Microsoft names it when it doesn't say.
pub const VERIFICATION_URL: &str = "https://microsoft.com/devicelogin";
/// What the tokens may do: read the user's files, and renew themselves.
const SCOPES: &str = "Files.Read offline_access";
/// Personal Microsoft accounts.
pub const DEFAULT_TENANT: &str = "consumers";
const JSON_LIMIT: u64 = 8 << 20;

/// The Azure app (a public client) the headset signs in as, built in with
/// `FRAMEPLAYERX_ONEDRIVE_CLIENT_ID` at compile time. Empty when the build
/// has none: a source then needs its own ([`crate::OneDriveConfig::client_id`]).
pub const CLIENT_ID: &str = match option_env!("FRAMEPLAYERX_ONEDRIVE_CLIENT_ID") {
    Some(id) => id,
    None => "",
};

/// The app to sign in as: `own` when given, else the built-in one.
pub fn client_id(own: &str) -> Option<&str> {
    let own = own.trim();
    let id = if own.is_empty() {
        CLIENT_ID.trim()
    } else {
        own
    };
    (!id.is_empty()).then_some(id)
}

/// Whether an app or directory ID can go in a request as it is.
fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// What a signed-in source keeps: the token for Microsoft Graph, and the
/// one that renews it. Secrets, so Debug leaves them out.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OneDriveToken {
    pub access_token: String,
    pub refresh_token: String,
    /// When the access token stops working, in Unix seconds.
    #[serde(default)]
    pub expires_at: u64,
}

impl fmt::Debug for OneDriveToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OneDriveToken")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl OneDriveToken {
    /// The access token stops working within five minutes: renew it first.
    pub fn expiring(&self) -> bool {
        self.expires_at <= now() + 300
    }
}

/// A sign-in waiting for its code to be entered. The device code is a
/// secret, so Debug leaves it out.
#[derive(Clone)]
pub struct DeviceCode {
    /// What the user types in.
    pub user_code: String,
    /// Where they type it.
    pub verification_uri: String,
    /// Unix seconds.
    pub expires_at: u64,
    /// Seconds to wait between polls.
    pub interval: u64,
    device_code: String,
    client_id: String,
    tenant: String,
}

impl DeviceCode {
    pub fn expired(&self) -> bool {
        now() >= self.expires_at
    }

    /// The app and directory it signs in to, for the source to keep.
    pub fn client(&self) -> (&str, &str) {
        (&self.client_id, &self.tenant)
    }
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCode")
            .field("user_code", &self.user_code)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Microsoft's sign-in service and Graph.
#[derive(Clone, Debug)]
pub struct OneDriveAuth {
    login: String,
    graph: String,
}

impl Default for OneDriveAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl OneDriveAuth {
    pub fn new() -> Self {
        Self {
            login: LOGIN.into(),
            graph: GRAPH.into(),
        }
    }

    /// Both services on a loopback server, for hermetic tests: sign-in at
    /// `base`, Graph at `base/v1.0`. The real ones aren't configurable.
    #[doc(hidden)]
    pub fn with_test_endpoint(base: &str) -> Result<Self> {
        let parsed = url::Url::parse(base)
            .map_err(|_| Error::Config("invalid OneDrive test endpoint".into()))?;
        if parsed.scheme() != "http"
            || !matches!(
                parsed.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            )
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(Error::Config(
                "OneDrive test endpoint must be HTTP loopback".into(),
            ));
        }
        let base = base.trim_end_matches('/');
        Ok(Self {
            login: base.into(),
            graph: format!("{base}/v1.0"),
        })
    }

    /// Where Graph is, with no trailing slash.
    pub fn graph(&self) -> &str {
        &self.graph
    }

    fn options() -> HttpOptions {
        HttpOptions {
            max_retries: 1,
            read_timeout: Duration::from_secs(15),
            ..Default::default()
        }
    }

    /// Posts a form to the sign-in service: the status and JSON answer. Its
    /// errors come as 400 with an `error` code, so those are answers too.
    fn post(&self, tenant: &str, path: &str, fields: &[(&str, &str)]) -> Result<(u16, Value)> {
        if !plain_id(tenant) {
            return Err(Error::Config("invalid Microsoft directory".into()));
        }
        let url = format!("{}/{tenant}/oauth2/v2.0/{path}", self.login);
        let body = fields
            .iter()
            .map(|(k, v)| format!("{k}={}", encode_form_value(v)))
            .collect::<Vec<_>>()
            .join("&");
        let reply = HttpClient::new(Self::options(), None).request(
            "POST",
            &url,
            &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Accept", "application/json"),
            ],
            Some(body.as_bytes()),
            JSON_LIMIT,
        )?;
        let value: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
        if !value.is_object() {
            // Not the sign-in service answering (a proxy's page, an outage).
            reply.error_for_status(&url)?;
            return Err(Error::parse("OneDrive sign-in", "not a JSON answer"));
        }
        Ok((reply.status, value))
    }

    /// Asks for a code to enter at [`DeviceCode::verification_uri`], signing
    /// in as app `client_id` to directory `tenant` (`consumers` for personal
    /// accounts).
    pub fn start(&self, client_id: &str, tenant: &str) -> Result<DeviceCode> {
        if !plain_id(client_id) {
            return Err(Error::Config("invalid OneDrive app (client) ID".into()));
        }
        let (status, v) = self.post(
            tenant,
            "devicecode",
            &[("client_id", client_id), ("scope", SCOPES)],
        )?;
        if status != 200 {
            return Err(sign_in_error(&v));
        }
        let user_code =
            string(&v, "user_code").ok_or_else(|| Error::parse("OneDrive sign-in", "no code"))?;
        let device_code = string(&v, "device_code")
            .ok_or_else(|| Error::parse("OneDrive sign-in", "no device code"))?;
        let verification_uri = string(&v, "verification_uri")
            .filter(|u| u.starts_with("https://"))
            .unwrap_or_else(|| VERIFICATION_URL.into());
        let expires_in = v.get("expires_in").and_then(Value::as_u64).unwrap_or(900);
        Ok(DeviceCode {
            user_code,
            verification_uri,
            expires_at: now().saturating_add(expires_in),
            interval: v
                .get("interval")
                .and_then(Value::as_u64)
                .unwrap_or(5)
                .clamp(1, 60),
            device_code,
            client_id: client_id.into(),
            tenant: tenant.into(),
        })
    }

    /// Whether the code was entered and the sign-in finished: the tokens
    /// then, `None` while it is still waiting.
    pub fn poll(&self, code: &DeviceCode) -> Result<Option<OneDriveToken>> {
        if code.expired() {
            return Err(Error::Auth("OneDrive code expired".into()));
        }
        let (status, v) = self.post(
            &code.tenant,
            "token",
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", &code.client_id),
                ("device_code", &code.device_code),
            ],
        )?;
        if status == 200 {
            return token(&v, None).map(Some);
        }
        match string(&v, "error").as_deref() {
            Some("authorization_pending" | "slow_down") => Ok(None),
            _ => Err(sign_in_error(&v)),
        }
    }

    /// New tokens for a source signed in before. A sign-in Microsoft no
    /// longer accepts (password changed, access taken back) is
    /// [`Error::SignedOut`].
    pub fn refresh(
        &self,
        client_id: &str,
        tenant: &str,
        old: &OneDriveToken,
    ) -> Result<OneDriveToken> {
        if old.refresh_token.is_empty() {
            return Err(Error::SignedOut("OneDrive".into()));
        }
        let (status, v) = self.post(
            tenant,
            "token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("refresh_token", &old.refresh_token),
                ("scope", SCOPES),
            ],
        )?;
        if status == 200 {
            return token(&v, Some(&old.refresh_token));
        }
        match string(&v, "error").as_deref() {
            Some("invalid_grant" | "interaction_required" | "unauthorized_client") => {
                Err(Error::SignedOut("OneDrive".into()))
            }
            _ => Err(sign_in_error(&v)),
        }
    }

    /// Who the drive belongs to, as Microsoft names them: shows the sign-in
    /// works.
    pub fn account(&self, access_token: &str) -> Result<String> {
        let url = format!("{}/me/drive?$select=id,owner,driveType", self.graph);
        let client = HttpClient::new_with_headers(
            Self::options(),
            vec![
                ("Authorization".into(), format!("Bearer {access_token}")),
                ("Accept".into(), "application/json".into()),
            ],
            &self.graph,
        );
        let reply = client
            .request("GET", &url, &[], None, JSON_LIMIT)?
            .error_for_status(&url)?;
        let v: Value = serde_json::from_slice(&reply.body)?;
        let owner = v.pointer("/owner/user");
        Ok(owner
            .and_then(|u| string(u, "email").or_else(|| string(u, "displayName")))
            .unwrap_or_else(|| "OneDrive".into()))
    }
}

/// Tokens from a token answer. Microsoft may leave out a new refresh token,
/// and the one used goes on working then.
fn token(v: &Value, old_refresh: Option<&str>) -> Result<OneDriveToken> {
    let access_token = string(v, "access_token")
        .ok_or_else(|| Error::parse("OneDrive sign-in", "no access token"))?;
    let refresh_token = string(v, "refresh_token")
        .or_else(|| old_refresh.map(str::to_owned))
        .ok_or_else(|| Error::parse("OneDrive sign-in", "no refresh token"))?;
    let expires_in = v.get("expires_in").and_then(|e| {
        e.as_u64()
            .or_else(|| e.as_str().and_then(|s| s.parse().ok()))
    });
    Ok(OneDriveToken {
        access_token,
        refresh_token,
        expires_at: now().saturating_add(expires_in.unwrap_or(3600)),
    })
}

/// The error a sign-in answer names, with Microsoft's description of it.
fn sign_in_error(v: &Value) -> Error {
    let code = string(v, "error").unwrap_or_else(|| "unknown error".into());
    match code.as_str() {
        "expired_token" => Error::Auth("OneDrive code expired".into()),
        "authorization_declined" | "access_denied" => {
            Error::Auth("OneDrive sign-in was declined".into())
        }
        _ => {
            // The description's first line says what went wrong; the rest
            // is trace and correlation IDs.
            let description = string(v, "error_description").unwrap_or_default();
            let first = description.lines().next().unwrap_or_default().trim();
            Error::Auth(if first.is_empty() {
                format!("OneDrive sign-in: {code}")
            } else {
                format!("OneDrive sign-in: {first}")
            })
        }
    }
}

fn string(v: &Value, field: &str) -> Option<String> {
    v.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
