//! Non-blocking calloop transport, independent of compositor state for socket tests.
use std::{
    cell::RefCell,
    collections::VecDeque,
    io::{self, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    rc::Rc,
    sync::{Arc, Weak},
};

use smithay::reexports::calloop::{generic::Generic, Interest, LoopHandle, Mode, PostAction};

pub struct Listener {
    // This transport is also compiled alone by the socket integration tests.
    #[allow(dead_code)]
    pub token: smithay::reexports::calloop::RegistrationToken,
    streams: Rc<RefCell<Vec<Weak<UnixStream>>>>,
}
impl Drop for Listener {
    fn drop(&mut self) {
        for stream in self.streams.borrow().iter().filter_map(Weak::upgrade) { let _ = stream.shutdown(std::net::Shutdown::Both); }
    }
}

const MAX_LINE: usize = 1024 * 1024;
const MAX_PENDING: usize = 8 * MAX_LINE;
const READ_BUDGET: usize = 64 * 1024;

#[derive(Default)]
struct Outbox {
    bytes: VecDeque<u8>,
    writer_registered: bool,
}

fn flush(stream: &UnixStream, outbox: &mut Outbox) -> io::Result<()> {
    let mut stream = stream;
    while !outbox.bytes.is_empty() {
        let (first, _) = outbox.bytes.as_slices();
        match stream.write(first) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                outbox.bytes.drain(..n);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// Synchronous adapters are also used by the standalone socket tests.
#[allow(dead_code)]
pub fn listen<D: 'static>(
    handle: LoopHandle<'static, D>,
    listener: UnixListener,
    dispatch: impl Fn(&[u8], &mut D) -> Vec<u8> + Clone + 'static,
) -> io::Result<Listener> {
    listen_async(handle, listener, move |line, data, reply| reply.send(dispatch(line, data)))
}

#[allow(dead_code)]
pub fn listen_async<D: 'static>(
    handle: LoopHandle<'static, D>,
    listener: UnixListener,
    dispatch: impl Fn(&[u8], &mut D, Reply<D>) + Clone + 'static,
) -> io::Result<Listener> {
    listener.set_nonblocking(true)?;
    let clients = handle.clone();
    let streams: Rc<RefCell<Vec<Weak<UnixStream>>>> = Rc::new(RefCell::new(Vec::new()));
    let accepted = streams.clone();
    let token = handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, _| {
                // Bound the work per event so a connection flood cannot starve rendering.
                for _ in 0..32 {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let lifetime = stream.try_clone().ok().map(Arc::new);
                            accepted.borrow_mut().retain(|s| s.strong_count() > 0);
                            if let Some(copy) = &lifetime { accepted.borrow_mut().push(Arc::downgrade(copy)); }
                            let registered = if lifetime.is_some() { add_client_inner(clients.clone(), stream, dispatch.clone(), lifetime) }
                                else { add_client_inner(clients.clone(), stream, dispatch.clone(), None) };
                            if let Err(err) = registered
                            {
                                tracing::warn!(?err, "failed to register IPC client");
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(err) => {
                            tracing::warn!(?err, "IPC accept failed");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(Listener { token, streams })
}

#[allow(dead_code)]
pub fn add_client<D: 'static>(
    handle: LoopHandle<'static, D>,
    stream: UnixStream,
    dispatch: impl Fn(&[u8], &mut D) -> Vec<u8> + 'static,
) -> io::Result<()> {
    add_client_inner(handle, stream, move |line, data, reply| reply.send(dispatch(line, data)), None)
}

fn add_client_inner<D: 'static>(
    handle: LoopHandle<'static, D>,
    stream: UnixStream,
    dispatch: impl Fn(&[u8], &mut D, Reply<D>) + 'static,
    lifetime: Option<Arc<UnixStream>>,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let mut input = Vec::new();
    let outbox = Rc::new(RefCell::new(Outbox::default()));
    let writers = handle.clone();
    let response_stream = match &lifetime { Some(stream) => stream.clone(), None => Arc::new(stream.try_clone()?) };
    handle
        .insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, stream, data| {
                let _keepalive = &lifetime;
                let mut stream = &**stream;
                let mut closed = false;
                let mut count = 0;
                let mut chunk = [0; 4096];
                while count < READ_BUDGET {
                    match stream.read(&mut chunk) {
                        Ok(0) => {
                            closed = true;
                            break;
                        }
                        Ok(n) => {
                            count += n;
                            input.extend_from_slice(&chunk[..n]);
                            while let Some(end) = input.iter().position(|&b| b == b'\n') {
                                if end > MAX_LINE {
                                    return Ok(PostAction::Remove);
                                }
                                dispatch(&input[..end], data, Reply {
                                    outbox: outbox.clone(), stream: response_stream.clone(), handle: writers.clone(),
                                });
                                input.drain(..=end);
                                if outbox.borrow().bytes.len() > MAX_PENDING {
                                    return Ok(PostAction::Remove);
                                }
                            }
                            if input.len() > MAX_LINE {
                                return Ok(PostAction::Remove);
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        // Client errors must never propagate into the compositor event loop.
                        Err(_) => return Ok(PostAction::Remove),
                    }
                }
                Ok(if closed {
                    PostAction::Remove
                } else {
                    PostAction::Continue
                })
            },
        )
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(())
}

/// A response may be delivered on a later calloop turn (never from a worker).
pub struct Reply<D: 'static> {
    outbox: Rc<RefCell<Outbox>>,
    stream: Arc<UnixStream>,
    handle: LoopHandle<'static, D>,
}
impl<D: 'static> Reply<D> {
    pub fn send(self, bytes: Vec<u8>) {
        let mut output = self.outbox.borrow_mut();
        output.bytes.extend(bytes);
        if output.bytes.len() > MAX_PENDING || flush(&self.stream, &mut output).is_err() {
            output.bytes.clear();
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
            return;
        }
        if output.bytes.is_empty() || output.writer_registered { return; }
        let writer = match self.stream.try_clone() { Ok(s) => s, Err(_) => return };
        let pending = self.outbox.clone();
        let lifetime = self.stream.clone();
        if self.handle.insert_source(Generic::new(writer, Interest::WRITE, Mode::Level), move |_, stream, _| {
            let _keepalive = &lifetime;
            let mut output = pending.borrow_mut();
            if flush(stream, &mut output).is_err() { output.bytes.clear(); }
            if output.bytes.is_empty() {
                output.writer_registered = false;
                Ok(PostAction::Remove)
            } else { Ok(PostAction::Continue) }
        }).is_ok() {
            output.writer_registered = true;
        } else {
            output.bytes.clear();
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
    }
}
