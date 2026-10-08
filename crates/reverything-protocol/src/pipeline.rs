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
//! A request that is dropped or replaced closes its reply channel without a reply.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::{Client, Request, Response};

/// A connection that answers requests one at a time.
pub trait Transport: Send {
    fn request(&mut self, request: &Request) -> io::Result<Response>;
}

impl Transport for Client {
    fn request(&mut self, request: &Request) -> io::Result<Response> {
        Client::request(self, request)
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
    closed: bool,
}

impl Queue {
    fn push(&mut self, job: Job) {
        match job.request {
            // A replaced job is dropped, which closes its reply channel
            Request::Search { .. } => self.search = Some(job),
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

type Connect = Box<dyn FnMut() -> io::Result<Box<dyn Transport>> + Send>;
type Greeting = Box<dyn Fn() -> Option<Request> + Send>;

pub struct Pipeline {
    shared: Arc<(Mutex<Queue>, Condvar)>,
    connect_time: Arc<Mutex<Option<Duration>>>,
}

impl Pipeline {
    /// Starts the worker. `connect` opens a connection when there is none, `greeting` gives a
    /// request to send first on every new connection.
    pub fn new(
        connect: impl FnMut() -> io::Result<Box<dyn Transport>> + Send + 'static,
        greeting: impl Fn() -> Option<Request> + Send + 'static,
    ) -> Self {
        let shared = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let connect_time = Arc::new(Mutex::new(None));
        let worker = Worker {
            shared: shared.clone(),
            connect: Box::new(connect),
            greeting: Box::new(greeting),
            connect_time: connect_time.clone(),
        };
        std::thread::Builder::new()
            .name("service connection".into())
            .spawn(move || worker.run())
            .expect("Failed to start the service connection thread");
        Self {
            shared,
            connect_time,
        }
    }

    /// Queues `request`. The reply arrives on the returned channel, which closes without one if
    /// the request was replaced or dropped.
    pub fn send(&self, request: Request) -> async_channel::Receiver<Reply> {
        let (reply, receiver) = async_channel::bounded(1);
        let (queue, wake) = &*self.shared;
        queue.lock().unwrap().push(Job { request, reply });
        wake.notify_one();
        receiver
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
        wake.notify_one();
    }
}

struct Worker {
    shared: Arc<(Mutex<Queue>, Condvar)>,
    connect: Connect,
    greeting: Greeting,
    connect_time: Arc<Mutex<Option<Duration>>>,
}

impl Worker {
    fn run(mut self) {
        let mut connection: Option<Box<dyn Transport>> = None;
        // Id of the newest result set the service created
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

            let reply = self.send(&mut connection, &job.request);
            if let Ok(Response::Search { search, .. }) = &reply {
                current = *search;
            }
            // Nobody waiting is fine
            let _ = job.reply.try_send(reply);
        }
    }

    /// Sends a request, connecting first if needed. A failed request drops the connection so
    /// the next one reconnects, e.g. after the service restarted.
    fn send(&mut self, connection: &mut Option<Box<dyn Transport>>, request: &Request) -> Reply {
        if connection.is_none() {
            let t = Instant::now();
            let mut connected = (self.connect)()?;
            *self.connect_time.lock().unwrap() = Some(t.elapsed());
            if let Some(greeting) = (self.greeting)() {
                connected.request(&greeting)?;
            }
            *connection = Some(connected);
        }
        let result = connection.as_mut().unwrap().request(request);
        if result.is_err() {
            *connection = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Sort;
    use std::sync::mpsc;

    /// Records the requests it gets and answers them once the test allows it.
    struct Fake {
        sent: mpsc::Sender<String>,
        go: Arc<(Mutex<usize>, Condvar)>,
        searches: u64,
    }

    impl Transport for Fake {
        fn request(&mut self, request: &Request) -> io::Result<Response> {
            let (allowed, cv) = &*self.go;
            let mut allowed = cv.wait_while(allowed.lock().unwrap(), |a| *a == 0).unwrap();
            *allowed -= 1;
            drop(allowed);
            Ok(match request {
                Request::Search { query, .. } => {
                    self.searches += 1;
                    self.sent.send(format!("search {}", query)).unwrap();
                    Response::Search {
                        search: self.searches,
                        total: 0,
                        took_us: 0,
                        rows: Vec::new(),
                    }
                }
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
    fn newest_search_first_and_outdated_rows_dropped() {
        let (sent, log) = mpsc::channel();
        let go = Arc::new((Mutex::new(0), Condvar::new()));
        let fake_go = go.clone();
        let mut fake = Some(Fake {
            sent,
            go: fake_go,
            searches: 0,
        });
        let pipeline = Pipeline::new(
            move || Ok(Box::new(fake.take().expect("Connected twice")) as Box<dyn Transport>),
            || Some(Request::SetActive { active: true }),
        );
        let allow = |n: usize| {
            *go.0.lock().unwrap() += n;
            go.1.notify_all();
        };
        let next = || log.recv_timeout(Duration::from_secs(5)).unwrap();

        // The first search is sent right away (after the greeting) and blocks the connection
        let first = pipeline.send(search("a"));
        allow(1);
        assert_eq!(next(), "SetActive { active: true }");
        // Meanwhile: typing, rows of the first search, a status poll
        let status = pipeline.send(Request::Status);
        let old_rows = pipeline.send(rows(1, 0));
        let replaced = pipeline.send(search("ab"));
        let latest = pipeline.send(search("abc"));

        allow(1);
        assert_eq!(next(), "search a");
        assert!(first.recv_blocking().is_ok());
        assert!(
            replaced.recv_blocking().is_err(),
            "a replaced search gets no reply"
        );

        // Rows of the result set that is on its way, queued before its reply
        let new_rows = pipeline.send(rows(2, 256));
        allow(1);
        assert_eq!(next(), "search abc");
        assert!(matches!(
            latest.recv_blocking(),
            Ok(Ok(Response::Search { search: 2, .. }))
        ));
        // Rows of the first result set are outdated now
        assert!(old_rows.recv_blocking().is_err());

        allow(2);
        assert_eq!(next(), "rows 2 256");
        assert!(new_rows.recv_blocking().is_ok());
        assert_eq!(next(), "Status");
        assert!(status.recv_blocking().is_ok());
    }
}
