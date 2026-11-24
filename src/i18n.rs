// SPDX-License-Identifier: GPL-3.0-or-later
//! Language of bot messages (Russian or English), switched at run time.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

static ENGLISH: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    Ru,
    En,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Self::Ru => "ru",
            Self::En => "en",
        }
    }
}

pub fn set(lang: Lang) {
    ENGLISH.store(lang == Lang::En, Ordering::Relaxed);
}

#[cfg(not(test))]
pub fn en() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

#[cfg(test)]
thread_local! {
    static TEST_EN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Tests run in parallel, so each test thread keeps its own language.
#[cfg(test)]
pub fn en() -> bool {
    let _ = ENGLISH.load(Ordering::Relaxed);
    TEST_EN.with(std::cell::Cell::get)
}

#[cfg(test)]
pub fn set_for_test(lang: Lang) {
    TEST_EN.with(|c| c.set(lang == Lang::En));
}

/// Picks the static string of the current language: `tr!("рус", "eng")`.
#[macro_export]
macro_rules! tr {
    ($ru:literal, $en:literal $(,)?) => {
        if $crate::i18n::en() {
            $en
        } else {
            $ru
        }
    };
}

/// Formats in the current language: `trf!("{n} шт.", "{n} pcs")`.
#[macro_export]
macro_rules! trf {
    ($ru:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::en() {
            format!($en $(, $arg)*)
        } else {
            format!($ru $(, $arg)*)
        }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn picks_language() {
        let n = 3;
        super::set_for_test(super::Lang::En);
        assert_eq!(crate::trf!("{n} шт.", "{n} pcs"), "3 pcs");
        assert_eq!(crate::tr!("да", "yes"), "yes");
        super::set_for_test(super::Lang::Ru);
        assert_eq!(crate::trf!("{n} шт.", "{n} pcs"), "3 шт.");
    }
}
