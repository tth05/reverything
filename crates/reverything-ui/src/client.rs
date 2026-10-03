//! Shared connection to the service. Requests block, so they run on GPUI's background
//! executor; the mutex keeps them in order on the single pipe.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reverything_protocol::{Client, Request, Response};

pub struct ServiceClient {
    client: Mutex<Option<Client>>,
    connect_time: Mutex<Option<Duration>>,
    /// Whether the window is active. The service only keeps the index loaded and live while a
    /// client is active, so this is sent on every new connection.
    active: AtomicBool,
}

impl ServiceClient {
    pub fn new(active: bool) -> Self {
        Self {
            client: Mutex::new(None),
            connect_time: Mutex::new(None),
            active: AtomicBool::new(active),
        }
    }

    /// Tells the service whether the window is active.
    pub fn set_active(&self, active: bool) -> Result<(), String> {
        self.active.store(active, Ordering::SeqCst);
        self.request(&Request::SetActive { active }).map(|_| ())
    }

    /// Sends a request, connecting first if needed. A failed request drops the connection so the
    /// next one reconnects, e.g. after the service restarted.
    pub fn request(&self, request: &Request) -> Result<Response, String> {
        let mut client = self.client.lock().unwrap();
        if client.is_none() {
            let t = Instant::now();
            let mut connected = Client::connect().map_err(describe)?;
            *self.connect_time.lock().unwrap() = Some(t.elapsed());
            // Sent before anything else, a search would otherwise count as active
            if !matches!(request, Request::SetActive { .. }) {
                let active = self.active.load(Ordering::SeqCst);
                connected
                    .request(&Request::SetActive { active })
                    .map_err(describe)?;
            }
            *client = Some(connected);
        }

        match client.as_mut().unwrap().request(request) {
            Ok(Response::Error(e)) => Err(e),
            Ok(response) => Ok(response),
            Err(e) => {
                *client = None;
                Err(describe(e))
            }
        }
    }

    /// How long establishing the last connection took
    pub fn connect_time(&self) -> Option<Duration> {
        *self.connect_time.lock().unwrap()
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
