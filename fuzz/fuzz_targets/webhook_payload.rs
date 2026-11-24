// SPDX-License-Identifier: GPL-3.0-or-later
#![no_main]

use hq_watchman::config::GithubOptions;
use hq_watchman::github::payload::{parse, Context};
use libfuzzer_sys::fuzz_target;

const EVENTS: &[&str] = &[
    "issues",
    "issue_comment",
    "pull_request",
    "pull_request_review",
    "pull_request_review_comment",
    "commit_comment",
    "discussion",
    "discussion_comment",
    "installation_repositories",
    "installation",
];

fuzz_target!(|data: &[u8]| {
    let Some((&first, body)) = data.split_first() else { return };
    let event = EVENTS[usize::from(first) % EVENTS.len()];
    let options = GithubOptions::default();
    let ctx = Context { admin_id: 1, options: &options, known_installations: &[1, 2] };
    let _ = parse(event, body, &ctx);
});
