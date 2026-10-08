//! Types queries against a running service the way the window does, in bursts with pauses, and
//! measures for every pause how long after the last keystroke the rows for exactly that text
//! arrived. That is what the window shows: answers for text that was typed over are ignored.
//!
//! - `serial`: what the window did before the request pipeline: one task per keystroke, taking
//!   turns on one connection, outdated results dropped once they arrived, the rows asked for
//!   after the search
//! - `pipeline`: requests go through [`reverything_protocol::pipeline`] and the first rows come
//!   with the search, but running searches are not cancelled
//! - `cancel`: like `pipeline`, and a newer search cancels the running one
//!
//! A status poll runs every second in all of them.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eyre::{bail, Result};

use reverything_protocol::pipeline::{Pipeline, Transport};
use reverything_protocol::{Client, Request, Response, Sort};

/// The texts typed one after the other. The typist pauses after each.
const TARGETS: &[&str] = &[
    "note",
    "notepad",
    "",
    "rea",
    "readme.md",
    "readme",
    "",
    "e",
    "",
    "s",
    "src",
    "",
    "index.js",
    "",
];
const PAUSE: Duration = Duration::from_millis(400);

/// The text after every keystroke, and whether the typist pauses after it
fn script() -> Vec<(String, bool)> {
    let mut steps = Vec::new();
    let mut text = String::new();
    for target in TARGETS {
        let common = text
            .bytes()
            .zip(target.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        while text.len() > common {
            text.pop();
            steps.push((text.clone(), false));
        }
        for c in target[common..].chars() {
            text.push(c);
            steps.push((text.clone(), false));
        }
        if let Some(last) = steps.last_mut() {
            last.1 = true;
        }
    }
    steps
}

fn search(query: &str, rows: u32) -> Request {
    Request::Search {
        query: query.to_string(),
        sort: Sort::default(),
        files: true,
        folders: true,
        rows,
    }
}

fn rows(search: u64, page: u64) -> Request {
    Request::Rows {
        search,
        start: page * 256,
        count: 256,
    }
}

fn connect() -> std::io::Result<Client> {
    let mut client = Client::connect()?;
    client.request(&Request::SetActive { active: true })?;
    Ok(client)
}

pub fn run(mode: &str, interval: Duration) -> Result<()> {
    let steps = script();
    let mut at = Vec::with_capacity(steps.len());
    let mut t = Duration::ZERO;
    for (_, pause) in &steps {
        at.push(t);
        t += interval + if *pause { PAUSE } else { Duration::ZERO };
    }

    // Warm up the service: loaded indices, ranking locations
    let mut warm = connect()?;
    for q in ["", "a", "e"] {
        warm.request(&search(q, 0))?;
    }
    drop(warm);

    let start = Instant::now();
    // When the rows for each step arrived
    let done: Arc<Mutex<Vec<Option<Duration>>>> = Arc::new(Mutex::new(vec![None; steps.len()]));
    let latest = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    match mode {
        "serial" => {
            let client = Arc::new(Mutex::new(connect()?));
            std::thread::spawn({
                let (client, stop) = (client.clone(), stop.clone());
                move || {
                    while !stop.load(Ordering::SeqCst) {
                        let _ = client.lock().unwrap().request(&Request::Status);
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            });
            for (k, (text, _)) in steps.iter().enumerate() {
                sleep_until(start + at[k]);
                latest.store(k, Ordering::SeqCst);
                let (client, done, latest, text) =
                    (client.clone(), done.clone(), latest.clone(), text.clone());
                std::thread::spawn(move || {
                    let response = client.lock().unwrap().request(&search(&text, 0));
                    // The window dropped responses of outdated searches
                    if latest.load(Ordering::SeqCst) != k {
                        return;
                    }
                    let Ok(Response::Search { search, .. }) = response else {
                        return;
                    };
                    for page in 0..2 {
                        let _ = client.lock().unwrap().request(&rows(search, page));
                    }
                    done.lock().unwrap()[k] = Some(start.elapsed());
                });
            }
        }
        "pipeline" | "cancel" => {
            let pipeline = Arc::new(Pipeline::new(
                || Ok(Box::new(connect()?) as Box<dyn Transport>),
                || None,
            ));
            if mode == "pipeline" {
                pipeline.keep_running_searches();
            }
            std::thread::spawn({
                let (pipeline, stop) = (pipeline.clone(), stop.clone());
                move || {
                    while !stop.load(Ordering::SeqCst) {
                        let _ = pipeline.send(Request::Status).recv_blocking();
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            });
            for (k, (text, _)) in steps.iter().enumerate() {
                sleep_until(start + at[k]);
                let reply = pipeline.send(search(text, 256));
                let (pipeline, done) = (pipeline.clone(), done.clone());
                std::thread::spawn(move || {
                    // Closed without a reply if a newer search replaced or cancelled it
                    let Ok(Ok(Response::Search { search, .. })) = reply.recv_blocking() else {
                        return;
                    };
                    // The first page came with the answer
                    done.lock().unwrap()[k] = Some(start.elapsed());
                    let _ = pipeline.send(rows(search, 1)).recv_blocking();
                });
            }
        }
        _ => bail!("Mode is serial, pipeline or cancel"),
    }

    // The last step is a pause, so it gets an answer
    let last = steps.len() - 1;
    while done.lock().unwrap()[last].is_none() {
        if start.elapsed() > t + Duration::from_secs(60) {
            bail!("No rows for the last step after a minute");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    stop.store(true, Ordering::SeqCst);

    let done = done.lock().unwrap().clone();
    let mut latencies = Vec::new();
    println!("{:<12} {:>11}", "paused at", "rows after");
    for (k, (text, pause)) in steps.iter().enumerate() {
        if !pause {
            continue;
        }
        let latency = done[k].map(|d| d.saturating_sub(at[k]));
        latencies.extend(latency);
        println!(
            "{:<12} {:>8} ms",
            format!("{:?}", text),
            latency.map_or("-".into(), |l| format!("{:.0}", l.as_secs_f64() * 1000.0))
        );
    }
    latencies.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "{}: keystrokes every {} ms, rows {} ms after the last one before a pause (median), max {:.0} ms",
        mode,
        interval.as_millis(),
        latencies
            .get(latencies.len() / 2)
            .map_or("-".into(), |&l| format!("{:.0}", ms(l))),
        latencies.last().map_or(0.0, |&l| ms(l))
    );
    Ok(())
}

fn sleep_until(at: Instant) {
    if let Some(wait) = at.checked_duration_since(Instant::now()) {
        std::thread::sleep(wait);
    }
}
