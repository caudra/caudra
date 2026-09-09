//! Bridges `tracing` events to the OTLP exporter, so one call site feeds both
//! the log file and telemetry.
//!
//! The layer forwards an allow-list of targets ([`EVENT_NAMES`]) and nothing
//! else. A level filter would be the wrong gate: it would forward every future
//! `info!` in the workspace, including ones carrying user content.

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::attr::{AttrSet, AttrValue};
use crate::logs::{Severity, event_name};
use crate::{handle, redact_for_export};

const KEY_LEVEL: &str = "level";
const SPAN_NAME_SUFFIX: &str = ".name";

/// Installed unconditionally at startup. It stays inert until
/// [`crate::init`] enables telemetry, which removes any ordering constraint
/// between subscriber setup and telemetry setup.
pub struct OtelLayer;

pub fn layer() -> OtelLayer {
    OtelLayer
}

impl<S> Layer<S> for OtelLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(name) = event_name(event.metadata().target()) else {
            return;
        };
        let Some(handle) = handle() else {
            return;
        };

        let mut attrs = AttrSet::new();
        // Span fields first so an event field of the same name wins.
        for span in ctx.event_scope(event).into_iter().flatten() {
            if let Some(fields) = span.extensions().get::<AttrSet>() {
                attrs.extend_from(fields);
            }
        }
        let mut visitor = AttrVisitor { attrs: &mut attrs };
        event.record(&mut visitor);
        redact_for_export(handle, &mut attrs);

        handle.event(name, Severity::of(*event.metadata().level()), attrs);
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut fields = AttrSet::new();
        attrs.record(&mut AttrVisitor { attrs: &mut fields });
        if !fields.is_empty() {
            span.extensions_mut().insert(fields);
        }
    }
}

struct AttrVisitor<'a> {
    attrs: &'a mut AttrSet,
}

impl AttrVisitor<'_> {
    /// `level` and the span-name suffix are already carried by the record, so
    /// letting a field overwrite them would corrupt the envelope.
    fn insert(&mut self, field: &Field, value: impl Into<AttrValue>) {
        let name = field.name();
        if name == KEY_LEVEL || name.ends_with(SPAN_NAME_SUFFIX) {
            return;
        }
        self.attrs.insert(name, value);
    }
}

impl Visit for AttrVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, value);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, value);
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.insert(field, i64::try_from(value).unwrap_or(i64::MAX));
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.insert(field, i64::try_from(value).unwrap_or(i64::MAX));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, value);
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert(field, format!("{value:?}"));
    }
}

/// Events the layer forwards are always recorded, whatever `RUST_LOG` says
/// about the file sink, because telemetry has its own opt-in.
pub fn telemetry_targets() -> tracing_subscriber::filter::Targets {
    crate::logs::EVENT_NAMES
        .iter()
        .fold(tracing_subscriber::filter::Targets::new(), |t, name| {
            t.with_target(*name, Level::TRACE)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::{EVENT_API_REQUEST, EVENT_NAMES};

    const UNRELATED_TARGET: &str = "caudra_agent::agent::run";

    #[test]
    fn only_the_declared_event_names_resolve() {
        assert_eq!(event_name(EVENT_API_REQUEST), Some(EVENT_API_REQUEST));
        assert_eq!(event_name(UNRELATED_TARGET), None);
    }

    #[test]
    fn every_event_name_is_forwarded_at_every_level() {
        let targets = telemetry_targets();
        for name in EVENT_NAMES {
            assert!(
                targets.would_enable(name, &Level::TRACE),
                "{name} must reach the exporter"
            );
        }
        assert!(!targets.would_enable(UNRELATED_TARGET, &Level::ERROR));
    }

    #[test]
    fn severity_tracks_the_event_level() {
        assert_eq!(Severity::of(Level::ERROR).text, "ERROR");
        assert_eq!(Severity::of(Level::INFO), Severity::INFO);
    }
}
