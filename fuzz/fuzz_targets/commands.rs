// SPDX-License-Identifier: GPL-3.0-or-later
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let _ = hq_watchman::telegram::commands::parse_command(data);
    let _ = hq_watchman::telegram::commands::parse_duration(data);
});
