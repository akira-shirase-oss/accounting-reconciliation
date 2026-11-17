//! A declared profile with every R1 feature: Windows-1252, `;`, day-first dates (and the
//! same layout with the order undeclared), Indian grouping, parentheses and trailing
//! minus, money-out and money-in columns, newest-first balances.
#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use recon_domain::{AccountId, Currency};
use recon_import::dates::{DateFormat, DateOrder};
use recon_import::decode::TextEncoding;
use recon_import::number::{GroupStyle, Negative, NumberFormat};
use recon_import::profile::{AmountColumns, RowOrder, StatementColumns};
use recon_import::{AccountContext, CsvLimits, StatementParser, StatementProfile};

#[path = "common.rs"]
mod common;

fn profile(order: Option<DateOrder>) -> StatementProfile {
    StatementProfile {
        encoding: TextEncoding::Windows1252,
        delimiter: ';',
        header: ["D", "N", "R", "O", "I", "B"].map(str::to_owned).to_vec(),
        columns: StatementColumns {
            booking_date: "D".into(),
            value_date: None,
            amount: AmountColumns::Split {
                money_out: "O".into(),
                money_in: "I".into(),
            },
            currency: None,
            narration: "N".into(),
            reference: Some("R".into()),
            counterparty: None,
            balance_after: Some("B".into()),
        },
        date_format: DateFormat {
            order,
            separator: Some('/'),
            month_names: false,
            padded: false,
        },
        number_format: NumberFormat {
            decimal: '.',
            grouping: Some(','),
            group_style: GroupStyle::Indian,
            negatives: vec![Negative::Parentheses, Negative::TrailingMinus],
            plus_sign: true,
        },
        row_order: RowOrder::NewestFirst,
    }
}

fn run(order: Option<DateOrder>, input: &[u8], chunk: usize) -> String {
    let compiled = Arc::new(profile(order).compile().expect("the fuzz profile compiles"));
    let mut parser = StatementParser::with_profile(
        AccountContext {
            account: AccountId::from_uuid(uuid::Uuid::from_u128(2)),
            currency: Currency::Inr,
        },
        compiled,
        CsvLimits::default(),
    );
    let mut lines = Vec::new();
    for part in input.chunks(chunk) {
        if let Err(e) = parser.push(part, &mut |l| lines.push(l)) {
            return format!("refused {e:?} after {}", lines.len());
        }
    }
    match parser.finish(&mut |l| lines.push(l)) {
        Ok(s) => format!("{s:?} {lines:?}"),
        Err(e) => format!("refused {e:?} after {}", lines.len()),
    }
}

fuzz_target!(|data: &[u8]| {
    let (chunk, rest) = common::split(data);
    let input = [b"D;N;R;O;I;B\n".as_slice(), rest].concat();
    for order in [Some(DateOrder::Dmy), None] {
        assert_eq!(
            run(order, &input, input.len().max(1)),
            run(order, &input, chunk)
        );
    }
});
