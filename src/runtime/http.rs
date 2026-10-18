//! Sending HTTP requests.

use std::time::{Duration, Instant};
use ureq::RequestExt;

/// Methods servers commonly implement, as defined by RFC 9110 and RFC 5789.
pub const STANDARD_METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

#[derive(Debug, Clone)]
pub struct Prepared {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Received {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub duration: Duration,
}

#[derive(Debug, Clone)]
pub struct HttpError {
    pub message: String,
    /// Connection problems and timeouts may succeed when retried.
    pub transient: bool,
    /// The request may have reached the server: its outcome is unknown.
    pub timed_out: bool,
}

pub fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .user_agent(concat!("klausur/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

pub fn send(agent: &ureq::Agent, req: &Prepared, timeout: Duration) -> Result<Received, HttpError> {
    let mut builder = ureq::http::Request::builder()
        .method(req.method.as_str())
        .uri(req.url.as_str());
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let request = builder
        .body(req.body.clone().unwrap_or_default())
        .map_err(|e| HttpError {
            message: format!("invalid request: {e}"),
            transient: false,
            timed_out: false,
        })?;
    let started = Instant::now();
    let result = request
        .with_agent(agent)
        .configure()
        .timeout_global(Some(timeout))
        .allow_non_standard_methods(true)
        .build()
        .run();
    let mut response = result.map_err(|e| {
        let transient = matches!(
            e,
            ureq::Error::Io(_)
                | ureq::Error::Timeout(_)
                | ureq::Error::ConnectionFailed
                | ureq::Error::HostNotFound
        );
        HttpError {
            timed_out: matches!(e, ureq::Error::Timeout(_)),
            message: describe(&e),
            transient,
        }
    })?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = response
        .body_mut()
        .with_config()
        .limit(256 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| HttpError {
            message: format!("failed to read the response body: {}", describe(&e)),
            transient: true,
            timed_out: true,
        })?;
    Ok(Received {
        status,
        headers,
        body,
        duration: started.elapsed(),
    })
}

fn describe(e: &ureq::Error) -> String {
    match e {
        ureq::Error::ConnectionFailed => "connection failed".into(),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::Timeout(_) => "timed out".into(),
        other => other.to_string(),
    }
}

pub fn is_transient_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}
