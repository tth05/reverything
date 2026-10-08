//! Shared connection to the service. One worker thread owns the pipe and decides what to send
//! next, so the newest search does not wait behind outdated ones, see
//! [`reverything_protocol::pipeline`].

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reverything_protocol::pipeline::{Pipeline, Reply, Transport};
use reverything_protocol::{Client, Request, Response};

pub struct ServiceClient {
    pipeline: Pipeline,
    /// Whether the window is active. The service only keeps the index loaded and live while a
    /// client is active, so this is sent on every new connection.
    active: Arc<AtomicBool>,
}

impl ServiceClient {
    pub fn new(active: bool) -> Self {
        let active = Arc::new(AtomicBool::new(active));
        let greeting = active.clone();
        let pipeline = Pipeline::new(
            || Ok(Box::new(Client::connect()?) as Box<dyn Transport>),
            // Sent before anything else, a search would otherwise count as active
            move || {
                Some(Request::SetActive {
                    active: greeting.load(Ordering::SeqCst),
                })
            },
        );
        Self { pipeline, active }
    }

    /// Tells the service whether the window is active.
    pub fn set_active(&self, active: bool) -> Result<(), String> {
        self.active.store(active, Ordering::SeqCst);
        self.request(&Request::SetActive { active }).map(|_| ())
    }

    /// Sends a request and waits for the answer, for requests that are never replaced
    /// (everything but searches, rows and status).
    pub fn request(&self, request: &Request) -> Result<Response, String> {
        let receiver = self.pipeline.send(request.clone());
        match receiver.recv_blocking() {
            Ok(reply) => convert(reply),
            Err(_) => Err("The request was dropped".into()),
        }
    }

    /// Queues a request. Resolves to `None` if a newer request made it obsolete before it was
    /// sent: a newer search, or for rows, a newer result set.
    pub fn send(&self, request: Request) -> impl Future<Output = Option<Result<Response, String>>> {
        let receiver = self.pipeline.send(request);
        async move { receiver.recv().await.ok().map(convert) }
    }

    /// How long establishing the last connection took
    pub fn connect_time(&self) -> Option<Duration> {
        self.pipeline.connect_time()
    }
}

fn convert(reply: Reply) -> Result<Response, String> {
    match reply {
        Ok(Response::Error(e)) => Err(e),
        Ok(response) => Ok(response),
        Err(e) => Err(describe(e)),
    }
}

fn describe(e: std::io::Error) -> String {
    match e.raw_os_error() {
        // ERROR_FILE_NOT_FOUND: nobody listens on the pipe
        Some(2) => "The Reverything service is not running".into(),
        // ERROR_ACCESS_DENIED
        Some(5) => "Access to the Reverything service was denied".into(),
        _ => e.to_string(),
    }
}
