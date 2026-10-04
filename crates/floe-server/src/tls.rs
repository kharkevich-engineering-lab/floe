//! In-process TLS (D39, D59): `server.tls.mode = "off" | "files" | "acme"`.
//!
//! A reverse proxy may terminate TLS and run h2c to floe (`off`); a standalone host terminates
//! TLS itself with the operator's certificate (`files`) or one it obtains over ACME DNS-01
//! (`acme`). The certificate logic lives in `floe-tls`; this module is the listener and the
//! glue to the server (task narration, readiness).
//!
//! The listener performs the handshake lazily, on the connection's first read/write, so one
//! slow client never serializes the accept loop. ALPN offers `h2` and `http/1.1`; hyper's auto
//! builder sniffs the HTTP/2 preface, so both work over the same port. The rustls config
//! resolves its certificate per handshake, so a renewal or a reloaded file is presented to
//! the next connection while established ones continue untouched.

use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::{Accept, TlsAcceptor, server::TlsStream};

use crate::TcpAccept;

pub use floe_tls::Tls;

/// Pseudo-repository the instance-level TLS task is filed under (`tasks` log, metrics).
pub const TASK_SCOPE: &str = "_instance";

/// ACME orders narrated as tasks (D13): `tls-acme` under [`TASK_SCOPE`], with a log line per
/// step, so a first boot that waits on the CA is never silent.
pub struct TaskNarrator(pub Arc<floe_wal::tasks::Tasks>);

struct TaskNarration(parking_lot::Mutex<Option<floe_wal::tasks::TaskHandle>>);

impl floe_tls::Narrator for TaskNarrator {
    fn begin(&self, kind: &str, summary: &str) -> Box<dyn floe_tls::Narration> {
        let mut params = std::collections::HashMap::new();
        params.insert("summary".to_string(), summary.to_string());
        let handle = match self.0.begin(TASK_SCOPE, kind, params, None) {
            floe_wal::tasks::Begin::Started(h) => {
                h.notice(summary);
                Some(h)
            }
            floe_wal::tasks::Begin::AlreadyRunning(_) => None,
        };
        Box::new(TaskNarration(parking_lot::Mutex::new(handle)))
    }
}

impl floe_tls::Narration for TaskNarration {
    fn notice(&self, text: &str) {
        if let Some(h) = self.0.lock().as_ref() {
            h.notice(text.to_string());
        }
        tracing::info!(task = "tls-acme", "{text}");
    }
    fn finish(self: Box<Self>, result: Result<String, String>) {
        if let Some(h) = self.0.lock().take() {
            match result {
                Ok(s) => {
                    let _record = h.finish_ok(s, None);
                }
                Err(e) => {
                    let _record = h.finish_err(502, e);
                }
            }
        }
    }
}

/// `axum::serve::Listener` that wraps every accepted TCP connection in a
/// lazily-handshaking TLS stream (`TCP_NODELAY` set, like the plain listener).
pub struct TlsListener {
    pub(crate) tcp: TcpAccept,
    pub acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = LazyTls;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.tcp.accept().await {
                Ok((stream, addr)) => {
                    if let Err(e) = stream.set_nodelay(true) {
                        tracing::debug!(error = ?e, %addr, "failed to set TCP_NODELAY");
                    }
                    return (LazyTls::Handshaking(self.acceptor.accept(stream)), addr);
                }
                Err(e) => tracing::warn!(error = ?e, "TCP accept failed"),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// A TLS connection whose handshake completes on first use.
pub enum LazyTls {
    Handshaking(Accept<TcpStream>),
    Ready(TlsStream<TcpStream>),
    Failed,
}

impl LazyTls {
    /// Drive the handshake; `Ready(Ok(stream))` once established.
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<&mut TlsStream<TcpStream>>> {
        loop {
            match self {
                LazyTls::Ready(s) => return Poll::Ready(Ok(s)),
                LazyTls::Failed => {
                    return Poll::Ready(Err(io::Error::other("TLS handshake failed")));
                }
                LazyTls::Handshaking(accept) => match Pin::new(accept).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(s)) => *self = LazyTls::Ready(s),
                    Poll::Ready(Err(e)) => {
                        tracing::debug!(error = %e, "TLS handshake failed");
                        *self = LazyTls::Failed;
                    }
                },
            }
        }
    }
}

impl AsyncRead for LazyTls {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.poll_ready(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(s)) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for LazyTls {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.poll_ready(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(s)) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            LazyTls::Ready(s) => Pin::new(s).poll_flush(cx),
            _ => Poll::Ready(Ok(())),
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            LazyTls::Ready(s) => Pin::new(s).poll_shutdown(cx),
            _ => Poll::Ready(Ok(())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_builds_no_listener_state() {
        let cfg = floe_config::Config::default();
        let store: floe_store::DynStore = floe_store::memory::MemoryStore::shared();
        assert!(Tls::load(&cfg, store).unwrap().is_none());
    }

    #[test]
    fn acme_without_its_secrets_fails_closed_at_startup() {
        let mut cfg = floe_config::Config::default();
        cfg.server.tls.mode = floe_config::TlsMode::Acme;
        cfg.server.tls.acme.domains = vec!["git.example.com".into()];
        cfg.server.tls.acme.email = "ops@example.com".into();
        cfg.server.tls.acme.storage_key_env = "FLOE_TEST_TLS_KEY_THAT_IS_NOT_SET".into();
        let store: floe_store::DynStore = floe_store::memory::MemoryStore::shared();
        let e = Tls::load(&cfg, store).unwrap_err().to_string();
        assert!(e.contains("FLOE_TEST_TLS_KEY_THAT_IS_NOT_SET"), "{e}");
    }
}
