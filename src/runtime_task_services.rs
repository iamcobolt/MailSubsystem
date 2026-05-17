//! Shared runtime startup and task ownership helpers.

use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

use anyhow::Context;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::api::{self, ApiState};
use crate::db;

pub async fn load_database(runtime_name: &'static str) -> anyhow::Result<Arc<db::Database>> {
    let db_config = db::DatabaseConfig::load()
        .with_context(|| format!("Load database config for {}", runtime_name))?;
    let database = db::Database::new(&db_config.connection_string())
        .await
        .with_context(|| format!("Connect to database for {}", runtime_name))?;
    Ok(Arc::new(database))
}

pub fn api_state(database: Arc<db::Database>, account_id: impl Into<String>) -> Arc<ApiState> {
    Arc::new(ApiState::new(database, account_id))
}

pub async fn api_state_from_database(
    account_id: impl Into<String>,
    runtime_name: &'static str,
) -> anyhow::Result<Arc<ApiState>> {
    let database = load_database(runtime_name).await?;
    Ok(api_state(database, account_id))
}

pub async fn bind_api_server(
    bind_addr: impl Into<String>,
    state: Arc<ApiState>,
    context: &'static str,
) -> anyhow::Result<BoundApiServer> {
    let bind_addr = bind_addr.into();
    let listener = api::bind_listener(&bind_addr)
        .await
        .with_context(|| format!("Bind {} at {}", context, bind_addr))?;
    Ok(BoundApiServer {
        bind_addr,
        listener,
        state,
    })
}

pub struct BoundApiServer {
    bind_addr: String,
    listener: TcpListener,
    state: Arc<ApiState>,
}

impl BoundApiServer {
    pub fn bind_addr(&self) -> &str {
        &self.bind_addr
    }

    pub async fn serve(self) -> anyhow::Result<()> {
        api::serve_with_listener(self.listener, self.state).await
    }

    pub fn spawn(self, task_name: &'static str) -> RuntimeTask {
        RuntimeTask::spawn(task_name, async move { self.serve().await })
    }
}

pub struct RuntimeTask {
    name: &'static str,
    handle: JoinHandle<anyhow::Result<()>>,
}

impl RuntimeTask {
    pub fn spawn<F>(name: &'static str, future: F) -> Self
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        Self {
            name,
            handle: tokio::spawn(future),
        }
    }
}

pub struct RuntimeTaskSet {
    tasks: Vec<RuntimeTask>,
}

impl RuntimeTaskSet {
    pub fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    pub fn push(&mut self, task: RuntimeTask) {
        self.tasks.push(task);
    }

    pub async fn wait_for_any(&mut self) -> Option<RuntimeTaskResult> {
        if self.tasks.is_empty() {
            return None;
        }

        poll_fn(|cx| {
            let mut index = 0;
            while index < self.tasks.len() {
                match Pin::new(&mut self.tasks[index].handle).poll(cx) {
                    Poll::Ready(result) => {
                        let task = self.tasks.swap_remove(index);
                        return Poll::Ready(Some(RuntimeTaskResult {
                            name: task.name,
                            result,
                        }));
                    }
                    Poll::Pending => index += 1,
                }
            }
            Poll::Pending
        })
        .await
    }

    pub async fn abort_all_and_wait(&mut self) {
        for task in &self.tasks {
            task.handle.abort();
        }
        while let Some(task) = self.tasks.pop() {
            let _ = task.handle.await;
        }
    }
}

pub struct RuntimeTaskResult {
    name: &'static str,
    result: Result<anyhow::Result<()>, tokio::task::JoinError>,
}

impl RuntimeTaskResult {
    pub fn into_result(self) -> anyhow::Result<()> {
        match self.result {
            Ok(result) => result.with_context(|| format!("{} failed", self.name)),
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(anyhow::anyhow!("{} task join failed: {}", self.name, error)),
        }
    }
}

#[cfg(unix)]
pub struct ShutdownSignals {
    sigint: tokio::signal::unix::Signal,
    sigterm: tokio::signal::unix::Signal,
}

#[cfg(not(unix))]
pub struct ShutdownSignals;

impl ShutdownSignals {
    #[cfg(unix)]
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            sigint: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("setup SIGINT handler")?,
            sigterm: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("setup SIGTERM handler")?,
        })
    }

    #[cfg(not(unix))]
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self)
    }

    #[cfg(unix)]
    pub async fn recv(&mut self) -> anyhow::Result<&'static str> {
        tokio::select! {
            _ = self.sigint.recv() => Ok("SIGINT"),
            _ = self.sigterm.recv() => Ok("SIGTERM"),
        }
    }

    #[cfg(not(unix))]
    pub async fn recv(&mut self) -> anyhow::Result<&'static str> {
        tokio::signal::ctrl_c().await.context("listen for Ctrl+C")?;
        Ok("SIGINT")
    }
}
