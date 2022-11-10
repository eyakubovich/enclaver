use std::sync::{Arc, Mutex};
use tokio_pipe::{PipeRead};
use circbuf::CircBuf;
use anyhow::Result;
use tokio::io::{AsyncWrite, AsyncReadExt};
use tokio::task::JoinHandle;
use tokio::sync::watch;
use tokio_vsock::VsockStream;
use futures::Stream;
use futures::stream::FuturesUnordered;

use enclaver::enclave_log::SubStreamLabel;

const APP_LOG_CAPACITY: usize = 128*1024;

struct LogCursor {
    pos: usize,
}

impl LogCursor {
    fn new() -> Self {
        Self{
            pos: 0usize,
        }
    }
}

struct ByteLog {
    buffer: CircBuf,
    head: usize,
    watch_tx: watch::Sender<()>,
    watch_rx: watch::Receiver<()>,
}

impl ByteLog {
    fn new() -> Self {
        let (tx, rx) = watch::channel(());

        Self{
            buffer: CircBuf::with_capacity(APP_LOG_CAPACITY).unwrap(),
            head: 0usize,
            watch_rx: rx,
            watch_tx: tx,
        }
    }

    // returns the number of bytes it trimmed from the head
    fn append(&mut self, data: &[u8]) -> usize {
        use std::io::Write;

        let mut trim_cnt = 0usize;

        let avail = self.buffer.avail();
        if avail < data.len() {
            trim_cnt = data.len() - avail;
            self.buffer.advance_read(trim_cnt);
            self.head += trim_cnt;
        }
        assert!(self.buffer.avail() >= data.len());

        assert!(self.buffer.write(data).unwrap() == data.len());

        // notify the watchers that an append happened
        _ = self.watch_tx.send(());

        trim_cnt
    }

    fn read(&self, cursor: &mut LogCursor, mut buf: &mut [u8]) -> usize {
        let mut copied = 0usize;

        let mut offset = if cursor.pos < self.head {
            cursor.pos = self.head;
            0usize
        } else {
            cursor.pos - self.head
        };

        for mut data in self.buffer.get_bytes_upto_size(buf.len() + offset) {
            if offset < data.len() {
                data = &data[offset..];
                offset = 0;

                buf[..data.len()].copy_from_slice(data);
                buf = &mut buf[data.len()..];

                copied += data.len();
            } else {
                offset -= data.len();
            }
        }

        cursor.pos += copied;

        copied
    }

    fn watch(&mut self) -> watch::Receiver<()> {
        self.watch_rx.clone()
    }

    #[allow(dead_code)]
    fn cap(&self) -> usize {
        self.buffer.cap()
    }

    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.buffer.len()
    }
}

struct SubStream {
    log: Arc<Mutex<ByteLog>>,
    label: SubStreamLabel,
    feed_task: JoinHandle<()>,
}

impl SubStream {
    fn new(r_pipe: PipeRead, label: SubStreamLabel) -> Self {
        let log = Arc::new(Mutex::new(ByteLog::new()));

        let log2 = log.clone();
        let feed_task = tokio::task::spawn(async move {
            _ = SubStream::feed_from(r_pipe, log2).await;
        });

        Self{ log, label, feed_task }
    }

    fn reader(&self) -> LogReader {
        LogReader{
            log: self.log.clone(),
            label: self.label,
            cursor: LogCursor::new(),
        }
    }

    async fn feed_from(mut r_pipe: PipeRead, log: Arc<Mutex<ByteLog>>) -> Result<()> {
        let mut buf = vec![0u8; 16*1024];
        loop {
            let n = r_pipe.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }

            log.lock().unwrap().append(&buf[..n]);
        }
    }

    async fn stop(self) {
        self.feed_task.abort();
        _ = self.feed_task.await;
    }
}

struct LogReader {
    log: Arc<Mutex<ByteLog>>,
    label: SubStreamLabel,
    cursor: LogCursor,
}

impl LogReader {
    fn read(&mut self, buf: &mut [u8]) -> usize {
       self.log.lock().unwrap().read(&mut self.cursor, buf)
    }

    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    async fn stream<W: AsyncWrite + Unpin>(&mut self, writer: &mut W) -> Result<()> {
        let mut buf = vec![0u8; 4096];
        loop {
            let nread = self.read(&mut buf);
            if nread == 0 {
                break;
            }

            enclaver::enclave_log::write(writer, self.label, &buf[..nread]).await?;
        }

        Ok(())
    }

    fn watch(&self) -> watch::Receiver<()> {
        self.log.lock().unwrap().watch()
    }

    async fn changed<T>(mut rx: watch::Receiver<()>, cookie: T) -> (watch::Receiver<()>, T) {
        rx.changed().await.unwrap();
        (rx, cookie)
    }
}

