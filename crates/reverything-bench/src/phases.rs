//! Collects how long each `tracing` span took, summed per span name, so a benchmark can show
//! where a search spent its time without a trace viewer.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::span::{Attributes, Id};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

static TOTALS: Mutex<BTreeMap<&'static str, Duration>> = Mutex::new(BTreeMap::new());

struct Started(Instant);

struct PhaseLayer;

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for PhaseLayer {
    fn on_new_span(&self, _: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(Started(Instant::now()));
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let elapsed = span.extensions().get::<Started>().map(|s| s.0.elapsed());
        if let Some(elapsed) = elapsed {
            *TOTALS.lock().unwrap().entry(span.name()).or_default() += elapsed;
        }
    }
}

pub fn install() {
    tracing::subscriber::set_global_default(Registry::default().with(PhaseLayer))
        .expect("A tracing subscriber is already installed");
}

/// The time per span name since the last call.
pub fn take() -> BTreeMap<&'static str, Duration> {
    std::mem::take(&mut *TOTALS.lock().unwrap())
}
