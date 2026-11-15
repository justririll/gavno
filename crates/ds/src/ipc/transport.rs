//! Non-blocking calloop transport, independent of compositor state for socket tests.
use std::{
    cell::RefCell,
    collections::VecDeque,
    io::{self, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    rc::Rc,
};

use smithay::reexports::calloop::{generic::Generic, Interest, LoopHandle, Mode, PostAction};

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

pub fn listen<D: 'static>(
    handle: LoopHandle<'static, D>,
    listener: UnixListener,
    dispatch: impl Fn(&[u8], &mut D) -> Vec<u8> + Clone + 'static,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let clients = handle.clone();
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, _| {
                // Bound the work per event so a connection flood cannot starve rendering.
                for _ in 0..32 {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Err(err) = add_client(clients.clone(), stream, dispatch.clone())
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
    Ok(())
}

pub fn add_client<D: 'static>(
    handle: LoopHandle<'static, D>,
    stream: UnixStream,
    dispatch: impl Fn(&[u8], &mut D) -> Vec<u8> + 'static,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let mut input = Vec::new();
    let outbox = Rc::new(RefCell::new(Outbox::default()));
    let writers = handle.clone();
    handle
        .insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, stream, data| {
                let mut stream = &**stream;
                let mut output = outbox.borrow_mut();
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
                                output.bytes.extend(dispatch(&input[..end], data));
                                input.drain(..=end);
                                if output.bytes.len() > MAX_PENDING {
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
                if flush(stream, &mut output).is_err() {
                    return Ok(PostAction::Remove);
                }
                if !output.bytes.is_empty() && !output.writer_registered {
                    let writer = match stream.try_clone() {
                        Ok(writer) => writer,
                        Err(_) => return Ok(PostAction::Remove),
                    };
                    let pending = outbox.clone();
                    if writers
                        .insert_source(
                            Generic::new(writer, Interest::WRITE, Mode::Level),
                            move |_, stream, _| {
                                let mut output = pending.borrow_mut();
                                if flush(stream, &mut output).is_err() {
                                    output.bytes.clear();
                                }
                                if output.bytes.is_empty() {
                                    output.writer_registered = false;
                                    Ok(PostAction::Remove)
                                } else {
                                    Ok(PostAction::Continue)
                                }
                            },
                        )
                        .is_err()
                    {
                        return Ok(PostAction::Remove);
                    }
                    output.writer_registered = true;
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
