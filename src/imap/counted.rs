//! A byte count on a socket, so a sync phase can say how much it moved.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The two directions of one connection, shared between the stream that
/// counts and whoever reads the count.
#[derive(Clone, Debug, Default)]
pub struct ByteCounter {
    read: Arc<AtomicU64>,
    written: Arc<AtomicU64>,
}

impl ByteCounter {
    /// Bytes received from the server so far.
    pub fn read(&self) -> u64 {
        self.read.load(Ordering::Relaxed)
    }

    /// Bytes sent to the server so far.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Both directions together.
    pub fn total(&self) -> u64 {
        self.read() + self.written()
    }
}

/// A stream that counts what passes through it.
///
/// Sits under TLS, so the count is what went over the wire, handshake and
/// record overhead included: what the network carried, rather than what the
/// IMAP parser was handed.
#[derive(Debug)]
pub struct Counted<S> {
    inner: S,
    counter: ByteCounter,
}

impl<S> Counted<S> {
    /// Wrap `inner`, returning the counter that keeps reporting after the
    /// stream has been handed on.
    pub fn new(inner: S) -> (Self, ByteCounter) {
        let counter = ByteCounter::default();
        let wrapped = Self {
            inner,
            counter: counter.clone(),
        };
        (wrapped, counter)
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = result {
            let received = (buf.filled().len() - before) as u64;
            self.counter.read.fetch_add(received, Ordering::Relaxed);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, data);
        if let Poll::Ready(Ok(sent)) = result {
            self.counter
                .written
                .fetch_add(sent as u64, Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn counts_both_directions() {
        let (client, server) = tokio::io::duplex(64);
        let (mut counted, counter) = Counted::new(client);
        let mut server = server;

        counted.write_all(b"hello").await.unwrap();
        let mut received = [0u8; 5];
        server.read_exact(&mut received).await.unwrap();
        server.write_all(b"world!!").await.unwrap();
        let mut back = [0u8; 7];
        counted.read_exact(&mut back).await.unwrap();

        assert_eq!(counter.written(), 5);
        assert_eq!(counter.read(), 7);
        assert_eq!(counter.total(), 12);
    }
}
