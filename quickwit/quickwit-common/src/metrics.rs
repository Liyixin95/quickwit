// Copyright (C) 2022 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use prometheus::{Encoder, HistogramOpts, Opts, TextEncoder};
pub use prometheus::{
    Histogram, HistogramTimer, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec,
};

pub fn new_counter(name: &str, description: &str, namespace: &str) -> IntCounter {
    let counter_opts = Opts::new(name, description).namespace(namespace);
    let counter = IntCounter::with_opts(counter_opts).expect("Failed to create counter");
    prometheus::register(Box::new(counter.clone())).expect("Failed to register counter");
    counter
}

pub fn new_counter_vec(
    name: &str,
    description: &str,
    namespace: &str,
    labels: &[&str],
) -> IntCounterVec {
    let counter_opts = Opts::new(name, description).namespace(namespace);
    let counter_vec =
        IntCounterVec::new(counter_opts, labels).expect("Failed to create counter vec");
    prometheus::register(Box::new(counter_vec.clone())).expect("Failed to register counter");
    counter_vec
}

pub fn new_histogram(name: &str, description: &str, namespace: &str) -> Histogram {
    let histogram_opts = HistogramOpts::new(name, description).namespace(namespace);
    let histogram = Histogram::with_opts(histogram_opts).expect("Failed to create histogram");
    prometheus::register(Box::new(histogram.clone())).expect("Failed to register counter");
    histogram
}

pub fn new_histogram_vec(
    name: &str,
    description: &str,
    namespace: &str,
    labels: &[&str],
) -> HistogramVec {
    let histogram_opts = HistogramOpts::new(name, description).namespace(namespace);
    let histogram_vec =
        HistogramVec::new(histogram_opts, labels).expect("Failed to create histogram vec");
    prometheus::register(Box::new(histogram_vec.clone())).expect("Failed to register counter");
    histogram_vec
}

pub fn new_gauge(name: &str, description: &str, namespace: &str) -> IntGauge {
    let gauge_opts = Opts::new(name, description).namespace(namespace);
    let gauge = IntGauge::with_opts(gauge_opts).expect("Failed to create gauge");
    prometheus::register(Box::new(gauge.clone())).expect("Failed to register gauge");
    gauge
}

pub fn new_gauge_vec(
    name: &str,
    description: &str,
    namespace: &str,
    labels: &[&str],
) -> IntGaugeVec {
    let gauge_opts = Opts::new(name, description).namespace(namespace);
    let gauge_vec = IntGaugeVec::new(gauge_opts, labels).expect("Failed to create gauge vec");
    prometheus::register(Box::new(gauge_vec.clone())).expect("Failed to register gauge vec");
    gauge_vec
}

pub fn metrics_handler() -> impl warp::Reply {
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    let encoder = TextEncoder::new();
    let _ = encoder.encode(&metric_families, &mut buffer); // TODO avoid ignoring the error.
    String::from_utf8_lossy(&buffer).to_string()
}

pub fn create_gauge_guard(gauge: &'static IntGauge) -> GaugeGuard {
    gauge.inc();
    GaugeGuard(gauge)
}

pub struct GaugeGuard(&'static IntGauge);

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.0.dec();
    }
}
