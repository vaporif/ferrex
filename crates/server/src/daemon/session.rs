use std::sync::Arc;
use std::time::Duration;

use ferrex_core::MemoryService;
use rmcp::ServiceExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, watch};

use crate::daemon::handshake::{Hello, HelloReply, RefusalKind};
use crate::daemon::idle::ConnEvent;
use crate::server::{FerrexServer, SessionDefaults};

const HELLO_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HELLO_BYTES: u64 = 64 * 1024;

#[derive(Clone)]
pub enum InitView {
    Pending,
    Ready(Arc<MemoryService>),
    Failed(Arc<str>),
}

type InitOutcome = Result<Arc<MemoryService>, Arc<str>>;

impl InitView {
    fn outcome(&self) -> Option<InitOutcome> {
        match self {
            Self::Pending => None,
            Self::Ready(service) => Some(Ok(Arc::clone(service))),
            Self::Failed(error) => Some(Err(Arc::clone(error))),
        }
    }
}

#[derive(Clone)]
pub struct SessionCtx {
    pub key: Arc<str>,
    pub init: watch::Receiver<InitView>,
    pub events: mpsc::UnboundedSender<ConnEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Hello,
    Active,
}

/// Reports the connection's last phase on drop, so a panic or abort still updates the idle counts.
struct PhaseGuard {
    events: mpsc::UnboundedSender<ConnEvent>,
    phase: Phase,
}

impl PhaseGuard {
    const fn new(events: mpsc::UnboundedSender<ConnEvent>) -> Self {
        Self {
            events,
            phase: Phase::Hello,
        }
    }

    fn activate(&mut self) {
        self.phase = Phase::Active;
        let _ = self.events.send(ConnEvent::HelloPassed);
    }
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let event = match self.phase {
            Phase::Hello => ConnEvent::HelloFailed,
            Phase::Active => ConnEvent::Ended,
        };
        let _ = self.events.send(event);
    }
}

pub async fn serve_connection(stream: UnixStream, mut ctx: SessionCtx) {
    let mut guard = PhaseGuard::new(ctx.events.clone());
    let (read, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read);

    let Some(line) = read_hello_line(&mut reader).await else {
        return;
    };
    let Some(init) = ctx
        .init
        .wait_for(|view| !matches!(view, InitView::Pending))
        .await
        .ok()
        .and_then(|view| view.outcome())
    else {
        return;
    };
    let (service, namespace) = match accept_hello(&line, &ctx, init).await {
        Ok(accepted) => accepted,
        Err(reply) => {
            tracing::info!(kind = ?reply.kind, error = reply.error.as_deref().unwrap_or(""), "refused hello");
            send_reply(&mut writer, &reply).await;
            return;
        }
    };
    if !send_reply(&mut writer, &HelloReply::ok()).await {
        return;
    }
    guard.activate();

    let server = FerrexServer::with_session(service, SessionDefaults { namespace });
    match server.serve((reader, writer)).await {
        Ok(running) => {
            if let Err(e) = running.waiting().await {
                tracing::warn!(error = %e, "MCP session task failed");
            }
        }
        Err(e) => tracing::debug!(error = %e, "MCP session ended during initialize"),
    }
}

async fn read_hello_line(
    reader: &mut BufReader<impl tokio::io::AsyncRead + Unpin>,
) -> Option<String> {
    let mut line = String::new();
    let mut limited = reader.take(MAX_HELLO_BYTES);
    match tokio::time::timeout(HELLO_READ_TIMEOUT, limited.read_line(&mut line)).await {
        Err(_) => {
            tracing::debug!("no hello within {HELLO_READ_TIMEOUT:?}, closing connection");
            None
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "hello read failed");
            None
        }
        Ok(Ok(0)) => None,
        Ok(Ok(_)) => Some(line),
    }
}

async fn accept_hello(
    line: &str,
    ctx: &SessionCtx,
    init: InitOutcome,
) -> Result<(Arc<MemoryService>, String), HelloReply> {
    let service = init.map_err(|error| HelloReply::refuse(RefusalKind::Init, &*error))?;
    let hello = Hello::decode(line)?;
    hello.check(&ctx.key)?;
    service
        .vector_store()
        .ensure_collection(&hello.namespace)
        .await
        .map_err(|e| {
            HelloReply::refuse(
                RefusalKind::Session,
                format!("namespace {:?}: {e}", hello.namespace),
            )
        })?;
    Ok((service, hello.namespace))
}

async fn send_reply(writer: &mut OwnedWriteHalf, reply: &HelloReply) -> bool {
    writer
        .write_all(reply.encode().as_bytes())
        .await
        .inspect_err(|e| tracing::debug!(error = %e, "hello reply write failed"))
        .is_ok()
}
