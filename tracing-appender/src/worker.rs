use crate::Msg;
use crossbeam_channel::{Receiver, RecvError, TryRecvError};
use std::fmt;
use std::io::Write;
use std::{io, thread};

#[derive(Default)]
pub struct WorkerOptions {
    on_thread_start: Option<Box<dyn Fn() + Send + Sync + 'static>>,
    on_thread_stop: Option<Box<dyn Fn() + Send + Sync + 'static>>,
}

impl WorkerOptions {
    pub fn on_thread_start(self, cb: impl Fn() + Send + Sync + 'static) -> Self {
        WorkerOptions {
            on_thread_start: Some(Box::new(cb)),
            ..self
        }
    }

    pub fn on_thread_stop(self, cb: impl Fn() + Send + Sync + 'static) -> Self {
        WorkerOptions {
            on_thread_stop: Some(Box::new(cb)),
            ..self
        }
    }
}

impl fmt::Debug for WorkerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerOptions")
            .field(
                "on_thread_start",
                if self.on_thread_start.is_some() {
                    &"Some(callback)"
                } else {
                    &"None"
                },
            )
            .field(
                "on_thread_stop",
                if self.on_thread_stop.is_some() {
                    &"Some(callback)"
                } else {
                    &"None"
                },
            )
            .finish()
    }
}

pub(crate) struct Worker<T: Write + Send + Sync + 'static> {
    writer: T,
    receiver: Receiver<Msg>,
    shutdown: Receiver<()>,
    options: WorkerOptions,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum WorkerState {
    Empty,
    Disconnected,
    Continue,
    Shutdown,
}

impl<T: Write + Send + Sync + 'static> Worker<T> {
    pub(crate) fn new(
        receiver: Receiver<Msg>,
        writer: T,
        shutdown: Receiver<()>,
        options: WorkerOptions,
    ) -> Worker<T> {
        Self {
            writer,
            receiver,
            shutdown,
            options,
        }
    }

    fn handle_recv(&mut self, result: &Result<Msg, RecvError>) -> io::Result<WorkerState> {
        match result {
            Ok(Msg::Line(msg)) => {
                self.writer.write_all(msg)?;
                Ok(WorkerState::Continue)
            }
            Ok(Msg::Shutdown) => Ok(WorkerState::Shutdown),
            Err(_) => Ok(WorkerState::Disconnected),
        }
    }

    fn handle_try_recv(&mut self, result: &Result<Msg, TryRecvError>) -> io::Result<WorkerState> {
        match result {
            Ok(Msg::Line(msg)) => {
                self.writer.write_all(msg)?;
                Ok(WorkerState::Continue)
            }
            Ok(Msg::Shutdown) => Ok(WorkerState::Shutdown),
            Err(TryRecvError::Empty) => Ok(WorkerState::Empty),
            Err(TryRecvError::Disconnected) => Ok(WorkerState::Disconnected),
        }
    }

    /// Blocks on the first recv of each batch of logs, unless the
    /// channel is disconnected. Afterwards, grabs as many logs as
    /// it can off the channel, buffers them and attempts a flush.
    pub(crate) fn work(&mut self) -> io::Result<WorkerState> {
        // Worker thread yields here if receive buffer is empty
        let mut worker_state = self.handle_recv(&self.receiver.recv())?;

        while worker_state == WorkerState::Continue {
            let try_recv_result = self.receiver.try_recv();
            let handle_result = self.handle_try_recv(&try_recv_result);
            worker_state = handle_result?;
        }
        self.writer.flush()?;
        Ok(worker_state)
    }

    /// Creates a worker thread that processes a channel until it's disconnected
    pub(crate) fn worker_thread(mut self) -> std::thread::JoinHandle<()> {
        thread::Builder::new()
            .name("tracing-appender".to_string())
            .spawn(move || {
                self.options.on_thread_start.as_ref().map(|cb| cb());

                loop {
                    match self.work() {
                        Ok(WorkerState::Continue) | Ok(WorkerState::Empty) => {}
                        Ok(WorkerState::Shutdown) | Ok(WorkerState::Disconnected) => {
                            let _ = self.shutdown.recv();
                            break;
                        }
                        Err(_) => {
                            // TODO: Expose a metric for IO Errors, or print to stderr
                        }
                    }
                }
                if let Err(e) = self.writer.flush() {
                    eprintln!("Failed to flush. Error: {}", e);
                }

                self.options.on_thread_stop.as_ref().map(|cb| cb());
            })
            .expect("failed to spawn `tracing-appender` non-blocking worker thread")
    }
}
