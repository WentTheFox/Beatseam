//! Accounts, sessions, passkeys and device sign-in (`packages/api/src/auth.ts`).
//!
//! Only the *shapes* live here. The input validation the zod schemas also performed (username pattern, password length,
//! TOTP format, "either code or recoveryCode") belongs to whoever accepts the request, and arrives with the Rust server.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MIN_PASSWORD_LENGTH: usize = 12;
pub const MAX_PASSWORD_LENGTH: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum RoleDto {
    Admin,
    #[default]
    User,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatusDto {
    /// True until the first account is fully set up; the first sign-up becomes the admin.
    pub setup_required: bool,
    /// Whether that first sign-up also needs the server's `BEATSEAM_SETUP_TOKEN`.
    pub setup_token_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupRequestDto {
    pub username: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedeemInviteRequestDto {
    pub invite_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordLoginRequestDto {
    pub username: String,
    pub password: String,
}

/// Send either `code` or `recovery_code`, not both.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotpLoginRequestDto {
    pub mfa_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetPasswordRequestDto {
    pub password: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TotpCodeRequestDto {
    pub code: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateUserRequestDto {
    pub username: String,
    pub display_name: String,
    #[serde(default)]
    pub role: RoleDto,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateUserRequestDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<RoleDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeySummaryDto {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub backed_up: bool,
    pub created_at_ms: f64,
    pub last_used_at_ms: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionVia {
    Session,
    /// A native app's bearer token.
    Device,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeDto {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub role: RoleDto,
    /// Can sign in again later: has a passkey, or a password plus TOTP. An unenrolled session can only set those up.
    pub enrolled: bool,
    pub has_password: bool,
    pub totp_enabled: bool,
    pub passkeys: Vec<PasskeySummaryDto>,
    pub recovery_codes_remaining: u64,
    /// When this session last proved who it is; sensitive changes need this to be recent.
    pub authenticated_at_ms: Option<f64>,
    /// When that sign-in stops counting as recent for credential changes (`authenticated_at_ms` plus the server's
    /// window); `None` if it never does (a device token).
    pub fresh_until_ms: Option<f64>,
    /// The server's clock when this was answered, so a client can correct for its own clock being off.
    pub server_time_ms: f64,
    pub via: SessionVia,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserSummaryDto {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub role: RoleDto,
    pub disabled: bool,
    pub enrolled: bool,
    pub created_at_ms: f64,
}

/// The invite token is shown once; whoever opens `/enroll?token=…` (or posts it to `/auth/invites/redeem`) can set up
/// the account.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteResultDto {
    pub invite_token: String,
    pub expires_at_ms: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateUserResultDto {
    pub user: UserSummaryDto,
    pub invite: InviteResultDto,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummaryDto {
    pub id: String,
    pub created_at_ms: f64,
    pub last_seen_at_ms: f64,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum LoginResultDto {
    Ok { me: MeDto },
    /// Password accepted; now needs a TOTP or recovery code.
    MfaRequired { mfa_token: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotpSetupDto {
    pub secret: String,
    pub otpauth_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryCodesDto {
    pub recovery_codes: Vec<String>,
}

// ---- Passkeys (WebAuthn). The browser/OS API produces and consumes these JSON shapes; the server only passes them
// through to the verifier.

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PasskeyRegisterVerifyRequestDto {
    /// The `RegistrationResponseJSON` from `navigator.credentials.create()`.
    pub response: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasskeyRenameRequestDto {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyLoginVerifyRequestDto {
    pub challenge_id: String,
    /// The `AuthenticationResponseJSON` from `navigator.credentials.get()`.
    pub response: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyLoginOptionsDto {
    /// Echo back with the signed response.
    pub challenge_id: String,
    /// `PublicKeyCredentialRequestOptionsJSON`. No allowed-credentials list: the user picks a passkey from their device.
    pub options: Map<String, Value>,
}

// ---- Device sign-in for native apps (RFC 8628-style): the app shows a code, the user approves it in a signed-in
// browser.

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCodeRequestDto {
    pub device_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCodeResultDto {
    /// Secret the app keeps and uses to wait for approval. Never shown to the user.
    pub device_code: String,
    /// Short code the user reads off the app and enters (or confirms) in the browser.
    pub user_code: String,
    /// Where to send the user, with the code filled in.
    pub verification_uri: String,
    pub expires_in_seconds: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceWaitRequestDto {
    pub device_code: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeviceWaitResultDto {
    /// Not decided yet - ask again straight away (the request is held open, so this is a long-poll, not a timer).
    Pending,
    /// Shown once: the bearer token for `Authorization: Bearer ...`.
    Approved { token: String, me: MeDto },
    Denied,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceLookupDto {
    pub device_name: String,
    pub requested_at_ms: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceApproveRequestDto {
    pub user_code: String,
    pub approve: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceTokenSummaryDto {
    pub id: String,
    pub name: String,
    pub created_at_ms: f64,
    pub last_used_at_ms: Option<f64>,
}
