//! Debug/diagnostic tooling. Compiled in but inert unless explicitly enabled
//! via an environment flag, so it adds no cost to a normal run.

pub mod perf_monitor;
pub mod perf_observer;
