//! On-demand, unprivileged CPU profiles (feature `pprof`).
//!
//! `perf` needs `kernel.perf_event_paranoid` below what the benchmark host
//! allows an ordinary user; pprof samples with `setitimer(ITIMER_PROF)` and a
//! signal handler instead, so it runs as the node's own user.
//!
//! With `N42_PPROF_SECS=<n>` set, a thread watches `<datadir>/pprof.start`.
//! Creating that file starts one capture of `n` seconds at `N42_PPROF_HZ`
//! (default 499); the file is removed and the report is written next to it as
//! `pprof-<unix seconds>.txt`: the hottest functions by self samples and by
//! inclusive samples, and the sample count per thread name, plus every stack
//! in folded form (`pprof-<unix seconds>.folded`, root first) for caller
//! attribution or a flame graph. Build with
//! `--profile profiling` so the binary keeps its symbols.

use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Starts the watcher thread if `N42_PPROF_SECS` is set.
pub(crate) fn spawn_from_env(datadir: PathBuf) {
    let Some(secs) = std::env::var("N42_PPROF_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return;
    };
    let hz = std::env::var("N42_PPROF_HZ")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(499);
    let trigger = datadir.join("pprof.start");
    tracing::info!(target: "n42::pprof", trigger = %trigger.display(), secs, hz, "pprof capture armed");
    std::thread::Builder::new()
        .name("n42-pprof".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(200));
                if std::fs::remove_file(&trigger).is_err() {
                    continue;
                }
                match capture(secs, hz) {
                    Ok((text, folded)) => {
                        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                        let out = datadir.join(format!("pprof-{}.txt", now.as_secs()));
                        let _ = std::fs::write(datadir.join(format!("pprof-{}.folded", now.as_secs())), folded);
                        match std::fs::write(&out, text) {
                            Ok(()) => tracing::info!(target: "n42::pprof", out = %out.display(), "pprof report written"),
                            Err(error) => tracing::warn!(target: "n42::pprof", %error, "pprof report not written"),
                        }
                    }
                    Err(error) => tracing::warn!(target: "n42::pprof", %error, "pprof capture failed"),
                }
            }
        })
        .expect("spawn pprof watcher");
}

fn capture(secs: u64, hz: i32) -> Result<(String, String), pprof::Error> {
    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(hz)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()?;
    std::thread::sleep(Duration::from_secs(secs));
    let report = guard.report().build()?;

    let mut total = 0isize;
    let mut self_samples: HashMap<String, isize> = HashMap::new();
    let mut inclusive: HashMap<String, isize> = HashMap::new();
    let mut threads: HashMap<String, isize> = HashMap::new();
    let mut folded = String::new();
    for (frames, count) in &report.data {
        total += count;
        // Thread names carry a numeric suffix per worker; group them.
        let thread = frames
            .thread_name
            .trim_end_matches(|c: char| c.is_ascii_digit() || c == '-');
        *threads.entry(thread.to_string()).or_default() += count;
        // `frames` is leaf first; each frame may hold several inlined symbols.
        let names: Vec<String> = frames
            .frames
            .iter()
            .flatten()
            .map(|symbol| symbol.name())
            .collect();
        let _ = writeln!(
            folded,
            "{thread};{} {count}",
            names
                .iter()
                .rev()
                .map(|name| name.replace(';', ":"))
                .collect::<Vec<_>>()
                .join(";")
        );
        if let Some(leaf) = names.first() {
            *self_samples.entry(leaf.clone()).or_default() += count;
        }
        let mut seen = HashSet::new();
        for name in names {
            if seen.insert(name.clone()) {
                *inclusive.entry(name).or_default() += count;
            }
        }
    }

    let mut out = String::new();
    let _ = writeln!(out, "pprof: {total} samples over {secs}s at {hz} Hz");
    for (title, table, limit) in [
        ("threads", &threads, 40),
        ("self", &self_samples, 80),
        ("inclusive", &inclusive, 120),
    ] {
        let mut rows: Vec<_> = table.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1));
        let _ = writeln!(out, "\n== {title} ==");
        for (name, count) in rows.into_iter().take(limit) {
            let share = 100.0 * *count as f64 / total.max(1) as f64;
            let _ = writeln!(out, "{share:6.2}% {count:8} {name}");
        }
    }
    Ok((out, folded))
}
