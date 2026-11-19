//! Port of `packages/core/src/discord`: the Discord Rich Presence SET_ACTIVITY frame, transport-agnostic.

mod rich_presence;

pub use rich_presence::{DEFAULT_DISCORD_APPLICATION_ID, DiscordNowPlayingInfo, DiscordPresenceBridge, NonceGenerator, build_set_activity_frame};
