//! Differential test: every DTO against the zod schemas it mirrors (`tools/gen-api-golden.mjs` ->
//! `tests/data/dto_golden.json`). For each schema, every input zod accepts must deserialize into the Rust DTO and
//! serialize back to what zod produced, and structurally invalid inputs must be refused.
//!
//! Two deliberate differences, both compatible (see `docs/api-compatibility.md`): the Rust DTOs carry shapes, not zod's
//! value constraints (lengths, ranges, patterns), and a missing nullable key reads as `null`; and a lyric line's
//! `translation: null` is omitted when serialized (the field's doc explains why).

use beatseam_api::auth::*;
use beatseam_api::lastfm::*;
use beatseam_api::mutations::*;
use beatseam_api::sync::*;
use beatseam_api::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Number, Value};

/// Fields that are nullable *and* optional in zod: the TypeScript may send an explicit `null`, the Rust DTO leaves the
/// key out (same meaning on the wire, see the field docs), so an explicit null in the expected value equals an absent
/// key.
const NULL_MEANS_ABSENT: &[&str] = &["translation", "album", "durationSeconds"];

/// JSON numbers compare by value (`5` and `5.0` are the same number to every consumer). `nullable_optional` says
/// whether the schema under test has fields where null equals absent.
fn normalize(v: Value, nullable_optional: bool) -> Value {
    match v {
        Value::Number(n) => Value::Number(Number::from_f64(n.as_f64().unwrap()).unwrap()),
        Value::Array(items) => Value::Array(items.into_iter().map(|i| normalize(i, nullable_optional)).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter().filter(|(k, v)| !(nullable_optional && v.is_null() && NULL_MEANS_ABSENT.contains(&k.as_str()))).map(|(k, v)| (k, normalize(v, nullable_optional))).collect(),
        ),
        other => other,
    }
}

fn check<T: Serialize + DeserializeOwned>(case: &Value) -> usize {
    let name = case["name"].as_str().unwrap();
    // Only the schemas that actually have such fields get the leniency; everywhere else null must survive.
    let lenient = matches!(name, "lyrics" | "lastFmNowPlayingRequest");
    for (i, valid) in case["valid"].as_array().unwrap().iter().enumerate() {
        let parsed: T = serde_json::from_value(valid["input"].clone()).unwrap_or_else(|e| panic!("{name}[{i}] should deserialize: {e}\n{}", valid["input"]));
        let back = serde_json::to_value(&parsed).unwrap();
        assert_eq!(normalize(back, lenient), normalize(valid["parsed"].clone(), lenient), "{name}[{i}] round trip\ninput: {}", valid["input"]);
    }
    let mut refused = 0;
    for (i, invalid) in case["invalid"].as_array().unwrap().iter().enumerate() {
        if invalid.get("skip").is_some() {
            continue; // zod enforces a constraint the DTO leaves to whoever accepts the request
        }
        assert!(serde_json::from_value::<T>(invalid["input"].clone()).is_err(), "{name} invalid[{i}] should be refused: {}", invalid["input"]);
        refused += 1;
    }
    refused
}