pub struct AppLog {
    substreams: Vec<SubStream>,
}

impl AppLog {
    pub fn new() -> Self {
        Self{
            substreams: Vec::new(),
        }
    }

    pub fn add_substream(&mut self, r_pipe: PipeRead, label: SubStreamLabel) {
        self.substreams.push(SubStream::new(r_pipe, label));
    }

    // serve the log over vsock
    async fn serve_log(self, incoming: impl Stream<Item=VsockStream>) -> Result<()> {
        use futures::stream::StreamExt;

        let mut incoming = Box::pin(incoming);
        while let Some(sock) = incoming.next().await {
            let readers = self.substreams.iter()
                .map(SubStream::reader)
                .collect();

            tokio::task::spawn(async move {
                serve_conn(sock, readers).await
            });
        }

        Ok(())
    }

    // launch a task to service the pipe and serve the log over vsock
    pub fn start_serving(self, port: u32) -> JoinHandle<Result<()>> {
        match enclaver::vsock::serve(port) {
            Ok(incoming) => {
                tokio::task::spawn(async move {
                    self.serve_log(incoming).await?;
                    Ok(())
                })
            },
            Err(e) => tokio::task::spawn(async move { Err(e) }),
        }
    }

    pub async fn stop(self) {
        for ss in self.substreams {
            ss.stop().await;
        }
    }
}

async fn serve_conn(mut sock: VsockStream, mut readers: Vec<LogReader>) {
    let mut changed: FuturesUnordered<_> =
        readers.iter_mut()
            .map(|r| r.watch())
            .enumerate()
            .map(|(i, rx)| LogReader::changed(rx, i))
            .collect();

    use futures::stream::StreamExt;

    while let Some((rx, i)) = changed.next().await {
        if let Err(_) = readers[i].stream(&mut sock).await {
            break;
        }
        changed.push(LogReader::changed(rx, i));
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

    use super::{ByteLog, LogCursor};
    use crate::launcher::ExitStatus;

    fn check_log(log: &ByteLog, mut expected: u8) {
        // check that the log contents monotonically increase
        let mut c = LogCursor::new();

        let mut buf = vec![0u8; 1024];

        loop {
            match log.read(&mut c, &mut buf) {
                0 => break,
                nread =>  {
                    for actual in &buf[..nread] {
                        assert!(*actual == expected);
                        expected = expected.wrapping_add(1u8);
                    }
                }
            }
        }
    }

    fn iota_u8(slice: &mut [u8], mut start: u8) -> u8 {
        for x in slice {
            *x = start;
            start = start.wrapping_add(1u8);
        }

        start
    }

    #[test]
    fn test_byte_log() {
        let mut log = ByteLog::new();

        // append by a bit upto the log capacity
        let mut logged = 0usize;
        let mut quanta = 5;
        let mut i = 0u8;
        while logged < log.cap() - quanta {
            let mut data = vec![0u8; quanta];
            i = iota_u8(&mut data, i);

            assert!(log.append(&data) == 0usize);
            quanta += 1;
            logged += data.len();

            check_log(&log, 0);
        }

        let mut expected = 0u8;

        // overflow the log so it starts to trim
        while logged < log.cap() * 3 {
            let mut data = vec![0u8; quanta];
            i = iota_u8(&mut data, i);

            let trimmed = log.append(&data);
            assert!(trimmed > 0);
            quanta += 1;

            expected = expected.wrapping_add(trimmed as u8);
            check_log(&log, expected);

            logged += data.len();
        }
    }

    #[tokio::test]
    async fn test_app_log() {
        use rand::RngCore;
        use std::time::Duration;

        let (mut w, mut s, r) = super::new_app_log().unwrap();

        let runner = tokio::spawn(async move {
            s.run().await.unwrap();
        });

        let mut expected = vec![0u8; super::APP_LOG_CAPACITY*3];
        rand::thread_rng().fill_bytes(&mut expected);

        // write all in small chunks
        for chunk in expected.chunks(53) {
            w.write(chunk).await.unwrap();
        }

        tokio::time::sleep(Duration::from_secs(1)).await;

        runner.abort();
        assert!(runner.await.unwrap_err().is_cancelled());

        // read the tail
        let mut c = LogCursor::new();
        let mut actual: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 1024];
        loop {
            let nread = r.read(&mut c, &mut buf);
            if nread == 0 {
                break;
            }

            actual.extend_from_slice(&mut buf[..nread]);
        }

        assert!(actual.len() == r.len());

        let tail_pos = expected.len() - actual.len();
        assert!(actual == expected[tail_pos..]);
    }
}
