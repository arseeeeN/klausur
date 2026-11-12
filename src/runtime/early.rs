//! Responses that arrive before the request body is sent, like curl handles them.
//!
//! A server may answer before it has read the whole body: Tomcat refuses a too large upload
//! with `413`, then closes the connection. A client that keeps writing gets `broken pipe` and
//! never sees the 413. This transport, wrapped around every connection, checks for input now and
//! then while a body is sent, and when the server has already answered, it stops sending (the
//! rest of the body is dropped) so the response is read as usual. A write that fails is checked
//! the same way before it counts as an error.

use std::fmt;
use ureq::Timeout;
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, NextTimeout, Transport};

// Every write after the first is checked. A check every so many bytes isn't enough: once the
// server stops reading, the next write blocks until the server closes the connection, and the
// reset that ends the write also discards the response that arrived meanwhile (on macOS at least).
// ureq writes at most 128 KB at a time and each check waits a millisecond at most, so a 50 MB
// upload costs about 0.4 s; a body that fits in one write isn't checked at all.

#[derive(Debug, Default)]
pub struct EarlyResponseConnector;

impl<In: Transport> Connector<In> for EarlyResponseConnector {
    type Out = EarlyResponse<In>;

    fn connect(
        &self,
        _details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| EarlyResponse {
            inner,
            sent: 0,
            answered: false,
            unread: false,
        }))
    }
}

pub struct EarlyResponse<T> {
    inner: T,
    /// Bytes written on this connection so far.
    sent: u64,
    /// The server answered before the body was complete: the rest isn't sent.
    answered: bool,
    /// The early response is in the input buffer, but ureq hasn't looked at it yet.
    unread: bool,
}

impl<T: fmt::Debug> fmt::Debug for EarlyResponse<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EarlyResponse")
            .field("inner", &self.inner)
            .field("answered", &self.answered)
            .finish()
    }
}

impl<T: Transport> EarlyResponse<T> {
    /// Has a response arrived already? Waits a millisecond at most.
    fn response_waiting(&mut self) -> bool {
        let probe = NextTimeout {
            after: std::time::Duration::from_millis(1).into(),
            reason: Timeout::SendBody,
            ..NextTimeout::default()
        };
        matches!(self.inner.await_input(probe), Ok(true))
    }
}

impl<T: Transport> Transport for EarlyResponse<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        if self.answered {
            return Ok(());
        }
        if self.sent > 0 && self.response_waiting() {
            self.answered = true;
            self.unread = true;
            return Ok(());
        }
        self.sent += amount as u64;
        match self.inner.transmit_output(amount, timeout) {
            // The server may have answered and closed the connection: read what it said.
            Err(e) if self.response_waiting() => {
                let _ = e;
                self.answered = true;
                self.unread = true;
                Ok(())
            }
            other => other,
        }
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        // ureq only counts input as usable once it consumed some itself, so it asks for more,
        // and reading again can fail on the closed connection. Hand it the response already
        // read. (Outer layers call this, not `maybe_await_input`.)
        if std::mem::take(&mut self.unread) && !self.inner.buffers().input().is_empty() {
            return Ok(true);
        }
        self.inner.await_input(timeout)
    }

    fn is_open(&mut self) -> bool {
        // Part of the body was never sent: the connection can't be reused.
        !self.answered && self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}
