// SPDX-License-Identifier: GPL-3.0-or-later
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let _ = hq_watchman::steam::comments::parse_comments(data);
    let _ = hq_watchman::steam::comments::parse_guides_page(data);
});