#[test]
fn every_dto_matches_its_zod_schema() {
    let golden: Value = serde_json::from_str(include_str!("data/dto_golden.json")).unwrap();
    let (mut seen, mut refused) = (0, 0);
    for case in golden.as_array().unwrap() {
        seen += 1;
        refused += match case["name"].as_str().unwrap() {
            "root" => check::<RootDto>(case),
            "track" => check::<TrackDto>(case),
            "playlist" => check::<PlaylistDto>(case),
            "lyrics" => check::<LyricsDto>(case),
            "lyricsFile" => check::<LyricsFileDto>(case),
            "lyricsSource" => check::<LyricsSourceDto>(case),
            "lyricsAssignRequest" => check::<LyricsAssignRequestDto>(case),
            "lyricsReportRequest" => check::<LyricsReportRequestDto>(case),
            "lyricsReport" => check::<LyricsReportDto>(case),
            "lyricsSaveRequest" => check::<LyricsSaveRequestDto>(case),
            "page" => check::<Page<TrackDto>>(case),
            "jobCounts" => check::<JobCountsDto>(case),
            "scanStatus" => check::<ScanStatusDto>(case),
            "scanJob" => check::<ScanJobDto>(case),
            "scanFailure" => check::<ScanFailureDto>(case),
            "authStatus" => check::<AuthStatusDto>(case),
            "setupRequest" => check::<SetupRequestDto>(case),
            "redeemInviteRequest" => check::<RedeemInviteRequestDto>(case),
            "passwordLoginRequest" => check::<PasswordLoginRequestDto>(case),
            "totpLoginRequest" => check::<TotpLoginRequestDto>(case),
            "setPasswordRequest" => check::<SetPasswordRequestDto>(case),
            "totpCodeRequest" => check::<TotpCodeRequestDto>(case),
            "createUserRequest" => check::<CreateUserRequestDto>(case),
            "updateUserRequest" => check::<UpdateUserRequestDto>(case),
            "passkeySummary" => check::<PasskeySummaryDto>(case),
            "me" => check::<MeDto>(case),
            "userSummary" => check::<UserSummaryDto>(case),
            "inviteResult" => check::<InviteResultDto>(case),
            "createUserResult" => check::<CreateUserResultDto>(case),
            "sessionSummary" => check::<SessionSummaryDto>(case),
            "loginResult" => check::<LoginResultDto>(case),
            "totpSetup" => check::<TotpSetupDto>(case),
            "recoveryCodes" => check::<RecoveryCodesDto>(case),
            "passkeyRegisterVerifyRequest" => check::<PasskeyRegisterVerifyRequestDto>(case),
            "passkeyRenameRequest" => check::<PasskeyRenameRequestDto>(case),
            "passkeyLoginVerifyRequest" => check::<PasskeyLoginVerifyRequestDto>(case),
            "passkeyLoginOptions" => check::<PasskeyLoginOptionsDto>(case),
            "deviceCodeRequest" => check::<DeviceCodeRequestDto>(case),
            "deviceCodeResult" => check::<DeviceCodeResultDto>(case),
            "deviceWaitRequest" => check::<DeviceWaitRequestDto>(case),
            "deviceWaitResult" => check::<DeviceWaitResultDto>(case),
            "deviceLookup" => check::<DeviceLookupDto>(case),
            "deviceApproveRequest" => check::<DeviceApproveRequestDto>(case),
            "deviceTokenSummary" => check::<DeviceTokenSummaryDto>(case),
            "lastFmStatus" => check::<LastFmStatusDto>(case),
            "lastFmConnectResult" => check::<LastFmConnectResultDto>(case),
            "lastFmNowPlayingRequest" => check::<LastFmNowPlayingRequestDto>(case),
            "lastFmScrobblesRequest" => check::<LastFmScrobblesRequestDto>(case),
            "mutation" => check::<MutationDto>(case),
            "mutationsRequest" => check::<MutationsRequestDto>(case),
            "mutationResult" => check::<MutationResultDto>(case),
            "mutationsResponse" => check::<MutationsResponseDto>(case),
            "userPlaylist" => check::<UserPlaylistDto>(case),
            "playbackState" => check::<PlaybackStateDto>(case),
            "userSetting" => check::<UserSettingDto>(case),
            "syncChange" => check::<SyncChangeDto>(case),
            "syncChangesResponse" => check::<SyncChangesResponseDto>(case),
            "syncSnapshotResponse" => check::<SyncSnapshotResponseDto>(case),
            "resyncRequired" => check::<ResyncRequiredDto>(case),
            other => panic!("no Rust DTO is mapped to the zod schema {other:?}: add it to this test"),
        };
    }
    assert_eq!(seen, 59);
    assert!(refused >= 10, "only {refused} structurally invalid inputs were refused");
}
