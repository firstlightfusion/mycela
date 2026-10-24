use std::net::SocketAddr;

use ascii_tcp::{AsciiTcpError, ChannelConfig, LineEnding, Server, ServerContext};
use tokio::task::JoinHandle;

/// Start an ASCII TCP line-protocol server that responds to incoming requests.
///
/// The server accepts a request line from each client, passes it to the provided
/// handler, and writes the handler's response back with the default line ending.
///
/// The handler receives the request text and the peer socket address and should
/// return a response string.
pub fn start_server<F, Fut>(addr: SocketAddr, handler: F) -> JoinHandle<()>
where
    F: Fn(String, SocketAddr) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    start_server_with_line_ending(addr, LineEnding::default(), handler)
}

/// Start an ASCII TCP server with an explicit request/response line ending.
pub fn start_server_with_line_ending<F, Fut>(
    addr: SocketAddr,
    line_ending: LineEnding,
    handler: F,
) -> JoinHandle<()>
where
    F: Fn(String, SocketAddr) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    let ctx = ServerContext {
        bind_addr: addr,
        line_ending,
        ..ServerContext::default()
    };
    spawn_server(ctx, handler)
}

/// Start an ASCII TCP server using the widget's transport config.
pub fn start_from_config<F, Fut>(config: &ChannelConfig, handler: F) -> JoinHandle<()>
where
    F: Fn(String, SocketAddr) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    let ctx = ServerContext {
        bind_addr: SocketAddr::from(([0, 0, 0, 0], config.port)),
        line_ending: config.line_ending,
        io_timeout: std::time::Duration::from_millis(config.io_timeout_ms),
        ..ServerContext::default()
    };
    spawn_server(ctx, handler)
}

/// Runs `ctx` with `handler` on a task; aborting the returned handle drops the server, which shuts it down.
///
/// A handler error is answered with an `ERROR <message>` line rather than ending the
/// connection, which clients share across all of their channels.
fn spawn_server<F, Fut>(ctx: ServerContext, handler: F) -> JoinHandle<()>
where
    F: Fn(String, SocketAddr) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    let ctx = ctx.with_handler(move |request, peer| {
        let response = handler(request, peer);
        async move {
            Ok::<_, AsciiTcpError>(response.await.unwrap_or_else(|err| format!("ERROR {err}")))
        }
    });

    tokio::spawn(async move {
        let server = match Server::start(ctx).await {
            Ok(server) => server,
            Err(err) => {
                tracing::error!("[ascii-tcp-server] failed to start: {err}");
                return;
            }
        };

        std::future::pending::<()>().await;
        drop(server);
    })
}
