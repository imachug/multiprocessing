//! Uni- and bidirectional channels between processes.
//!
//! Create and use a unidirectional channel:
//!
//! ```rust
//! # use multiprocessing::{channel, Receiver, Sender};
//! let (mut sender, mut receiver): (Sender<i32>, Receiver<i32>) = channel::<i32>()?;
//! sender.send(&57)?;
//! drop(sender);
//! assert_eq!(receiver.recv()?, Some(57));
//! assert_eq!(receiver.recv()?, None);
//! # std::io::Result::Ok(())
//! ```
//!
//! Create and use a bidirectional channel:
//!
//! ```rust
//! # use multiprocessing::{duplex, Duplex};
//! let (mut side1, mut side2) = duplex::<i32, (i32, i32)>()?;
//! side1.send(&57)?;
//! assert_eq!(side2.recv()?, Some(57));
//! side2.send(&(1, 2))?;
//! assert_eq!(side1.recv()?, Some((1, 2)));
//! drop(side1);
//! assert_eq!(side2.recv()?, None);
//! # std::io::Result::Ok(())
//! ```

use crate::{entry, Deserializer, Object, Serializer};
use nix::libc::{AF_UNIX, SOCK_CLOEXEC, SOCK_SEQPACKET};
use std::io::{Error, ErrorKind, IoSlice, IoSliceMut, Result};
use std::marker::PhantomData;
use std::os::unix::{
    io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
    net::{AncillaryData, SocketAncillary, UnixStream},
};

pub(crate) const MAX_PACKET_SIZE: usize = 16 * 1024;

/// The transmitting side of a unidirectional channel.
///
/// `T` is the type of the objects this side sends via the channel and the other side receives.
#[derive(Object)]
pub struct Sender<T: Object> {
    fd: UnixStream,
    marker: PhantomData<fn(T) -> T>,
}

/// The receiving side of a unidirectional channel.
///
/// `T` is the type of the objects the other side sends via the channel and this side receives.
#[derive(Object)]
pub struct Receiver<T: Object> {
    fd: UnixStream,
    marker: PhantomData<fn(T) -> T>,
}

/// A side of a bidirectional channel.
///
/// `S` is the type of the objects this side sends via the channel and the other side receives, `R`
/// is the type of the objects the other side sends via the channel and this side receives.
#[derive(Object)]
pub struct Duplex<S: Object, R: Object> {
    fd: UnixStream,
    marker: PhantomData<fn(S, R) -> (S, R)>,
}

/// Create a unidirectional channel.
pub fn channel<T: Object>() -> Result<(Sender<T>, Receiver<T>)> {
    let (tx, rx) = duplex::<T, T>()?;
    Ok((tx.into_sender(), rx.into_receiver()))
}

/// Create a bidirectional channel.
pub fn duplex<A: Object, B: Object>() -> Result<(Duplex<A, B>, Duplex<B, A>)> {
    // UnixStream creates a SOCK_STREAM by default, while we need SOCK_SEQPACKET
    unsafe {
        let mut fds = [0, 0];
        if nix::libc::socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, fds.as_mut_ptr()) == -1
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok((Duplex::from_raw_fd(fds[0]), Duplex::from_raw_fd(fds[1])))
    }
}

fn send_on_fd<T: Object>(fd: &mut UnixStream, value: &T) -> Result<()> {
    let mut s = Serializer::new();
    s.serialize(value);

    let fds = s.drain_handles();
    let serialized = s.into_vec();

    let mut ancillary_buffer = [0; 253];

    // Send the data and pass file descriptors
    let mut buffer_pos: usize = 0;
    let mut fds_pos: usize = 0;

    loop {
        let buffer_end = serialized.len().min(buffer_pos + MAX_PACKET_SIZE - 1);
        let fds_end = fds.len().min(fds_pos + 253);

        let is_last = buffer_end == serialized.len() && fds_end == fds.len();

        let mut ancillary = SocketAncillary::new(&mut ancillary_buffer);
        if !ancillary.add_fds(&fds[fds_pos..fds_end]) {
            return Err(Error::new(ErrorKind::Other, "Too many fds to pass"));
        }

        let n_written = fd.send_vectored_with_ancillary(
            &[
                IoSlice::new(&[is_last as u8]),
                IoSlice::new(&serialized[buffer_pos..buffer_end]),
            ],
            &mut ancillary,
        )?;
        buffer_pos += n_written - 1;
        fds_pos = fds_end;

        if is_last {
            break;
        }
    }

    Ok(())
}

