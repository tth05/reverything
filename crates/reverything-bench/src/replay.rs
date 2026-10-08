//! Types queries against a running service the way the window does, and measures for every
//! keystroke how long it takes until rows for that text (or something typed later) arrived.
//!
//! `serial` does what the window did before the request pipeline: one task per keystroke,
//! taking turns on one connection, outdated results dropped once they arrived, and the rows
//! asked for after the search. `pipeline` is what it does now: requests go through
//! [`reverything_protocol::pipeline`], the first rows come with the search, and every answer
//! newer than the shown one is shown. A status poll runs every second in both.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eyre::{bail, Result};

use reverything_protocol::pipeline::{Pipeline, Transport};
use reverything_protocol::{Client, Request, Response, Sort};

/// What gets typed: each step is the input text after one keystroke
fn script() -> Vec<String> {
    let mut steps = Vec::new();
    let typed = |text: &str, steps: &mut Vec<String>| {
        for i in 1..=text.len() {
            steps.push(text[..i].to_string());
        }
        for i in (0..text.len()).rev() {
            steps.push(text[..i].to_string());
        }
    };
    typed("notepad", &mut steps);
    typed("readme.md", &mut steps);
    typed("e", &mut steps);
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
    // Warm up the service: loaded indices, ranking locations
    let mut warm = connect()?;
    for q in ["", "a", "e"] {
        warm.request(&search(q, 0))?;
    }
    drop(warm);

    let start = Instant::now();
    // When rows for each step arrived
    let done: Arc<Mutex<Vec<Option<Duration>>>> = Arc::new(Mutex::new(vec![None; steps.len()]));
    let latest = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();

    match mode {
        "serial" => {
            let client = Arc::new(Mutex::new(connect()?));
            threads.push(std::thread::spawn({
                let (client, stop) = (client.clone(), stop.clone());
                move || {
                    while !stop.load(Ordering::SeqCst) {
                        let _ = client.lock().unwrap().request(&Request::Status);
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }));
            for (k, text) in steps.iter().enumerate() {
                sleep_until(start + interval * k as u32);
                latest.store(k, Ordering::SeqCst);
                let (client, done, latest, text) =
                    (client.clone(), done.clone(), latest.clone(), text.clone());
                threads.push(std::thread::spawn(move || {
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
                }));
            }
        }
        "pipeline" => {
            let pipeline = Arc::new(Pipeline::new(
                || Ok(Box::new(connect()?) as Box<dyn Transport>),
                || None,
            ));
            threads.push(std::thread::spawn({
                let (pipeline, stop) = (pipeline.clone(), stop.clone());
                move || {
                    while !stop.load(Ordering::SeqCst) {
                        let _ = pipeline.send(Request::Status).recv_blocking();
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }));
            for (k, text) in steps.iter().enumerate() {
                sleep_until(start + interval * k as u32);
                let reply = pipeline.send(search(text, 256));
                let (pipeline, done) = (pipeline.clone(), done.clone());
                threads.push(std::thread::spawn(move || {
                    // Closed without a reply if a newer search replaced it
                    let Ok(Ok(Response::Search { search, .. })) = reply.recv_blocking() else {
                        return;
                    };
                    // The first page came with the answer and can be shown, even if newer
                    // input is on its way
                    done.lock().unwrap()[k] = Some(start.elapsed());
                    let _ = pipeline.send(rows(search, 1)).recv_blocking();
                }));
            }
        }
        _ => bail!("Mode is serial or pipeline"),
    }

    // Everything typed is answered once the last step is
    let last = steps.len() - 1;
    while done.lock().unwrap()[last].is_none() {
        if start.elapsed() > interval * steps.len() as u32 + Duration::from_secs(60) {
            bail!("No rows for the last step after a minute");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    stop.store(true, Ordering::SeqCst);

    let done = done.lock().unwrap().clone();
    let mut latencies = Vec::new();
    println!("{:<12} {:>9} {:>11}", "input", "typed at", "rows after");
    for (k, text) in steps.iter().enumerate() {
        let typed = interval * k as u32;
        // Rows for this or a later input
        let shown = done[k..].iter().flatten().min().copied();
        let latency = shown.map(|s| s.saturating_sub(typed));
        if let Some(l) = latency {
            latencies.push(l);
        }
        println!(
            "{:<12} {:>7} ms {:>8} ms",
            format!("{:?}", text),
            typed.as_millis(),
            latency.map_or("-".into(), |l| format!("{:.0}", l.as_secs_f64() * 1000.0))
        );
    }
    latencies.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "{}: {} keystrokes every {} ms, rows after: median {:.0} ms, 90% {:.0} ms, max {:.0} ms",
        mode,
        steps.len(),
        interval.as_millis(),
        ms(latencies[latencies.len() / 2]),
        ms(latencies[latencies.len() * 9 / 10]),
        ms(*latencies.last().unwrap())
    );
    Ok(())
}

fn sleep_until(at: Instant) {
    if let Some(wait) = at.checked_duration_since(Instant::now()) {
        std::thread::sleep(wait);
    }
}
