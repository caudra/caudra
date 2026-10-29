//! What a turn, and a whole session, cost.
//!
//! Rates live on the model. Turning them into a bill happens here: the
//! wall-clock schedules that scale them, and how a stored session's total comes
//! back. The rule everything else leans on is that a turn is priced once, when
//! it runs, and that number is what gets stored, summed and shown. History is
//! never re-priced, because rates move.

use std::collections::HashMap;
use std::fmt;

use caudra_storage::sessions::{StoredTokenUsage, add_cost};
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::model::{Billing, Model, ModelPricing, TokenUsage};

/// Marks a figure a subscription covered: what the same tokens are worth at API
/// rates, with no invoice behind it. Every place that prints one uses this.
pub const NOT_BILLED_MARK: &str = "\u{2248}";
const HOURS_PER_DAY: u8 = 24;
const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_DAY: i64 = HOURS_PER_DAY as i64 * SECONDS_PER_HOUR;
/// Multiplier of a provider that bills the same rate around the clock.
pub(crate) const FLAT_RATE: f64 = 1.0;

/// Rates that move with the wall clock. DeepSeek doubles everything during its
/// peak UTC hours, so model tables quote the off-peak rates and the provider's
/// schedule scales the bill inside its windows. The next price change is then a
/// constant edit instead of new code.
///
/// One multiplier is enough while providers move all four rates together.
#[derive(Debug)]
pub struct PricingSchedule {
    windows: &'static [PricingWindow],
    multiplier: f64,
}

/// Half-open `[start, end)` in whole UTC hours. `start > end` wraps past
/// midnight, so a window never needs splitting in two.
#[derive(Debug)]
pub struct PricingWindow {
    start: u8,
    end: u8,
}

impl PricingWindow {
    pub const fn start(&self) -> u8 {
        self.start
    }

    pub const fn end(&self) -> u8 {
        self.end
    }

    /// `hours(22, 2)` wraps past midnight. Every window is a `const`, so a typo
    /// here is a build error rather than a mispriced turn.
    pub const fn hours(start: u8, end: u8) -> Self {
        assert!(start < HOURS_PER_DAY, "window starts after the day ends");
        assert!(end <= HOURS_PER_DAY, "window ends after the day ends");
        assert!(start != end, "window covers no time at all");
        Self { start, end }
    }

    fn contains(&self, hour: u8) -> bool {
        if self.start < self.end {
            hour >= self.start && hour < self.end
        } else {
            hour >= self.start || hour < self.end
        }
    }
}

/// `01:00-04:00`, the shape provider docs quote peak hours in.
impl fmt::Display for PricingWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:00-{:02}:00", self.start, self.end)
    }
}

impl PricingSchedule {
    pub const fn windows(&self) -> &'static [PricingWindow] {
        self.windows
    }

    pub const fn multiplier(&self) -> f64 {
        self.multiplier
    }

    pub const fn new(windows: &'static [PricingWindow], multiplier: f64) -> Self {
        assert!(
            !windows.is_empty(),
            "a schedule with no windows never applies; drop it instead"
        );
        assert!(
            multiplier > FLAT_RATE,
            "model tables quote the off-peak rates, so a schedule only ever adds a surcharge"
        );
        Self {
            windows,
            multiplier,
        }
    }

    pub(crate) fn multiplier_at(&self, at: Timestamp) -> f64 {
        let hour = (at.as_second().rem_euclid(SECONDS_PER_DAY) / SECONDS_PER_HOUR) as u8;
        if self.windows.iter().any(|w| w.contains(hour)) {
            self.multiplier
        } else {
            FLAT_RATE
        }
    }
}

/// `2x during 01:00-04:00, 06:00-10:00 UTC`. Docs and hints render this rather
/// than restate the hours in prose that drifts.
impl fmt::Display for PricingSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x during ", self.multiplier)?;
        for (i, window) in self.windows.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{window}")?;
        }
        f.write_str(" UTC")
    }
}

/// Whole UTC days `from <= day < until`, open on either end left `None`.
/// `until` is the first day the period no longer covers: a promotion "through
/// November 30" is `until(2026-12-01)`, and a shutdown "on November 16" is
/// `until(2026-11-16)`. Each edge falls at 00:00 UTC. Providers name a day
/// without a timezone, so an instant near midnight may land a day off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    pub from: Option<Date>,
    pub until: Option<Date>,
}

impl Period {
    pub const fn from(day: Date) -> Self {
        Self {
            from: Some(day),
            until: None,
        }
    }

    pub const fn until(day: Date) -> Self {
        Self {
            from: None,
            until: Some(day),
        }
    }

    /// `from` must come before `until`. jiff compares no dates in a `const`,
    /// so a test holds every table to it instead.
    pub const fn between(from: Date, until: Date) -> Self {
        Self {
            from: Some(from),
            until: Some(until),
        }
    }

    pub fn contains(&self, at: Timestamp) -> bool {
        self.contains_day(utc_day(at))
    }

    pub fn contains_day(&self, day: Date) -> bool {
        self.from.is_none_or(|from| from <= day) && self.until.is_none_or(|until| day < until)
    }

