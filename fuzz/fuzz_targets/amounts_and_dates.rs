//! Amount and date fields on their own: an accepted amount is within ±10^12 minor units and
//! has the currency asked for; an accepted date is a real day in range.
#![no_main]

use libfuzzer_sys::fuzz_target;
use recon_domain::Currency;
use recon_domain::money::MAX_ABS_MINOR;
use recon_import::dates::{DateFormat, DateOrder, Parsed};
use recon_import::number::{GroupStyle, Negative, NumberFormat};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    for (decimal, grouping, style) in [
        ('.', Some(','), GroupStyle::Western),
        ('.', Some(','), GroupStyle::Indian),
        (',', Some('.'), GroupStyle::Western),
        ('.', None, GroupStyle::Western),
    ] {
        let format = NumberFormat {
            decimal,
            grouping,
            group_style: style,
            negatives: vec![
                Negative::Minus,
                Negative::Parentheses,
                Negative::TrailingMinus,
            ],
            plus_sign: true,
        };
        for currency in [Currency::Inr, Currency::Jpy, Currency::Kwd] {
            if let Ok(m) = format.parse(text, currency) {
                assert!(m.minor().abs() <= MAX_ABS_MINOR);
                assert_eq!(m.currency(), currency);
            }
        }
    }
    for format in [
        DateFormat::ISO,
        DateFormat {
            order: None,
            separator: Some('/'),
            month_names: false,
            padded: false,
        },
        DateFormat {
            order: Some(DateOrder::Dmy),
            separator: Some('-'),
            month_names: true,
            padded: false,
        },
        DateFormat {
            order: Some(DateOrder::Ymd),
            separator: None,
            month_names: false,
            padded: true,
        },
    ] {
        if let Parsed::Date(d) = format.parse(text) {
            assert!((1990..=2100).contains(&d.year()));
        }
    }
});
