//! A connection to the service shared by a client's requests, which decides what to send next.
//!
//! The service answers one request at a time per connection, and a search can take a while on
//! large indices. While typing, sending every keystroke's search in order would make the latest
//! one wait for all the outdated ones. So one worker thread owns the connection and picks the
//! next request when it is free:
//!
//! 1. Control requests (activation, drive changes), in order
//! 2. The newest search; one that was not sent yet is replaced by a newer one
//! 3. Rows of the current result set, in order; rows of older result sets are dropped
//! 4. The newest status request
//!
//! When a newer search arrives while one runs, a second thread cancels the running one over a
//! connection of its own ([`Request::Cancel`]), so the newer one starts right away.
//!
//! A request that is dropped, replaced or cancelled closes its reply channel without a reply.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::{Client, Request, Response};

/// A connection that answers requests one at a time.
pub trait Transport: Send {
    fn request(&mut self, request: &Request) -> io::Result<Response>;

    /// Identifies the connection for [`Request::Cancel`]
    fn session(&self) -> u64;
}

impl Transport for Client {
    fn request(&mut self, request: &Request) -> io::Result<Response> {
        Client::request(self, request)
    }

    fn session(&self) -> u64 {
        Client::session(self)
    }
}

pub type Reply = io::Result<Response>;

struct Job {
    request: Request,
    reply: async_channel::Sender<Reply>,
}

#[derive(Default)]
struct Queue {
    control: VecDeque<Job>,
    search: Option<Job>,
    rows: VecDeque<Job>,
    status: Option<Job>,
    /// Session and number of the search the service is working on
    running: Option<(u64, u64)>,
    /// A cancel for the canceller to send
    cancel: Option<(u64, u64)>,
    /// The newest search a cancel was asked for, so it is asked for once
    cancelled: Option<(u64, u64)>,
    /// Cancelling is off, for comparing
    keep_running: bool,
    closed: bool,
}

impl Queue {
    fn push(&mut self, job: Job) {
        match job.request {
            // A replaced job is dropped, which closes its reply channel
            Request::Search { .. } => {
                self.search = Some(job);
                // The running search is outdated now
                if self.running.is_some() && self.running != self.cancelled && !self.keep_running {
                    self.cancel = self.running;
                    self.cancelled = self.running;
                }
            }
            Request::Rows { .. } => self.rows.push_back(job),
            Request::Status => self.status = Some(job),
            _ => self.control.push_back(job),
        }
    }

    /// The next job to send. Rows of result sets older than `current` and jobs nobody waits for
    /// anymore are dropped.
    fn next(&mut self, current: u64) -> Option<Job> {
        let wanted = |job: &Job| !job.reply.is_closed();
        if let Some(job) = self.control.pop_front() {
            return Some(job);
        }
        if let Some(job) = self.search.take().filter(wanted) {
            return Some(job);
        }
        while let Some(job) = self.rows.pop_front() {
            if matches!(job.request, Request::Rows { search, .. } if search >= current)
                && wanted(&job)
            {
                return Some(job);
            }
        }
        self.status.take().filter(wanted)
    }
}

type Connect = Arc<dyn Fn() -> io::Result<Box<dyn Transport>> + Send + Sync>;
type Greeting = Box<dyn Fn() -> Option<Request> + Send>;
type Shared = Arc<(Mutex<Queue>, Condvar)>;

pub struct Pipeline {
    shared: Shared,
    connect_time: Arc<Mutex<Option<Duration>>>,
}

impl Pipeline {
    /// Starts the worker and the canceller. `connect` opens a connection, `greeting` gives a
    /// request to send first on every new connection of the worker.
    pub fn new(
        connect: impl Fn() -> io::Result<Box<dyn Transport>> + Send + Sync + 'static,
        greeting: impl Fn() -> Option<Request> + Send + 'static,
    ) -> Self {
        let shared: Shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let connect: Connect = Arc::new(connect);
        let connect_time = Arc::new(Mutex::new(None));
        let worker = Worker {
            shared: shared.clone(),
            connect: connect.clone(),
            greeting: Box::new(greeting),
            connect_time: connect_time.clone(),
        };
        std::thread::Builder::new()
            .name("service connection".into())
            .spawn(move || worker.run())
            .expect("Failed to start the service connection thread");
        let canceller = shared.clone();
        std::thread::Builder::new()
            .name("search cancel".into())
            .spawn(move || cancel_searches(canceller, connect))
            .expect("Failed to start the search cancel thread");
        Self {
            shared,
            connect_time,
        }
    }