    /// Whether an edge still lies after `at`, so what the period decides can
    /// still change.
    pub(crate) fn moves_after(&self, at: Timestamp) -> bool {
        let day = utc_day(at);
        [self.from, self.until]
            .into_iter()
            .flatten()
            .any(|edge| edge > day)
    }
}

fn utc_day(at: Timestamp) -> Date {
    at.to_zoned(TimeZone::UTC).date()
}

/// Rates a provider sets for a span of days: a list price that starts on a
/// date (`Period::from`), an introductory price that ends on one
/// (`Period::until`), or both. Inside `during` every turn bills at `pricing`,
/// tiers included, and outside it at the entry's own rates, so a build shipped
/// before either edge stays right after it.
#[derive(Debug)]
pub struct PricePeriod {
    /// The `ModelEntry` prefix whose rates the period sets.
    pub prefix: &'static str,
    pub during: Period,
    pub pricing: ModelPricing,
}

/// An entry's rates on both sides of its [`PricePeriod`]. A model billed from
/// the static table carries them while an edge is still ahead, so each turn
/// pays the side it ends on.
#[derive(Debug, Clone, Copy)]
pub struct DatedRates {
    pub period: &'static PricePeriod,
    pub otherwise: &'static ModelPricing,
}

impl DatedRates {
    pub(crate) fn at(&self, at: Timestamp) -> &'static ModelPricing {
        if self.period.during.contains(at) {
            &self.period.pricing
        } else {
            self.otherwise
        }
    }
}

/// A session's bill, kept apart by who pays it. Both are `None` until something
/// priced shows up, so callers show no cost instead of a made up "$0.000".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionSpend {
    /// USD a provider actually invoices.
    pub billed: Option<f64>,
    /// USD the same tokens would have cost at API rates, on a subscription that
    /// covers them. Never added to `billed`.
    pub subscription: Option<f64>,
}

/// One model's price and who pays it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelSpend {
    pub usd: Option<f64>,
    pub billing: Billing,
}

impl ModelSpend {
    /// What a row's turns recorded paying, `None` only when estimation is allowed.
    /// A row can hold both payers when one model served under an API key
    /// and a subscription, and the figure then folds them the way a provider
    /// row does: any charge makes it a bill, and a `$0` one does not.
    pub fn recorded(usage: &StoredTokenUsage) -> Option<Self> {
        let billing = match (usage.cost, usage.subscription_cost) {
            (None, None) if usage.no_cost_estimate => Billing::Api,
            (None, None) => return None,
            (Some(cost), _) if cost > 0.0 => Billing::Api,
            (_, Some(_)) => Billing::Subscription,
            (Some(_), None) => Billing::Api,
        };
        let mut usd = usage.cost;
        add_cost(&mut usd, usage.subscription_cost);
        Some(Self { usd, billing })
    }

    /// Where an estimate belongs in a [`StoredTokenUsage`]: it is priced under
    /// one payer, so only one column is filled.
    fn columns(self) -> (Option<f64>, Option<f64>) {
        match self.billing {
            Billing::Api => (self.usd, None),
            Billing::Subscription => (None, self.usd),
        }
    }
}

/// The bill a stored session ran up. Status bar, `/usage`, ACP and headless all
/// come here, so they cannot disagree about the same session.
///
/// Turns record what they paid, in one column per payer, so summing each
/// column is the truth and both are kept as they are. What was written before
/// that kept counters only, and its estimate against today's table is settled
/// into the entry here, once: after a later turn merges its own cost in, the
/// counters no longer say which of them was already paid for, so estimating
/// again would drop everything the entry had before. Counters with no breakdown
/// get one seeded, so there is always somewhere to settle into.
pub fn settle_session(
    total: &TokenUsage,
    by_model: &mut HashMap<String, StoredTokenUsage>,
    current: &Model,
    fast: bool,
) -> SessionSpend {
    qualify_keys(by_model, &current.provider);
    if by_model.is_empty() && *total != TokenUsage::default() {
        by_model.insert(current.spec(), total.billed(None, current.billing));
    }
    for (id, usage) in by_model.iter_mut() {
        if ModelSpend::recorded(usage).is_none() {
            (usage.cost, usage.subscription_cost) = model_cost(id, usage, current, fast).columns();
        }
    }
    SessionSpend {
        billed: sum(by_model.values().filter_map(|usage| usage.cost)),
        subscription: sum(by_model
            .values()
            .filter_map(|usage| usage.subscription_cost)),
    }
}

fn sum(costs: impl Iterator<Item = f64>) -> Option<f64> {
    costs.reduce(|total, cost| total + cost)
}

/// Puts the provider back in front of every key written before the breakdown
/// named one. A bare key meant "whoever served this session", and `provider` is
/// that, so the rewrite recovers exactly what the key already implied and
/// nothing more. Runs on load, so the ambiguity is resolved once rather than
/// guessed at by every reader.
fn qualify_keys(by_model: &mut HashMap<String, StoredTokenUsage>, provider: &str) {
    let bare: Vec<String> = by_model
        .keys()
        .filter(|id| !id.contains('/'))
        .cloned()
        .collect();
    for id in bare {
        let Some(usage) = by_model.remove(&id) else {
            continue;
        };
        *by_model.entry(format!("{provider}/{id}")).or_default() += usage;
    }
}

