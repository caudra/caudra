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

/// A price the provider has announced for a later date, such as the end of an
/// introductory rate. The model table quotes today's price. From `from` at
/// 00:00 UTC every turn bills at `pricing`, tiers included, so a build shipped
/// during a promotion stays right after it ends. Providers name a day without
/// a timezone, so a turn near midnight may land a day off.
#[derive(Debug)]
pub struct PriceChange {
    /// The `ModelEntry` prefix whose rates change.
    pub prefix: &'static str,
    pub from: Date,
    pub pricing: ModelPricing,
}

impl PriceChange {
    pub(crate) fn applies_at(&self, at: Timestamp) -> bool {
        at.to_zoned(TimeZone::UTC).date() >= self.from
    }
}

/// The rates in force at `at`: `base` until `change` applies, then the
/// change's. Also returns the change while it is still ahead, so whoever
/// holds the rates can switch over later.
pub(crate) fn rates_in_force<'a>(
    base: &'a ModelPricing,
    change: Option<&'static PriceChange>,
    at: Timestamp,
) -> (&'a ModelPricing, Option<&'static PriceChange>) {
    match change {
        Some(change) if change.applies_at(at) => (&change.pricing, None),
        pending => (base, pending),
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
    /// What a row's turns recorded paying, `None` while nothing on it was
    /// priced. A row can hold both payers when one model served under an API key
    /// and a subscription, and the figure then folds them the way a provider
    /// row does: any charge makes it a bill, and a `$0` one does not.
    pub fn recorded(usage: &StoredTokenUsage) -> Option<Self> {
        let billing = match (usage.cost, usage.subscription_cost) {
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
            price_change: None,
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
    const LONG_AFTER: &str = "2027-06-01T12:00:00Z";
    static CHANGE: PriceChange = PriceChange {
        prefix: CURRENT,
        from: civil::date(2026, 12, 1),
        pricing: ModelPricing {
            input: CHANGED_INPUT_RATE,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
            fast: None,
            tiers: ModelPricing::UNTIERED,
        },
    };

    fn instant(text: &str) -> Timestamp {
        text.parse().expect("an RFC 3339 instant")
    }

    #[test_case(LAST_SECOND_BEFORE, false        ; "last_second_before_the_day")]
    #[test_case(FIRST_SECOND_OF, true            ; "first_second_of_the_day")]
    #[test_case(EVENING_BEFORE_IN_NEW_YORK, true ; "the_day_is_read_in_utc")]
    #[test_case(LONG_AFTER, true                 ; "long_after")]
    fn a_change_applies_from_midnight_utc(at: &str, expected: bool) {
        assert_eq!(CHANGE.applies_at(instant(at)), expected);
    }

    /// A model built before the date keeps the old rates on it, so the switch
    /// has to happen when a turn is billed.
    #[test_case(LAST_SECOND_BEFORE, Some(LIST_PRICE)      ; "before_bills_the_table")]
    #[test_case(FIRST_SECOND_OF, Some(CHANGED_LIST_PRICE) ; "from_the_day_bills_the_change")]
    fn a_turn_bills_the_rates_in_force_when_it_ends(at: &str, expected: Option<f64>) {
        let mut current = model(CURRENT, INPUT_RATE);
        current.price_change = Some(&CHANGE);
        let usage = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };

        assert_eq!(current.billed_cost_at(&usage, false, instant(at)), expected);
        assert_eq!(
            current.list_cost(&usage, false),
            Some(LIST_PRICE),
            "re-pricing history must not depend on the clock"
        );
    }

    #[test]
    fn rates_in_force_hand_over_once() {
        let base = ModelPricing {
            input: INPUT_RATE,
            ..ModelPricing::ZERO
        };

        let (before, pending) = rates_in_force(&base, Some(&CHANGE), instant(LAST_SECOND_BEFORE));
        assert_eq!(before.input, INPUT_RATE);
        assert!(pending.is_some());

        let (after, pending) = rates_in_force(&base, Some(&CHANGE), instant(FIRST_SECOND_OF));
        assert_eq!(after.input, CHANGED_INPUT_RATE);
        assert!(pending.is_none());
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
