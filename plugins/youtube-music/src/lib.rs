//! YouTube Music guest: `playback.resolve` over the pinned client
//! ladder, `playback.candidates` over WEB_REMIX metadata search,
//! `catalog.suggest` query completions over WEB_REMIX
//! `music/get_search_suggestions`, and `radio.seed` automix paging
//! over the WEB_REMIX `next` endpoint (ABI 0.3.0).

mod candidates;
mod guest;
mod parse;
mod radio;
mod rungs;
mod suggest;

use guest::dispatch;

auqw_guest_sdk::export_plugin!(dispatch);
