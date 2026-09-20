//! YouTube Music guest: `playback.resolve` over the pinned client
//! ladder and `playback.candidates` over WEB_REMIX metadata search
//! (ABI 0.2.0).

mod candidates;
mod guest;
mod parse;
mod rungs;

use guest::dispatch;

auqw_guest_sdk::export_plugin!(dispatch);
