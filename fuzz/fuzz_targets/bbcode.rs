// SPDX-License-Identifier: GPL-3.0-or-later
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let text = hq_watchman::steam::bbcode::to_text(data);
    let _ = hq_watchman::text::external_html(&hq_watchman::text::clean(&text));
});