    /// Queues `request`. The reply arrives on the returned channel, which closes without one if
    /// the request was replaced, dropped or cancelled.
    pub fn send(&self, request: Request) -> async_channel::Receiver<Reply> {
        let (reply, receiver) = async_channel::bounded(1);
        let (queue, wake) = &*self.shared;
        queue.lock().unwrap().push(Job { request, reply });
        wake.notify_all();
        receiver
    }

    /// Lets running searches finish even when a newer one arrives, to measure what cancelling
    /// them brings.
    pub fn keep_running_searches(&self) {
        self.shared.0.lock().unwrap().keep_running = true;
    }

    /// How long establishing the last connection took
    pub fn connect_time(&self) -> Option<Duration> {
        *self.connect_time.lock().unwrap()
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let (queue, wake) = &*self.shared;
        queue.lock().unwrap().closed = true;
        wake.notify_all();
    }
}

struct Worker {
    shared: Shared,
    connect: Connect,
    greeting: Greeting,
    connect_time: Arc<Mutex<Option<Duration>>>,
}

impl Worker {
    fn run(mut self) {
        let mut connection: Option<Box<dyn Transport>> = None;
        // Number of the newest search the service answered on this connection
        let mut current = 0;
        loop {
            let job = {
                let (queue, wake) = &*self.shared;
                let mut queue = queue.lock().unwrap();
                loop {
                    if queue.closed {
                        return;
                    }
                    if let Some(job) = queue.next(current) {
                        break job;
                    }
                    queue = wake.wait(queue).unwrap();
                }
            };

            if connection.is_none() {
                match self.connect() {
                    Ok(connected) => {
                        connection = Some(connected);
                        current = 0;
                    }
                    Err(e) => {
                        let _ = job.reply.try_send(Err(e));
                        continue;
                    }
                }
            }
            let transport = connection.as_mut().unwrap();

            let search = matches!(job.request, Request::Search { .. });
            if search {
                // The service numbers the searches of a connection
                self.shared.0.lock().unwrap().running = Some((transport.session(), current + 1));
            }
            let reply = transport.request(&job.request);
            if search {
                self.shared.0.lock().unwrap().running = None;
            }

            match &reply {
                Ok(Response::Search { search, .. }) => current = *search,
                // Closes the reply channel: a newer search is on its way
                Ok(Response::Cancelled { search }) => {
                    current = *search;
                    continue;
                }
                Ok(_) => {}
                // Reconnect for the next request, e.g. after the service restarted
                Err(_) => connection = None,
            }
            // Nobody waiting is fine
            let _ = job.reply.try_send(reply);
        }
    }

    fn connect(&mut self) -> io::Result<Box<dyn Transport>> {
        let t = Instant::now();
        let mut connected = (self.connect)()?;
        *self.connect_time.lock().unwrap() = Some(t.elapsed());
        if let Some(greeting) = (self.greeting)() {
            connected.request(&greeting)?;
        }
        Ok(connected)
    }
}

