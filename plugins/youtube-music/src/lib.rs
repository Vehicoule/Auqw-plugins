//! YouTube Music guest: `playback.resolve` over the pinned client
//! ladder, `playback.candidates` over WEB_REMIX metadata search, and
//! `radio.seed` automix paging over the WEB_REMIX `next` endpoint
//! (ABI 0.3.0).

mod candidates;
mod guest;
mod parse;
mod radio;
mod rungs;

use guest::dispatch;

auqw_guest_sdk::export_plugin!(dispatch);
