//! `make model-watch`: prints how the built-in model tables differ from the
//! models.dev catalog, as Markdown.

use std::fs;
use std::process::ExitCode;

use caudra_providers::model_watch::{audit, fetch_catalog};
use color_eyre::eyre::{Result, WrapErr, eyre};
use jiff::civil::Date;
use jiff::{ToSpan, Zoned};

const CATALOG_FLAG: &str = "--catalog";
const SINCE_FLAG: &str = "--since";
const TODAY_FLAG: &str = "--today";
const DEFAULT_LOOKBACK_DAYS: i64 = 90;
const USAGE: &str =
    "usage: caudra-model-watch [--catalog PATH] [--since YYYY-MM-DD] [--today YYYY-MM-DD]";

#[derive(Default)]
struct Options {
    catalog: Option<String>,
    since: Option<Date>,
    today: Option<Date>,
}

fn parse_options(mut args: impl Iterator<Item = String>) -> Result<Options> {
    let mut options = Options::default();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| eyre!("{flag} needs a value\n{USAGE}"))?;
        let date = || {
            value
                .parse::<Date>()
                .wrap_err_with(|| format!("{flag} takes a date such as 2026-10-01"))
        };
        match flag.as_str() {
            CATALOG_FLAG => options.catalog = Some(value.clone()),
            SINCE_FLAG => options.since = Some(date()?),
            TODAY_FLAG => options.today = Some(date()?),
            _ => return Err(eyre!("unknown flag {flag}\n{USAGE}")),
        }
    }
    Ok(options)
}

fn run() -> Result<String> {
    let options = parse_options(std::env::args().skip(1))?;
    let today = options.today.unwrap_or_else(|| Zoned::now().date());
    let since = match options.since {
        Some(since) => since,
        None => today.checked_sub(DEFAULT_LOOKBACK_DAYS.days())?,
    };
    let catalog = match options.catalog {
        Some(path) => fs::read_to_string(&path).wrap_err_with(|| format!("reading {path}"))?,
        None => fetch_catalog().wrap_err("fetching the models.dev catalog")?,
    };
    let audit = audit(&catalog, today, since).wrap_err("reading the models.dev catalog")?;
    Ok(audit.to_string())
}

fn main() -> ExitCode {
    match run() {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("caudra-model-watch: {error:#}");
            ExitCode::FAILURE
        }
    }
}
