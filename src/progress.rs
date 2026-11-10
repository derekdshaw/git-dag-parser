//! Draws the progress bar of pass 2 from the parser's progress events

use std::sync::Mutex;

use git_dag_parser_core::parser::PROGRESS_TARGET;
use indicatif::{ProgressBar, ProgressStyle};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// A layer that turns the events of `PROGRESS_TARGET` into a progress bar
#[derive(Default)]
pub struct ProgressLayer {
    bar: Mutex<Option<ProgressBar>>,
}

/// The fields of one progress event
#[derive(Default)]
struct Fields {
    message: String,
    total: Option<u64>,
    done: Option<u64>,
    finish: bool,
}

impl Visit for Fields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "total" => self.total = Some(value),
            "done" => self.done = Some(value),
            _ => {}
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "finish" {
            self.finish = value;
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

impl<S: Subscriber> Layer<S> for ProgressLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != PROGRESS_TARGET {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let mut bar = self.bar.lock().expect("no progress event panicked");
        if let Some(total) = fields.total {
            let new = ProgressBar::new(total).with_style(
                ProgressStyle::default_bar()
                    .template("  {prefix} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) [{elapsed_precise}] {msg}")
                    .expect("valid template")
                    .progress_chars("#>-"),
            );
            new.set_prefix(fields.message);
            *bar = Some(new);
        }
        if let (Some(done), Some(b)) = (fields.done, bar.as_ref()) {
            b.inc(done);
        }
        if fields.finish {
            if let Some(b) = bar.take() {
                b.finish_and_clear();
            }
        }
    }
}