fn recv_on_fd<T: Object>(fd: &mut UnixStream) -> Result<Option<T>> {
    // Read the data and the passed file descriptors
    let mut serialized: Vec<u8> = Vec::new();
    let mut buffer_pos: usize = 0;

    let mut ancillary_buffer = [0; 253];
    let mut received_fds: Vec<OwnedFd> = Vec::new();

    loop {
        serialized.resize(buffer_pos + MAX_PACKET_SIZE - 1, 0);

        let mut marker = [0];
        let mut ancillary = SocketAncillary::new(&mut ancillary_buffer[..]);

        let n_read = fd.recv_vectored_with_ancillary(
            &mut [
                IoSliceMut::new(&mut marker),
                IoSliceMut::new(&mut serialized[buffer_pos..]),
            ],
            &mut ancillary,
        )?;

        for cmsg in ancillary.messages() {
            if let Ok(AncillaryData::ScmRights(rights)) = cmsg {
                for fd in rights {
                    received_fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            } else {
                return Err(Error::new(
                    ErrorKind::Other,
                    "Unexpected kind of cmsg on stream",
                ));
            }
        }

        if ancillary.is_empty() && n_read == 0 {
            if buffer_pos == 0 && received_fds.is_empty() {
                return Ok(None);
            } else {
                return Err(Error::new(ErrorKind::Other, "Unterminated data on stream"));
            }
        }

        if n_read == 0 {
            return Err(Error::new(
                ErrorKind::Other,
                "Unexpected empty message on stream",
            ));
        }

        buffer_pos += n_read - 1;
        if marker[0] == 1 {
            break;
        }
    }

    serialized.truncate(buffer_pos);

    let mut d = Deserializer::from(serialized, received_fds);
    Ok(Some(d.deserialize()))
}

impl<T: Object> Sender<T> {
    fn from_unix_stream(fd: UnixStream) -> Self {
        Sender {
            fd,
            marker: PhantomData,
        }
    }

    /// Send a value to the other side.
    pub fn send(&mut self, value: &T) -> Result<()> {
        send_on_fd(&mut self.fd, value)
    }
}

impl<T: Object> AsRawFd for Sender<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl<T: Object> IntoRawFd for Sender<T> {
    fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw_fd()
    }
}

impl<T: Object> FromRawFd for Sender<T> {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        entry::disable_nonblock(fd).expect("Failed to reset O_NONBLOCK");
        Self::from_unix_stream(UnixStream::from_raw_fd(fd))
    }
}

impl<T: Object> Receiver<T> {
    fn from_unix_stream(fd: UnixStream) -> Self {
        Receiver {
            fd,
            marker: PhantomData,
        }
    }

    /// Receive a value from the other side.
    ///
    /// Returns `Ok(None)` if the other side has dropped the channel.
    pub fn recv(&mut self) -> Result<Option<T>> {
        recv_on_fd(&mut self.fd)
    }
}

impl<T: Object> AsRawFd for Receiver<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl<T: Object> IntoRawFd for Receiver<T> {
    fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw_fd()
    }
}

impl<T: Object> FromRawFd for Receiver<T> {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        entry::disable_nonblock(fd).expect("Failed to reset O_NONBLOCK");
        Self::from_unix_stream(UnixStream::from_raw_fd(fd))
    }
}

impl<S: Object, R: Object> Duplex<S, R> {
    fn from_unix_stream(fd: UnixStream) -> Self {
        Duplex {
            fd,
            marker: PhantomData,
        }
    }

    /// Send a value to the other side.
    pub fn send(&mut self, value: &S) -> Result<()> {
        send_on_fd(&mut self.fd, value)
    }

    /// Receive a value from the other side.
    ///
    /// Returns `Ok(None)` if the other side has dropped the channel.
    pub fn recv(&mut self) -> Result<Option<R>> {
        recv_on_fd(&mut self.fd)
    }

    /// Send a value from the other side and wait for a response immediately.
    ///
    /// If the other side closes the channel before responding, an error is returned.
    pub fn request(&mut self, value: &S) -> Result<R> {
        self.send(value)?;
        self.recv()?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "The subprocess exitted before responding to the request",
            )
        })
    }

    pub(crate) fn into_sender(self) -> Sender<S> {
        Sender::from_unix_stream(self.fd)
    }

    pub(crate) fn into_receiver(self) -> Receiver<R> {
        Receiver::from_unix_stream(self.fd)
    }
}

impl<S: Object, R: Object> AsRawFd for Duplex<S, R> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl<S: Object, R: Object> IntoRawFd for Duplex<S, R> {
    fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw_fd()
    }
}

impl<S: Object, R: Object> FromRawFd for Duplex<S, R> {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        entry::disable_nonblock(fd).expect("Failed to reset O_NONBLOCK");
        Self::from_unix_stream(UnixStream::from_raw_fd(fd))
    }
}