/// Sends the cancels the queue asks for, on a connection of its own.
fn cancel_searches(shared: Shared, connect: Connect) {
    let mut connection: Option<Box<dyn Transport>> = None;
    loop {
        let (session, search) = {
            let (queue, wake) = &*shared;
            let mut queue = queue.lock().unwrap();
            loop {
                if queue.closed {
                    return;
                }
                if let Some(cancel) = queue.cancel.take() {
                    break cancel;
                }
                queue = wake.wait(queue).unwrap();
            }
        };
        if connection.is_none() {
            connection = connect().ok();
        }
        if let Some(c) = connection.as_mut() {
            if c.request(&Request::Cancel { session, search }).is_err() {
                connection = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Sort;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    /// Records the requests it gets and answers them once the test allows it. A search is
    /// answered as cancelled if a cancel for it arrived on another connection meanwhile.
    struct Fake {
        sent: mpsc::Sender<String>,
        go: Arc<(Mutex<usize>, Condvar)>,
        searches: u64,
        cancelled: Arc<AtomicU64>,
    }

    impl Transport for Fake {
        fn request(&mut self, request: &Request) -> io::Result<Response> {
            if let Request::Cancel { session, search } = request {
                self.sent
                    .send(format!("cancel {} {}", session, search))
                    .unwrap();
                self.cancelled.fetch_max(*search, Ordering::SeqCst);
                return Ok(Response::Done);
            }
            if let Request::Search { query, .. } = request {
                self.searches += 1;
                self.sent.send(format!("search {}", query)).unwrap();
            }
            let (allowed, cv) = &*self.go;
            let mut allowed = cv.wait_while(allowed.lock().unwrap(), |a| *a == 0).unwrap();
            *allowed -= 1;
            drop(allowed);
            Ok(match request {
                Request::Search { .. }
                    if self.cancelled.load(Ordering::SeqCst) >= self.searches =>
                {
                    Response::Cancelled {
                        search: self.searches,
                    }
                }
                Request::Search { .. } => Response::Search {
                    search: self.searches,
                    total: 0,
                    took_us: 0,
                    rows: Vec::new(),
                },
                Request::Rows { search, start, .. } => {
                    self.sent
                        .send(format!("rows {} {}", search, start))
                        .unwrap();
                    Response::Rows {
                        search: *search,
                        start: *start,
                        rows: Vec::new(),
                    }
                }
                other => {
                    self.sent.send(format!("{:?}", other)).unwrap();
                    Response::Done
                }
            })
        }

        fn session(&self) -> u64 {
            7
        }
    }

    fn search(query: &str) -> Request {
        Request::Search {
            query: query.into(),
            sort: Sort::default(),
            files: true,
            folders: true,
            rows: 0,
        }
    }

    fn rows(search: u64, start: u64) -> Request {
        Request::Rows {
            search,
            start,
            count: 256,
        }
    }

    #[test]
    fn newest_search_first_outdated_work_dropped() {
        let (sent, log) = mpsc::channel();
        let go = Arc::new((Mutex::new(0), Condvar::new()));
        let cancelled = Arc::new(AtomicU64::new(0));
        let (fake_sent, fake_go) = (Mutex::new(sent), go.clone());
        let pipeline = Pipeline::new(
            move || {
                Ok(Box::new(Fake {
                    sent: fake_sent.lock().unwrap().clone(),
                    go: fake_go.clone(),
                    searches: 0,
                    cancelled: cancelled.clone(),
                }) as Box<dyn Transport>)
            },
            || Some(Request::SetActive { active: true }),
        );
        let allow = |n: usize| {
            *go.0.lock().unwrap() += n;
            go.1.notify_all();
        };
        let next = || log.recv_timeout(Duration::from_secs(5)).unwrap();

        // The first search is sent right away (after the greeting) and keeps running
        let first = pipeline.send(search("a"));
        allow(1);
        assert_eq!(next(), "SetActive { active: true }");
        assert_eq!(next(), "search a");
        // Meanwhile: a status poll, rows of an earlier result set, typing
        let status = pipeline.send(Request::Status);
        let old_rows = pipeline.send(rows(0, 0));
        let replaced = pipeline.send(search("ab"));
        // Typing cancels the running search, once
        assert_eq!(next(), "cancel 7 1");
        let latest = pipeline.send(search("abc"));
        let new_rows = pipeline.send(rows(2, 256));

        allow(1);
        assert!(
            first.recv_blocking().is_err(),
            "a cancelled search gets no reply"
        );
        assert!(
            replaced.recv_blocking().is_err(),
            "a replaced search gets no reply"
        );
        assert_eq!(next(), "search abc");
        allow(1);
        assert!(matches!(
            latest.recv_blocking(),
            Ok(Ok(Response::Search { search: 2, .. }))
        ));
        assert!(
            old_rows.recv_blocking().is_err(),
            "rows of an old result set"
        );

        allow(2);
        assert_eq!(next(), "rows 2 256");
        assert!(new_rows.recv_blocking().is_ok());
        assert_eq!(next(), "Status");
        assert!(status.recv_blocking().is_ok());
    }
}
