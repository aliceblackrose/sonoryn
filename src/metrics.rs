use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// Process-local reliability and latency counters.
///
/// These deliberately use atomics rather than an async lock so recording a
/// metric never stalls the Gateway, command, resolver, or voice tasks.
#[derive(Debug, Default)]
pub struct Metrics {
    commands: AtomicU64,
    command_latency_micros: AtomicU64,
    command_latency_max_micros: AtomicU64,
    resolves: AtomicU64,
    resolve_latency_micros: AtomicU64,
    resolve_latency_max_micros: AtomicU64,
    startups: AtomicU64,
    startup_latency_micros: AtomicU64,
    startup_latency_max_micros: AtomicU64,
    underruns: AtomicU64,
    skips: AtomicU64,
    failures: AtomicU64,
    reconnects: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub commands: u64,
    pub command_latency_micros: u64,
    pub command_latency_max_micros: u64,
    pub resolves: u64,
    pub resolve_latency_micros: u64,
    pub resolve_latency_max_micros: u64,
    pub startups: u64,
    pub startup_latency_micros: u64,
    pub startup_latency_max_micros: u64,
    pub underruns: u64,
    pub skips: u64,
    pub failures: u64,
    pub reconnects: u64,
}

impl Metrics {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_command_latency(&self, elapsed: Duration) {
        record_latency(
            &self.commands,
            &self.command_latency_micros,
            &self.command_latency_max_micros,
            elapsed,
        );
    }

    pub fn record_resolve_latency(&self, elapsed: Duration) {
        record_latency(
            &self.resolves,
            &self.resolve_latency_micros,
            &self.resolve_latency_max_micros,
            elapsed,
        );
    }

    pub fn record_startup_latency(&self, elapsed: Duration) {
        record_latency(
            &self.startups,
            &self.startup_latency_micros,
            &self.startup_latency_max_micros,
            elapsed,
        );
    }

    pub fn increment_underruns(&self) {
        self.underruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_skips(&self) {
        self.skips.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_failures(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_reconnects(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            commands: self.commands.load(Ordering::Relaxed),
            command_latency_micros: self.command_latency_micros.load(Ordering::Relaxed),
            command_latency_max_micros: self.command_latency_max_micros.load(Ordering::Relaxed),
            resolves: self.resolves.load(Ordering::Relaxed),
            resolve_latency_micros: self.resolve_latency_micros.load(Ordering::Relaxed),
            resolve_latency_max_micros: self.resolve_latency_max_micros.load(Ordering::Relaxed),
            startups: self.startups.load(Ordering::Relaxed),
            startup_latency_micros: self.startup_latency_micros.load(Ordering::Relaxed),
            startup_latency_max_micros: self.startup_latency_max_micros.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            skips: self.skips.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
        }
    }

    /// Renders a dependency-free Prometheus text exposition suitable for an
    /// operator endpoint.
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        let metrics = self.snapshot();
        format!(
            concat!(
                "# TYPE sonoryn_commands_total counter\n",
                "sonoryn_commands_total {}\n",
                "# TYPE sonoryn_command_latency_microseconds_total counter\n",
                "sonoryn_command_latency_microseconds_total {}\n",
                "# TYPE sonoryn_command_latency_microseconds_max gauge\n",
                "sonoryn_command_latency_microseconds_max {}\n",
                "# TYPE sonoryn_resolves_total counter\n",
                "sonoryn_resolves_total {}\n",
                "# TYPE sonoryn_resolve_latency_microseconds_total counter\n",
                "sonoryn_resolve_latency_microseconds_total {}\n",
                "# TYPE sonoryn_resolve_latency_microseconds_max gauge\n",
                "sonoryn_resolve_latency_microseconds_max {}\n",
                "# TYPE sonoryn_voice_startups_total counter\n",
                "sonoryn_voice_startups_total {}\n",
                "# TYPE sonoryn_voice_startup_latency_microseconds_total counter\n",
                "sonoryn_voice_startup_latency_microseconds_total {}\n",
                "# TYPE sonoryn_voice_startup_latency_microseconds_max gauge\n",
                "sonoryn_voice_startup_latency_microseconds_max {}\n",
                "# TYPE sonoryn_underruns_total counter\n",
                "sonoryn_underruns_total {}\n",
                "# TYPE sonoryn_skips_total counter\n",
                "sonoryn_skips_total {}\n",
                "# TYPE sonoryn_failures_total counter\n",
                "sonoryn_failures_total {}\n",
                "# TYPE sonoryn_reconnects_total counter\n",
                "sonoryn_reconnects_total {}\n"
            ),
            metrics.commands,
            metrics.command_latency_micros,
            metrics.command_latency_max_micros,
            metrics.resolves,
            metrics.resolve_latency_micros,
            metrics.resolve_latency_max_micros,
            metrics.startups,
            metrics.startup_latency_micros,
            metrics.startup_latency_max_micros,
            metrics.underruns,
            metrics.skips,
            metrics.failures,
            metrics.reconnects,
        )
    }
}

fn record_latency(count: &AtomicU64, total: &AtomicU64, maximum: &AtomicU64, elapsed: Duration) {
    let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    count.fetch_add(1, Ordering::Relaxed);
    total.fetch_add(micros, Ordering::Relaxed);
    maximum.fetch_max(micros, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Metrics;

    #[test]
    fn latency_metrics_track_count_total_and_maximum() {
        let metrics = Metrics::new();
        metrics.record_command_latency(Duration::from_micros(10));
        metrics.record_command_latency(Duration::from_micros(25));

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.commands, 2);
        assert_eq!(snapshot.command_latency_micros, 35);
        assert_eq!(snapshot.command_latency_max_micros, 25);
    }

    #[test]
    fn reliability_counters_and_text_exposition_are_stable() {
        let metrics = Metrics::new();
        metrics.increment_underruns();
        metrics.increment_skips();
        metrics.increment_failures();
        metrics.increment_reconnects();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.underruns, 1);
        assert_eq!(snapshot.skips, 1);
        assert_eq!(snapshot.failures, 1);
        assert_eq!(snapshot.reconnects, 1);

        let rendered = metrics.render_prometheus();
        assert!(rendered.contains("sonoryn_underruns_total 1\n"));
        assert!(rendered.contains("sonoryn_reconnects_total 1\n"));
    }
}