/// One model's slice of [`settle_session`], as `/usage` breaks it down per row.
/// `id` is a `provider/model` spec once [`qualify_keys`] has run; a bare one
/// from a caller that skipped settling still resolves, under the session's
/// provider.
///
/// A row that recorded a price reports that, as [`ModelSpend::recorded`]. A row
/// written before Caudra recorded its payer is re-priced under `current`'s
/// billing mode, the same borrowing that already lets today's rates stand in for
/// the rates a historical turn actually paid.
pub fn model_cost(id: &str, usage: &StoredTokenUsage, current: &Model, fast: bool) -> ModelSpend {
    if let Some(spend) = ModelSpend::recorded(usage) {
        return spend;
    }
    let billing = current.billing;
    if id == current.id || id == current.spec() {
        return ModelSpend {
            usd: current.list_cost(&(*usage).into(), fast),
            billing,
        };
    }
    let usd = Model::from_spec(id)
        .or_else(|_| Model::from_spec(&format!("{}/{}", current.provider, id)))
        .ok()
        .and_then(|model| model.list_cost(&(*usage).into(), fast));
    ModelSpend { usd, billing }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReasoningOptions;
    use crate::manifest::ManifestRegistry;
    use crate::model::{FastPricing, ModelFamily, PricingTier};
    use jiff::civil;
    use std::borrow::Cow;
    use std::sync::Arc;
    use test_case::test_case;

    const SECONDS_PER_MINUTE: i64 = 60;
    const PEAK: f64 = 2.0;
    const DAYS_SINCE_EPOCH: i64 = 20_000;
    const PEAK_WINDOWS: &[PricingWindow] =
        &[PricingWindow::hours(1, 4), PricingWindow::hours(6, 10)];
    const WRAPPING_WINDOW: &[PricingWindow] = &[PricingWindow::hours(22, 2)];
    const UNTIL_MIDNIGHT_WINDOW: &[PricingWindow] = &[PricingWindow::hours(22, HOURS_PER_DAY)];
    const WHOLE_DAY_WINDOW: &[PricingWindow] = &[PricingWindow::hours(0, HOURS_PER_DAY)];
    const LAST_MINUTE: i64 = 59;
    const LAST_SECOND: i64 = 59;

    const CURRENT: &str = "current";
    const INPUT_RATE: f64 = 3.0;
    const UNPRICED: f64 = 0.0;
    const ONE_MILLION: u32 = 1_000_000;
    /// [`ONE_MILLION`] input tokens at [`INPUT_RATE`].
    const LIST_PRICE: f64 = 3.0;
    const RECORDED: f64 = 0.5;
    const UNRESOLVABLE: &str = "a-model-no-table-has-ever-heard-of";
    const ALSO_UNRESOLVABLE: &str = "another-model-no-table-has-ever-heard-of";
    const FAST_INPUT_RATE: f64 = 12.0;
    /// [`ONE_MILLION`] input tokens at [`FAST_INPUT_RATE`].
    const FAST_LIST_PRICE: f64 = 12.0;
    /// A tier that [`ONE_MILLION`] prompt tokens would cross in one request.
    const LONG_PROMPT_TIER: &[PricingTier] = &[PricingTier {
        above: ONE_MILLION - 1,
        input: 2.0 * INPUT_RATE,
        output: 0.0,
        cache_write: 0.0,
        cache_read: 0.0,
        fast: None,
    }];
    /// A real spec, so the bare-id fallback runs against the real tables.
    const SCHEDULED_SPEC: &str = "deepseek/deepseek-v4-pro";
    /// A provider whose model ids carry a slash of their own.
    const NESTED_SPEC: &str = "openrouter/vendor/model";
    const LEGACY_KEYS_QUALIFY: &str =
        "a breakdown must settle into one row per provider and model, whenever it was written";
    const SUBSCRIPTION: f64 = 0.25;
    const FREE: f64 = 0.0;
    const PAYERS_KEPT: &str = "settling must keep what each payer recorded, however often it runs";

    fn utc(day: i64, hour: i64, minute: i64, second: i64) -> Timestamp {
        Timestamp::from_second(
            day * SECONDS_PER_DAY + hour * SECONDS_PER_HOUR + minute * SECONDS_PER_MINUTE + second,
        )
        .expect("timestamp in range")
    }

    fn model(id: &str, input_rate: f64) -> Model {
        Model {
            id: id.into(),
            provider: Arc::from("anthropic"),
            family: ModelFamily::Claude,
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: None,
            supports_cache_breakpoints_override: None,
            supports_pdf: false,
            pricing: ModelPricing {
                input: input_rate,
                ..ModelPricing::ZERO
            },
            dated_rates: None,
            discovered_free: false,
            max_output_tokens: None,
            context_window: 0,
            window_excludes_output: false,
            reasoning_options: ReasoningOptions::default(),
            thinking_fields: None,
            billing: Billing::default(),
        }
    }

    fn stored(cost: Option<f64>) -> StoredTokenUsage {
        StoredTokenUsage {
            input: ONE_MILLION,
            cost,
            ..Default::default()
        }
    }

    fn breakdown(entries: &[(&str, Option<f64>)]) -> HashMap<String, StoredTokenUsage> {
        entries
            .iter()
            .map(|(id, cost)| ((*id).to_owned(), stored(*cost)))
            .collect()
    }

    /// Windows are half-open down to the second, since an off-by-one bills a
    /// whole hour at the wrong rate. Every case also runs on a day before the
    /// epoch, where the timestamp is negative and must not wrap into another
    /// window.
    #[test_case(PEAK_WINDOWS, 0, LAST_MINUTE, LAST_SECOND, FLAT_RATE ; "last_second_before_the_start")]
    #[test_case(PEAK_WINDOWS, 1, 0, 0, PEAK                          ; "first_second_of_a_window")]
    #[test_case(PEAK_WINDOWS, 3, LAST_MINUTE, LAST_SECOND, PEAK      ; "last_second_inside")]
    #[test_case(PEAK_WINDOWS, 4, 0, 0, FLAT_RATE                     ; "first_second_after_the_end")]
    #[test_case(PEAK_WINDOWS, 7, 15, 0, PEAK                         ; "second_window")]
    #[test_case(PEAK_WINDOWS, 23, 0, 0, FLAT_RATE                    ; "after_the_last_window")]
    #[test_case(WRAPPING_WINDOW, 23, 0, 0, PEAK                      ; "wrapping_before_midnight")]
    #[test_case(WRAPPING_WINDOW, 0, 0, 0, PEAK                       ; "wrapping_across_midnight")]
    #[test_case(WRAPPING_WINDOW, 21, LAST_MINUTE, LAST_SECOND, FLAT_RATE ; "wrapping_start_is_half_open")]
    #[test_case(WRAPPING_WINDOW, 2, 0, 0, FLAT_RATE                  ; "wrapping_end_is_half_open")]
    #[test_case(UNTIL_MIDNIGHT_WINDOW, 23, LAST_MINUTE, LAST_SECOND, PEAK ; "ends_at_midnight")]
    #[test_case(UNTIL_MIDNIGHT_WINDOW, 0, 0, 0, FLAT_RATE            ; "ending_at_midnight_does_not_wrap")]
    #[test_case(WHOLE_DAY_WINDOW, 0, 0, 0, PEAK                      ; "whole_day_starts_at_midnight")]
    #[test_case(WHOLE_DAY_WINDOW, 23, LAST_MINUTE, LAST_SECOND, PEAK ; "whole_day_never_leaves_peak")]
    fn windows_price_by_the_utc_clock(
        windows: &'static [PricingWindow],
        hour: i64,
        minute: i64,
        second: i64,
        expected: f64,
    ) {
        let schedule = PricingSchedule::new(windows, PEAK);
        for day in [DAYS_SINCE_EPOCH, -DAYS_SINCE_EPOCH] {
            assert_eq!(
                schedule.multiplier_at(utc(day, hour, minute, second)),
                expected,
                "day {day}"
            );
        }
    }

    const CHANGED_INPUT_RATE: f64 = 6.0;
    /// [`ONE_MILLION`] input tokens at [`CHANGED_INPUT_RATE`].
    const CHANGED_LIST_PRICE: f64 = 6.0;
    const LAST_SECOND_BEFORE: &str = "2026-11-30T23:59:59Z";
    const FIRST_SECOND_OF: &str = "2026-12-01T00:00:00Z";
    /// 04:30 UTC on the first, still the evening before in New York.
    const EVENING_BEFORE_IN_NEW_YORK: &str = "2026-11-30T23:30:00-05:00";
    const LAST_SECOND_OF_DECEMBER: &str = "2026-12-31T23:59:59Z";
    const FIRST_SECOND_OF_JANUARY: &str = "2027-01-01T00:00:00Z";
    const LONG_AFTER: &str = "2027-06-01T12:00:00Z";
    const DECEMBER: Date = civil::date(2026, 12, 1);
    const JANUARY: Date = civil::date(2027, 1, 1);
    const FROM_DECEMBER: Period = Period::from(DECEMBER);
    const UNTIL_DECEMBER: Period = Period::until(DECEMBER);
    const DECEMBER_ONLY: Period = Period::between(DECEMBER, JANUARY);
    const ALWAYS: Period = Period {
        from: None,
        until: None,
    };
    const CHANGED_RATES: ModelPricing = ModelPricing {
        input: CHANGED_INPUT_RATE,
        ..ModelPricing::ZERO
    };
    static TABLE_RATES: ModelPricing = ModelPricing {
        input: INPUT_RATE,
        ..ModelPricing::ZERO
    };
    static RAISED_FROM_DECEMBER: PricePeriod = PricePeriod {
        prefix: CURRENT,
        during: FROM_DECEMBER,
        pricing: CHANGED_RATES,
    };
    static PROMO_UNTIL_DECEMBER: PricePeriod = PricePeriod {
        prefix: CURRENT,
        during: UNTIL_DECEMBER,
        pricing: CHANGED_RATES,
    };
    static DECEMBER_RATE: PricePeriod = PricePeriod {
        prefix: CURRENT,
        during: DECEMBER_ONLY,
        pricing: CHANGED_RATES,
    };
    const HISTORY_IGNORES_THE_CLOCK: &str = "re-pricing history must not depend on the clock";

    fn instant(text: &str) -> Timestamp {
        text.parse().expect("an RFC 3339 instant")
    }

    #[test_case(FROM_DECEMBER, LAST_SECOND_BEFORE, false         ; "from_excludes_the_day_before")]
    #[test_case(FROM_DECEMBER, FIRST_SECOND_OF, true             ; "from_includes_its_day")]
    #[test_case(FROM_DECEMBER, EVENING_BEFORE_IN_NEW_YORK, true  ; "the_day_is_read_in_utc")]
    #[test_case(FROM_DECEMBER, LONG_AFTER, true                  ; "from_never_ends")]
    #[test_case(UNTIL_DECEMBER, LAST_SECOND_BEFORE, true         ; "until_includes_the_day_before")]
    #[test_case(UNTIL_DECEMBER, FIRST_SECOND_OF, false           ; "until_excludes_its_day")]
    #[test_case(DECEMBER_ONLY, LAST_SECOND_BEFORE, false         ; "between_starts_on_from")]
    #[test_case(DECEMBER_ONLY, FIRST_SECOND_OF, true             ; "between_includes_from")]
    #[test_case(DECEMBER_ONLY, LAST_SECOND_OF_DECEMBER, true     ; "between_includes_the_day_before_until")]
    #[test_case(DECEMBER_ONLY, FIRST_SECOND_OF_JANUARY, false    ; "between_excludes_until")]
    #[test_case(ALWAYS, LONG_AFTER, true                         ; "open_on_both_ends")]
    fn a_period_covers_whole_utc_days(period: Period, at: &str, expected: bool) {
        assert_eq!(period.contains(instant(at)), expected);
    }

    #[test_case(FROM_DECEMBER, LAST_SECOND_BEFORE, true        ; "from_still_ahead")]
    #[test_case(FROM_DECEMBER, FIRST_SECOND_OF, false          ; "from_reached")]
    #[test_case(DECEMBER_ONLY, FIRST_SECOND_OF, true           ; "until_still_ahead")]
    #[test_case(DECEMBER_ONLY, FIRST_SECOND_OF_JANUARY, false  ; "both_edges_passed")]
    #[test_case(ALWAYS, LAST_SECOND_BEFORE, false              ; "no_edges")]
    fn a_period_moves_while_an_edge_is_ahead(period: Period, at: &str, expected: bool) {
        assert_eq!(period.moves_after(instant(at)), expected);
    }

    /// A model built before an edge keeps the rates it was built with, so the
    /// switch, and the switch back, happen when a turn is billed.
    #[test_case(&RAISED_FROM_DECEMBER, LAST_SECOND_BEFORE, LIST_PRICE         ; "from_bills_the_table_before")]
    #[test_case(&RAISED_FROM_DECEMBER, FIRST_SECOND_OF, CHANGED_LIST_PRICE    ; "from_bills_the_period_on_its_day")]
    #[test_case(&PROMO_UNTIL_DECEMBER, LAST_SECOND_BEFORE, CHANGED_LIST_PRICE ; "until_bills_the_period_before")]
    #[test_case(&PROMO_UNTIL_DECEMBER, FIRST_SECOND_OF, LIST_PRICE            ; "until_bills_the_table_on_its_day")]
    #[test_case(&DECEMBER_RATE, LAST_SECOND_BEFORE, LIST_PRICE                ; "between_bills_the_table_before")]
    #[test_case(&DECEMBER_RATE, LAST_SECOND_OF_DECEMBER, CHANGED_LIST_PRICE   ; "between_bills_the_period_inside")]
    #[test_case(&DECEMBER_RATE, FIRST_SECOND_OF_JANUARY, LIST_PRICE           ; "between_switches_back")]
    fn a_turn_bills_the_rates_in_force_when_it_ends(
        period: &'static PricePeriod,
        at: &str,
        expected: f64,
    ) {
        let mut current = model(CURRENT, INPUT_RATE);
        current.dated_rates = Some(DatedRates {
            period,
            otherwise: &TABLE_RATES,
        });
        let usage = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };

        assert_eq!(
            current.billed_cost_at(&usage, false, instant(at)),
            Some(expected)
        );
        assert_eq!(
            current.list_cost(&usage, false),
            Some(LIST_PRICE),
            "{HISTORY_IGNORES_THE_CLOCK}"
        );
    }

    #[test]
    fn schedules_render_the_hours_they_bill() {
        assert_eq!(
            PricingSchedule::new(PEAK_WINDOWS, PEAK).to_string(),
            "2x during 01:00-04:00, 06:00-10:00 UTC"
        );
        assert_eq!(
            PricingSchedule::new(WRAPPING_WINDOW, PEAK).to_string(),
            "2x during 22:00-02:00 UTC"
        );
    }

    /// Each entry bills [`ONE_MILLION`] input tokens, so a row worth
    /// [`LIST_PRICE`] came from the price table and [`RECORDED`] came from the
    /// turn itself. A breakdown nothing can price is not a free session:
    /// `Some(0.0)` would put a confident "$0.000" on screen.
    #[test_case(&[(UNRESOLVABLE, Some(RECORDED)), (ALSO_UNRESOLVABLE, Some(RECORDED))], INPUT_RATE, Some(2.0 * RECORDED) ; "recorded_costs_win_over_the_price_table")]
    #[test_case(&[(CURRENT, None), (UNRESOLVABLE, None)], INPUT_RATE, Some(LIST_PRICE)                                   ; "legacy_counters_use_each_models_own_rates")]
    #[test_case(&[(UNRESOLVABLE, Some(RECORDED)), (CURRENT, None), (ALSO_UNRESOLVABLE, None)], INPUT_RATE, Some(RECORDED + LIST_PRICE) ; "mixed_eras_sum_recorded_and_listed")]
    #[test_case(&[(UNRESOLVABLE, None)], INPUT_RATE, None                                                               ; "a_breakdown_that_prices_to_nothing")]
    #[test_case(&[(CURRENT, None)], UNPRICED, None                                                                      ; "unpriced_model_with_a_breakdown")]
    #[test_case(&[], INPUT_RATE, Some(LIST_PRICE)                                                                       ; "no_breakdown_prices_the_total")]
    #[test_case(&[], UNPRICED, None                                                                                     ; "no_breakdown_on_an_unpriced_model")]
    fn session_cost_bills_every_breakdown(
        entries: &[(&str, Option<f64>)],
        input_rate: f64,
        expected: Option<f64>,
    ) {
        let current = model(CURRENT, input_rate);
        let mut by_model = breakdown(entries);
        let counted = entries.len().max(1) as u32 * ONE_MILLION;

        // With a breakdown the stored running total says nothing about the
        // bill, so the two drifting apart (compaction, a resume) must not
        // change the answer.
        let totals = if entries.is_empty() {
            vec![counted]
        } else {
            vec![counted, 0, 999 * ONE_MILLION]
        };
        for input in totals {
            let total = TokenUsage {
                input,
                ..Default::default()
            };
            assert_eq!(
                settle_session(&total, &mut by_model, &current, false).billed,
                expected,
                "total of {input} input tokens"
            );
        }
    }

    /// An entry written before turns recorded their cost used to keep only the
    /// cost of the next turn that touched it, so every later load reported the
    /// session as costing whatever it last did.
    #[test_case(&[]                ; "counters_with_no_breakdown")]
    #[test_case(&[(CURRENT, None)] ; "a_breakdown_written_before_costs")]
    fn a_settled_estimate_survives_the_next_turn(entries: &[(&str, Option<f64>)]) {
        let current = model(CURRENT, INPUT_RATE);
        let mut by_model = breakdown(entries);
        let total = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };

        assert_eq!(
            settle_session(&total, &mut by_model, &current, false).billed,
            Some(LIST_PRICE)
        );

        *by_model.entry(CURRENT.to_owned()).or_default() += stored(Some(RECORDED));

        assert_eq!(
            settle_session(&total, &mut by_model, &current, false).billed,
            Some(LIST_PRICE + RECORDED)
        );
    }

    /// One model can serve under an API key and a subscription both, so a row
    /// can carry both columns. Settling used to keep only the API one, even at
    /// `$0`, and a reopened session lost its subscription spend. Both rows are
    /// already qualified, and one of them no table can price, so neither can be
    /// re-priced or renamed.
    #[test_case(None, Some(SUBSCRIPTION)        ; "subscription_only")]
    #[test_case(Some(RECORDED), Some(SUBSCRIPTION) ; "both_payers")]
    #[test_case(Some(FREE), Some(SUBSCRIPTION)  ; "free_api_beside_a_subscription")]
    fn settling_keeps_both_recorded_columns(cost: Option<f64>, subscription_cost: Option<f64>) {
        let current = model(CURRENT, INPUT_RATE);
        let row = StoredTokenUsage {
            cost,
            subscription_cost,
            ..stored(None)
        };
        let mut by_model = HashMap::from([
            (current.spec(), row),
            (format!("{}/{UNRESOLVABLE}", current.provider), row),
        ]);
        let recorded = by_model.clone();
        let mut settle = || settle_session(&TokenUsage::default(), &mut by_model, &current, false);

        let first = settle();
        let second = settle();

        let expected = SessionSpend {
            billed: cost.map(|cost| 2.0 * cost),
            subscription: subscription_cost.map(|cost| 2.0 * cost),
        };
        assert_eq!(first, expected, "{PAYERS_KEPT}");
        assert_eq!(second, expected, "{PAYERS_KEPT}");
        assert_eq!(by_model, recorded, "{PAYERS_KEPT}");
    }

    #[test_case(Some(RECORDED), None         => Some((Some(RECORDED), Billing::Api))                    ; "billed")]
    #[test_case(None, Some(SUBSCRIPTION)     => Some((Some(SUBSCRIPTION), Billing::Subscription))       ; "covered")]
    #[test_case(Some(RECORDED), Some(SUBSCRIPTION) => Some((Some(RECORDED + SUBSCRIPTION), Billing::Api)) ; "mixed_is_a_bill")]
    #[test_case(Some(FREE), Some(SUBSCRIPTION) => Some((Some(SUBSCRIPTION), Billing::Subscription))     ; "free_api_charges_nothing")]
    #[test_case(Some(FREE), None             => Some((Some(FREE), Billing::Api))                        ; "free_api_alone")]
    #[test_case(None, None                   => None                                                    ; "unpriced_stays_unpriced")]
    fn recorded_spend_adds_both_payers(
        cost: Option<f64>,
        subscription_cost: Option<f64>,
    ) -> Option<(Option<f64>, Billing)> {
        ModelSpend::recorded(&StoredTokenUsage {
            cost,
            subscription_cost,
            ..StoredTokenUsage::default()
        })
        .map(|spend| (spend.usd, spend.billing))
    }

    #[test_case(Billing::Api, false          ; "current_api_model")]
    #[test_case(Billing::Subscription, false ; "current_subscription_model")]
    #[test_case(Billing::Api, true           ; "catalog_model_under_api")]
    #[test_case(Billing::Subscription, true  ; "catalog_model_under_subscription")]
    fn unpriced_decisions_never_borrow_chat_prices(billing: Billing, catalog: bool) {
        let mut current = model(CURRENT, INPUT_RATE);
        current.billing = billing;
        let id = if catalog {
            SCHEDULED_SPEC.to_owned()
        } else {
            current.spec()
        };
        let legacy = stored(None);
        assert!(model_cost(&id, &legacy, &current, false).usd.is_some());
        let usage = StoredTokenUsage {
            no_cost_estimate: true,
            ..legacy
        };
        let expected = ModelSpend {
            usd: None,
            billing: Billing::Api,
        };
        assert_eq!(ModelSpend::recorded(&usage), Some(expected));
        assert_eq!(model_cost(&id, &usage, &current, false), expected);
        let mut by_model = HashMap::from([(id.clone(), usage)]);
        for _ in 0..2 {
            assert_eq!(
                settle_session(&usage.into(), &mut by_model, &current, false),
                SessionSpend::default()
            );
            assert_eq!(by_model[&id], usage);
        }
    }

    #[test_case(Billing::Api          ; "legacy_api")]
    #[test_case(Billing::Subscription ; "legacy_subscription")]
    fn legacy_usage_still_estimates_chat_prices(billing: Billing) {
        let mut current = model(CURRENT, INPUT_RATE);
        current.billing = billing;
        let usage = stored(None);
        assert!(!usage.no_cost_estimate);
        assert_eq!(ModelSpend::recorded(&usage), None);
        let expected = ModelSpend {
            usd: Some(LIST_PRICE),
            billing,
        };
        assert_eq!(
            model_cost(&current.spec(), &usage, &current, false),
            expected
        );
        let mut by_model = HashMap::from([(current.spec(), usage)]);
        let (billed, subscription) = expected.columns();
        assert_eq!(
            settle_session(&usage.into(), &mut by_model, &current, false),
            SessionSpend {
                billed,
                subscription
            }
        );
    }

    #[test_case(Some(RECORDED), None, false ; "billed_chat_then_decision")]
    #[test_case(None, Some(SUBSCRIPTION), false ; "covered_chat_then_decision")]
    #[test_case(Some(RECORDED), Some(SUBSCRIPTION), false ; "both_payers_then_decision")]
    #[test_case(Some(RECORDED), Some(SUBSCRIPTION), true ; "decision_then_both_payers")]
    fn merging_unpriced_decisions_preserves_known_chat_costs(
        cost: Option<f64>,
        subscription_cost: Option<f64>,
        decision_first: bool,
    ) {
        let current = model(CURRENT, INPUT_RATE);
        let chat = StoredTokenUsage {
            cost,
            subscription_cost,
            ..stored(None)
        };
        let decision = StoredTokenUsage {
            no_cost_estimate: true,
            ..stored(None)
        };
        let mut merged = if decision_first { decision } else { chat };
        merged += if decision_first { chat } else { decision };
        assert!(merged.no_cost_estimate);
        assert_eq!(ModelSpend::recorded(&merged), ModelSpend::recorded(&chat));
        assert_eq!(
            model_cost(&current.spec(), &merged, &current, false),
            model_cost(&current.spec(), &chat, &current, false)
        );
        let mut by_model = HashMap::from([(current.spec(), merged)]);
        assert_eq!(
            settle_session(&merged.into(), &mut by_model, &current, false),
            SessionSpend {
                billed: cost,
                subscription: subscription_cost,
            }
        );
        assert_eq!(by_model[&current.spec()], merged);
    }

    /// Rows written before the breakdown named a provider are bare. Loading one
    /// beside its qualified twin has to settle into a single row, not into two
    /// halves of the same session.
    #[test]
    fn a_legacy_bare_key_merges_into_its_qualified_row() {
        let current = model(CURRENT, INPUT_RATE);
        let qualified = current.spec();
        let mut by_model = HashMap::from([
            (CURRENT.to_owned(), stored(Some(RECORDED))),
            (qualified.clone(), stored(Some(RECORDED))),
        ]);

        let spend = settle_session(&TokenUsage::default(), &mut by_model, &current, false);

        assert_eq!(
            by_model.keys().collect::<Vec<_>>(),
            [&qualified],
            "{LEGACY_KEYS_QUALIFY}"
        );
        assert_eq!(spend.billed, Some(2.0 * RECORDED), "{LEGACY_KEYS_QUALIFY}");
    }

    /// A provider slug is everything before the first slash, so a model id that
    /// contains one is already qualified and must not collect a second prefix.
    #[test]
    fn a_model_id_that_contains_a_slash_is_already_qualified() {
        let current = model(CURRENT, INPUT_RATE);
        let mut by_model = HashMap::from([(NESTED_SPEC.to_owned(), stored(Some(RECORDED)))]);

        settle_session(&TokenUsage::default(), &mut by_model, &current, false);

        assert_eq!(
            by_model.keys().collect::<Vec<_>>(),
            [NESTED_SPEC],
            "{LEGACY_KEYS_QUALIFY}"
        );
    }

    /// The sibling is priced differently on purpose, so passing here cannot be
    /// `current`'s rates leaking through.
    #[test]
    fn bare_ids_resolve_against_the_current_provider() {
        let current = Model::from_spec(SCHEDULED_SPEC).unwrap();
        let sibling_id = ManifestRegistry::for_slug(&current.provider)
            .expect("a builtin provider")
            .models
            .iter()
            .find_map(|e| (e.pricing.input != current.pricing.input).then_some(e.prefixes[0]))
            .expect("a sibling model the table prices differently");
        let sibling = Model::from_spec(&format!("{}/{sibling_id}", current.provider)).unwrap();
        let usage = stored(None);

        let cost = model_cost(sibling_id, &usage, &current, false).usd;
        assert_eq!(cost, sibling.list_cost(&usage.into(), false));
        assert_ne!(cost, current.list_cost(&usage.into(), false));
    }

    const NOT_A_BILL: &str = "a plan's price must never reach the billed column";

    /// Subscription turns used to report nothing at all. They price like any
    /// other turn now; only the column they land in differs.
    #[test]
    fn a_subscription_prices_its_tokens_without_billing_them() {
        let mut current = model(CURRENT, INPUT_RATE);
        current.billing = Billing::Subscription;
        let total = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };

        let spend = settle_session(&total, &mut breakdown(&[]), &current, false);

        assert_eq!(spend.subscription, Some(LIST_PRICE));
        assert_eq!(spend.billed, None, "{NOT_A_BILL}");
    }

    #[test_case(Billing::Api          ; "an_api_key_is_invoiced")]
    #[test_case(Billing::Subscription ; "a_plan_owes_nothing")]
    fn a_row_reports_the_payer_it_was_priced_under(billing: Billing) {
        let mut current = model(CURRENT, INPUT_RATE);
        current.billing = billing;
        let spend = model_cost(CURRENT, &stored(None), &current, false);
        assert_eq!(spend.usd, Some(LIST_PRICE));
        assert_eq!(spend.billing, billing);
    }

    /// What was paid is what was paid, even for an id the table prices
    /// differently, and even for a model that prices to nothing today.
    #[test_case(INPUT_RATE ; "priced_model")]
    #[test_case(UNPRICED   ; "unpriced_model")]
    fn a_recorded_cost_short_circuits_the_price_table(input_rate: f64) {
        let current = model(CURRENT, input_rate);
        assert_eq!(
            model_cost(CURRENT, &stored(Some(RECORDED)), &current, false).usd,
            Some(RECORDED)
        );
    }

    /// A stored row sums many requests, so its total is no one prompt's size.
    /// Tiering the sum would bill every turn of a long session at the rate
    /// only its longest prompts paid.
    #[test]
    fn summed_counters_price_below_every_tier() {
        let mut current = model(CURRENT, INPUT_RATE);
        current.pricing.tiers = Cow::Borrowed(LONG_PROMPT_TIER);
        assert_eq!(
            model_cost(CURRENT, &stored(None), &current, false).usd,
            Some(LIST_PRICE)
        );
    }

    #[test_case(true, &[]                => Some(FAST_LIST_PRICE) ; "fast_seeded_from_the_counters")]
    #[test_case(true, &[(CURRENT, None)] => Some(FAST_LIST_PRICE) ; "fast_from_a_breakdown")]
    #[test_case(false, &[(CURRENT, None)] => Some(LIST_PRICE)     ; "standard_from_a_breakdown")]
    fn fast_rates_reach_the_session_total(
        fast: bool,
        entries: &[(&str, Option<f64>)],
    ) -> Option<f64> {
        let mut current = model(CURRENT, INPUT_RATE);
        current.pricing.fast = Some(FastPricing::derived(FAST_INPUT_RATE, 0.0));
        let total = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };

        settle_session(&total, &mut breakdown(entries), &current, fast).billed
    }
}
