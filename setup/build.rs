// SPDX-License-Identifier: GPL-3.0-or-later
fn main() {
    if let Err(e) = slint_build::compile("ui/app.slint") {
        eprintln!("slint: {e}");
        std::process::exit(1);
    }
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico")
            .set("ProductName", "HQ Watchman Setup")
            .set("FileDescription", "HQ Watchman Setup")
            .set("CompanyName", "Lev Burmistrov (Mahiron-hq)")
            .set(
                "LegalCopyright",
                "Copyright © 2026 Lev Burmistrov (Mahiron-hq), GPL-3.0-or-later",
            );
        if let Err(e) = res.compile() {
            eprintln!("resources: {e}");
            std::process::exit(1);
        }
    }
}
