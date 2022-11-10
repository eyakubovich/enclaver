use std::sync::{Arc, Mutex};
use anyhow::Result;
use tokio::io::{AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio::sync::watch;
use tokio_vsock::VsockStream;

use crate::launcher::ExitStatus;

enum EntrypointStatus {
    Running,
    Exited(ExitStatus),
    Fatal(String),
}

impl EntrypointStatus {
    fn as_json(&self) -> String {
        match self {
            Self::Running => "{ \"status\": \"running\" }\n".to_string(),
            Self::Exited(exit_status) => {
                match exit_status {
                    ExitStatus::Exited(code) => format!("{{ \"status\": \"exited\", \"code\": {code} }}\n"),
                    ExitStatus::Signaled(sig) => format!("{{ \"status\": \"signaled\", \"signal\": \"{sig}\" }}\n"),
                }
            },
            Self::Fatal(err) => format!("{{ \"status\": \"fatal\", \"error\": \"{err}\" }}\n"),
        }
    }
}

struct AppStatusInner {
    status: EntrypointStatus,
    watch_tx: watch::Sender<()>,
    watch_rx: watch::Receiver<()>,
}

impl AppStatusInner {
    fn new() -> Self {
        let (tx, rx) = watch::channel(());

        Self{
            status: EntrypointStatus::Running,
            watch_tx: tx,
            watch_rx: rx,
        }
    }

    fn exited(&mut self, status: ExitStatus) {
        self.status = EntrypointStatus::Exited(status);
        self.watch_tx.send(()).unwrap();
    }

    fn fatal(&mut self, err: String) {
        self.status = EntrypointStatus::Fatal(err);
        self.watch_tx.send(()).unwrap();
    }
}

#[derive(Clone)]
pub struct AppStatus {
    inner: Arc<Mutex<AppStatusInner>>,
}

impl AppStatus {
    pub fn new() -> Self {
        Self{
            inner: Arc::new(Mutex::new(AppStatusInner::new())),
        }
    }

    pub fn exited(&self, status: ExitStatus) {
        self.inner.lock().unwrap().exited(status);
    }

    pub fn fatal(&self, err: String) {
        self.inner.lock().unwrap().fatal(err);
    }

    pub fn start_serving(&self, port: u32) -> JoinHandle<Result<()>> {
        use futures::stream::StreamExt;

        match enclaver::vsock::serve(port) {
            Ok(incoming) => {
                let mut incoming = Box::pin(incoming);
                let app_status = self.clone();
                tokio::task::spawn(async move {
                    while let Some(sock) = incoming.next().await {
                        let app_status = app_status.clone();
                        tokio::task::spawn(async move {
                            app_status.stream(sock).await;
                        });
                    }
                    Ok(())
                })
            },
            Err(e) => tokio::task::spawn(async move { Err(e) })
        }
    }

    async fn stream(&self, mut sock: VsockStream) {
        let mut w = self.inner.lock().unwrap().watch_rx.clone();

        loop {
            let json_str = self.inner.lock().unwrap().status.as_json();
            _ = sock.write_all(json_str.as_bytes()).await;

            // wait for new data
            // unwrap() since the sender never closes first
            w.changed().await.unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::assert;
    use json::{object, JsonValue};
    use nix::sys::signal::Signal;
    use tokio::io::{BufReader, Lines, AsyncBufRead, AsyncBufReadExt};
    use tokio_vsock::VsockStream;
    use anyhow::{Result, anyhow};
    use enclaver::constants::STATUS_PORT;

    use crate::launcher::ExitStatus;

    async fn read_json<R: AsyncBufRead + Unpin>(lines: &mut Lines<R>) -> Result<JsonValue> {
        let line = lines.next_line()
            .await?
            .ok_or(anyhow!("unexpected EOF"))?;

        Ok(json::parse(&line)?)
    }

    async fn app_status_lines() -> Result<Lines<impl AsyncBufRead + Unpin>> {
        let sock = VsockStream::connect(enclaver::vsock::VMADDR_CID_HOST, STATUS_PORT).await?;
        // bug in VsockStream::connect: it can return Ok even if connect failed
        _ = sock.peer_addr()?;
        Ok(BufReader::new(sock).lines())
    }

    #[tokio::test]
    async fn test_app_status() {
        let app_status = super::AppStatus::new();
        let status_task = app_status.start_serving(STATUS_PORT);

        let mut client1 = app_status_lines().await.unwrap();
        let mut client2 = app_status_lines().await.unwrap();

        // Running
        let mut expected = object!{ status: "running" };

        let mut status = read_json(&mut client1).await.unwrap();

        assert!(status == expected);

        status = read_json(&mut client2).await.unwrap();
        assert!(status == expected);

        // Exited
        app_status.exited(ExitStatus::Exited(2));
        expected = object!{ status: "exited", code: 2 };

        status = read_json(&mut client1).await.unwrap();
        assert!(status == expected);

        status = read_json(&mut client2).await.unwrap();
        assert!(status == expected);

        // Signaled
        app_status.exited(ExitStatus::Signaled(Signal::SIGTERM));
        expected = object!{ status: "signaled", signal: "SIGTERM" };

        status = read_json(&mut client1).await.unwrap();
        assert!(status == expected);

        status = read_json(&mut client2).await.unwrap();
        assert!(status == expected);

        status_task.abort();
        _ = status_task.await;
    }
}
