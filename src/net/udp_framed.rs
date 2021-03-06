//! A custom version of tokio::net::UdpFramed that does not exit on send error and
//! which contains a reference to a `State` object
use std::net::{SocketAddr, Ipv4Addr, SocketAddrV4};

use futures::{Async, Poll, Stream, Sink, StartSend, AsyncSink};

use tokio::net::UdpSocket;

use tokio_io::codec::{Decoder, Encoder};
use bytes::{BytesMut, BufMut};

use std::sync::Arc;
use node::state::State;
use utils::to_ipv6;

/// A unified `Stream` and `Sink` interface to an underlying `UdpSocket`, using
/// the `Encoder` and `Decoder` traits to encode and decode frames.
///
/// Raw UDP sockets work with datagrams, but higher-level code usually wants to
/// batch these into meaningful chunks, called "frames". This method layers
/// framing on top of this socket by using the `Encoder` and `Decoder` traits to
/// handle encoding and decoding of messages frames. Note that the incoming and
/// outgoing frame types may be distinct.
///
/// This function returns a *single* object that is both `Stream` and `Sink`;
/// grouping this into a single object is often useful for layering things which
/// require both read and write access to the underlying object.
///
/// If you want to work more directly with the streams and sink, consider
/// calling `split` on the `UdpFramed` returned by this method, which will break
/// them into separate objects, allowing them to interact more easily.
#[must_use = "sinks do nothing unless polled"]
#[derive(Debug)]
pub struct UdpFramed<C> {
    socket: UdpSocket,
    codec: C,
    rd: BytesMut,
    wr: BytesMut,
    out_addr: SocketAddr,
    flushed: bool,
    node_state: Arc<State>,
}

impl<C: Decoder> Stream for UdpFramed<C> {
    type Item = (C::Item, SocketAddr);
    type Error = C::Error;

    fn poll(&mut self) -> Poll<Option<(Self::Item)>, Self::Error> {
        self.rd.reserve(INITIAL_RD_CAPACITY);

        let (n, addr) = unsafe {
            // Read into the buffer without having to initialize the memory.
            let (n, addr) = try_ready!(self.socket.poll_recv_from(self.rd.bytes_mut()));
            self.rd.advance_mut(n);
            (n, addr)
        };
        trace!("received {} bytes, decoding", n);
        let frame_res = self.codec.decode(&mut self.rd);
        self.rd.clear();
        let frame = frame_res?;
        let result = frame.map(|frame| (frame, addr)); // frame -> (frame, addr)
        trace!("frame decoded from buffer");
        Ok(Async::Ready(result))
    }
}

impl<C: Encoder> Sink for UdpFramed<C> {
    type SinkItem = (C::Item, SocketAddr);
    type SinkError = C::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        trace!("sending frame");

        if !self.flushed {
            match try!(self.poll_complete()) {
                Async::Ready(()) => {},
                Async::NotReady => return Ok(AsyncSink::NotReady(item)),
            }
        }

        let (frame, out_addr) = item;
        self.codec.encode(frame, &mut self.wr)?;
        self.out_addr = out_addr;
        self.flushed = false;
        trace!("frame encoded; length={}", self.wr.len());

        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), C::Error> {
        if self.flushed {
            return Ok(Async::Ready(()))
        }

        trace!("flushing frame; length={}", self.wr.len());
        match self.socket.poll_send_to(&self.wr, &self.out_addr) {
            Ok(Async::NotReady) => {
                return Ok(Async::NotReady);
            },
            Ok(Async::Ready(n)) => {
                trace!("written {}", n);

                let wrote_all = n == self.wr.len();
                self.wr.clear();
                self.flushed = true;

                if !wrote_all {
                    debug!("Failed to write entire datagram to socket; Wrote: {} expected: {}", n, self.wr.len());
                }
            },
            Err(e) => {
                if e.kind() == ::std::io::ErrorKind::WouldBlock {
                    return Ok(Async::NotReady);
                }
                debug!("Error sending frame: {:?}, removing peer: {}", e, self.out_addr);
                self.node_state.remove_peer(to_ipv6(self.out_addr));
            }
        }
        Ok(Async::Ready(()))
    }

    fn close(&mut self) -> Poll<(), C::Error> {
        try_ready!(self.poll_complete());
        Ok(().into())
    }
}

const INITIAL_RD_CAPACITY: usize = 64 * 1024;
const INITIAL_WR_CAPACITY: usize = 8 * 1024;

impl<C> UdpFramed<C> {
    /// Create a new `UdpFramed` backed by the given socket and codec.
    ///
    /// See struct level documention for more details.
    pub fn new(socket: UdpSocket, codec: C, state: Arc<State>) -> UdpFramed<C> {
        UdpFramed {
            socket: socket,
            codec: codec,
            out_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)),
            rd: BytesMut::with_capacity(INITIAL_RD_CAPACITY),
            wr: BytesMut::with_capacity(INITIAL_WR_CAPACITY),
            flushed: true,
            node_state: state,
        }
    }

    /// Returns a reference to the underlying I/O stream wrapped by `Framed`.
    ///
    /// # Note
    ///
    /// Care should be taken to not tamper with the underlying stream of data
    /// coming in as it may corrupt the stream of frames otherwise being worked
    /// with.
    #[allow(dead_code)]
    pub fn get_ref(&self) -> &UdpSocket {
        &self.socket
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `Framed`.
    ///
    /// # Note
    ///
    /// Care should be taken to not tamper with the underlying stream of data
    /// coming in as it may corrupt the stream of frames otherwise being worked
    /// with.
    #[allow(dead_code)]
    pub fn get_mut(&mut self) -> &mut UdpSocket {
        &mut self.socket
    }

    /// Consumes the `Framed`, returning its underlying I/O stream.
    #[allow(dead_code)]
    pub fn into_inner(self) -> UdpSocket {
        self.socket
    }
}