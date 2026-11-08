//! A line in the log every ten seconds while the computer is busy: how
//! busy each CPU was, how much memory is in use, and which programs took
//! the time. On a laptop that's tested by booting a USB stick and sending
//! its log back, this is how slowness (a video in Firefox, say) can be
//! seen afterwards without anyone watching `top`.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::memory::frames;
use crate::{process, task};

const PERIOD_MS: u64 = 10_000;
/// Quieter than this (all CPUs together), nothing is logged.
const BUSY_PERCENT: u64 = 10;

pub fn start() {
    task::spawn("load", watch);
}

fn watch() {
    let mut cpus = task::cpu_times();
    let mut programs = program_times();
    loop {
        task::sleep_ms(PERIOD_MS);
        let now_cpus = task::cpu_times();
        let now_programs = program_times();
        let percents: Vec<u64> = now_cpus
            .iter()
            .zip(&cpus)
            .map(|(&(busy, idle), &(old_busy, old_idle))| {
                let (busy, idle) = (busy.saturating_sub(old_busy), idle.saturating_sub(old_idle));
                (busy * 100).checked_div(busy + idle).unwrap_or(0)
            })
            .collect();
        let average = percents.iter().sum::<u64>() / percents.len().max(1) as u64;
        if average >= BUSY_PERCENT {
            info!("{}", describe(&percents, average, &programs, &now_programs));
        }
        cpus = now_cpus;
        programs = now_programs;
    }
}

/// By process: name, CPU milliseconds so far, and memory in use.
type Programs = BTreeMap<process::Pid, (String, u64, u64)>;

fn program_times() -> Programs {
    process::list().into_iter().map(|p| (p.pid, (p.name, p.cpu_ms, p.memory))).collect()
}

fn describe(percents: &[u64], average: u64, before: &Programs, after: &Programs) -> String {
    let memory = frames::stats();
    let mib = |bytes: u64| bytes / (1024 * 1024);
    let mut text = String::new();
    let _ = write!(text, "load: CPUs {average}% (");
    for (index, percent) in percents.iter().enumerate() {
        let _ = write!(text, "{}{percent}", if index == 0 { "" } else { " " });
    }
    let _ = write!(
        text,
        "), memory {} of {} MiB used; busiest:",
        mib(memory.managed_bytes - memory.free_bytes),
        mib(memory.managed_bytes)
    );
    // Each program's share of one CPU over the period.
    let mut busiest: Vec<(u64, process::Pid, &str, u64)> = after
        .iter()
        .map(|(&pid, (name, ms, memory))| {
            let old = before.get(&pid).map_or(0, |&(_, old, _)| old);
            (ms.saturating_sub(old) * 100 / PERIOD_MS, pid, name.as_str(), *memory)
        })
        .filter(|&(percent, ..)| percent > 0)
        .collect();
    busiest.sort_unstable_by_key(|&(percent, ..)| core::cmp::Reverse(percent));
    for (percent, pid, name, memory) in busiest.iter().take(5) {
        let _ = write!(text, " {name} ({pid}) {percent}% {} MiB,", mib(*memory));
    }
    text.pop(); // the last comma (or the colon, with nothing to list)
    text
}
