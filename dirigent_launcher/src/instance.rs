//! Per-user/channel desktop handoff. The OS owns the endpoint lifetime, so crashes leave no lock.
//! Production uses Windows named pipes; tests also run against Linux abstract sockets.

use interprocess::{
    ConnectWaitMode,
    local_socket::{
        GenericNamespaced, Listener, ListenerNonblockingMode, ListenerOptions, Stream, prelude::*,
    },
};
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(25);
const MAX_REQUEST: usize = 128 * 1024;

pub struct Request {
    pub project_directory: Result<Option<PathBuf>, String>,
    handled: mpsc::Sender<()>,
}

impl Request {
    /// Acknowledge only after the UI has opened the project and activated its window.
    pub fn handled(self) {
        let _ = self.handled.send(());
    }
}

/// Held until the desktop and its database workers have shut down. Drop before update relaunch.
pub struct Instance {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Instance {
    /// Returns a primary-instance guard, or None after the existing window handles this launch.
    /// `endpoint` must identify the user's channel, not an installation version.
    pub fn claim_or_forward(
        endpoint: &str,
        project_directory: &Result<Option<PathBuf>, String>,
        mut handle: impl FnMut(Request) -> bool + Send + 'static,
    ) -> io::Result<Option<Self>> {
        let name = endpoint.to_ns_name::<GenericNamespaced>()?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match ListenerOptions::new()
                .name(name.clone())
                .nonblocking(ListenerNonblockingMode::Both)
                .create_sync()
            {
                Ok(listener) => {
                    let stop = Arc::new(AtomicBool::new(false));
                    let worker_stop = stop.clone();
                    let worker = thread::Builder::new()
                        .name("dirigent-launch-requests".into())
                        .spawn(move || serve(listener, &worker_stop, &mut handle))?;
                    return Ok(Some(Self {
                        stop,
                        worker: Some(worker),
                    }));
                }
                // Windows reports an existing FIRST_PIPE_INSTANCE as access denied.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
                    ) => {}
                Err(error) => return Err(error),
            }
            match connect(endpoint) {
                Ok(stream) => {
                    allow_foreground(&stream);
                    let stop = AtomicBool::new(false);
                    let mut stream = BoundedStream {
                        stream,
                        deadline,
                        stop: &stop,
                    };
                    let bytes = serde_json::to_vec(project_directory)?;
                    if bytes.len() > MAX_REQUEST {
                        return Err(io::Error::other("launch request is too large"));
                    }
                    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
                    stream.write_all(&bytes)?;
                    let mut ack = [0];
                    stream.read_exact(&mut ack)?;
                    if ack != [1] {
                        return Err(io::Error::other("invalid launch acknowledgement"));
                    }
                    return Ok(None);
                }
                Err(error) if Instant::now() >= deadline => return Err(error),
                // The other process may still be starting or may have just exited. Re-elect
                // only before sending a request; never start a second writer beside a live UI.
                Err(_) => thread::sleep(POLL),
            }
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(windows)]
fn connect(endpoint: &str) -> io::Result<Stream> {
    use interprocess::os::windows::named_pipe::{
        DuplexPipeStream, local_socket::Stream as PipeStream, pipe_mode::Bytes,
    };
    // The local-socket adapter currently ignores ConnectOptions' timeout on Windows.
    let pipe = DuplexPipeStream::<Bytes>::connect_by_path_with_wait_mode(
        format!(r"\\.\pipe\{endpoint}"),
        ConnectWaitMode::Timeout(POLL),
    )?;
    let stream = Stream::from(PipeStream::from(pipe));
    stream.set_nonblocking(true)?;
    Ok(stream)
}

#[cfg(not(windows))]
fn connect(endpoint: &str) -> io::Result<Stream> {
    interprocess::local_socket::ConnectOptions::new()
        .name(endpoint.to_ns_name::<GenericNamespaced>()?)
        .wait_mode(ConnectWaitMode::Timeout(POLL))
        .nonblocking_stream(true)
        .connect_sync()
}

fn serve(listener: Listener, stop: &AtomicBool, handle: &mut impl FnMut(Request) -> bool) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok(stream) => {
                let mut stream = BoundedStream {
                    stream,
                    deadline: Instant::now() + TIMEOUT,
                    stop,
                };
                // A disconnected/malformed client must not stop subsequent launches.
                let _ = receive(&mut stream, handle);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // Keep ownership even on accept errors: a live UI must never lose its endpoint
            // and allow a second process to open the same state database.
            Err(_) => thread::sleep(POLL),
        }
    }
}

fn receive(
    stream: &mut BoundedStream<'_>,
    handle: &mut impl FnMut(Request) -> bool,
) -> io::Result<()> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_REQUEST {
        return Err(io::Error::other("launch request is too large"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    let project_directory = serde_json::from_slice(&bytes)?;
    let (handled, ack) = mpsc::channel();
    if !handle(Request {
        project_directory,
        handled,
    }) {
        return Err(io::Error::other("desktop is shutting down"));
    }
    retry(stream.stop, stream.deadline, || match ack.try_recv() {
        Ok(()) => Ok(()),
        Err(mpsc::TryRecvError::Empty) => Err(io::ErrorKind::WouldBlock.into()),
        Err(mpsc::TryRecvError::Disconnected) => {
            Err(io::Error::other("window closed before handling launch"))
        }
    })?;
    stream.write_all(&[1])
}

// Named pipes lack I/O timeouts. Nonblocking reads/writes keep shutdown interruptible and
// bound the entire exchange (including UI startup), rather than each individual read.
struct BoundedStream<'a> {
    stream: Stream,
    deadline: Instant,
    stop: &'a AtomicBool,
}

fn retry<T>(
    stop: &AtomicBool,
    deadline: Instant,
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the existing Dirigent window did not respond",
            ));
        }
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

impl Read for BoundedStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        retry(self.stop, self.deadline, || {
            let result = self.stream.read(buffer);
            // Empty nonblocking pipes return raw ERROR_NO_DATA, not Rust's WouldBlock.
            #[cfg(windows)]
            if result.as_ref().is_err_and(|error| {
                error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_NO_DATA as i32)
            }) {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            result
        })
    }
}

impl Write for BoundedStream<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        retry(self.stop, self.deadline, || {
            match self.stream.write(buffer) {
                // Byte-mode PIPE_NOWAIT writes can succeed with zero bytes when the buffer is full.
                #[cfg(windows)]
                Ok(0) if !buffer.is_empty() => Err(io::ErrorKind::WouldBlock.into()),
                result => result,
            }
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(()) // The protocol uses an explicit acknowledgement, not a pipe flush.
    }
}

#[cfg(windows)]
fn allow_foreground(stream: &Stream) {
    use interprocess::local_socket::traits::StreamCommon as _;
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    if let Ok(credentials) = stream.peer_creds()
        && let Some(pid) = credentials.pid()
    {
        // Explorer granted this launch foreground rights; pass them to the actual window owner.
        unsafe {
            AllowSetForegroundWindow(pid);
        }
    }
}

#[cfg(not(windows))]
fn allow_foreground(_stream: &Stream) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_endpoint() -> String {
        format!(
            "dirigent-test-{}-{}",
            std::process::id(),
            tempfile::tempdir()
                .unwrap()
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
        )
    }

    #[test]
    fn forwards_multiple_launches_and_releases_endpoint_for_restart() {
        let endpoint = unique_endpoint();
        let (tx, rx) = mpsc::channel();
        let owner = Instance::claim_or_forward(&endpoint, &Ok(None), move |request| {
            tx.send(request).is_ok()
        })
        .unwrap()
        .unwrap();
        for directory in [
            None,
            Some(PathBuf::from(r"C:\Projects\a path with spaces\λ\.")),
        ] {
            let name = endpoint.clone();
            let expected = directory.clone();
            let client = thread::spawn(move || {
                Instance::claim_or_forward(&name, &Ok(directory), |_| panic!("second primary"))
            });
            let request = rx.recv_timeout(TIMEOUT).unwrap();
            assert_eq!(request.project_directory.clone().unwrap(), expected);
            // The client waits for the UI, not just the socket accepting the message.
            assert!(!client.is_finished());
            request.handled();
            assert!(client.join().unwrap().unwrap().is_none());
        }
        // Other channels/users have independent endpoints.
        let other = Instance::claim_or_forward(&unique_endpoint(), &Ok(None), |_| true)
            .unwrap()
            .unwrap();
        drop(other);
        drop(owner);
        assert!(
            Instance::claim_or_forward(&endpoint, &Ok(None), |_| true)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn simultaneous_launches_elect_one_window_owner() {
        let endpoint = unique_endpoint();
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let (tx, rx) = mpsc::channel();
        let launches: Vec<_> = (0..4)
            .map(|_| {
                let endpoint = endpoint.clone();
                let barrier = barrier.clone();
                let tx = tx.clone();
                thread::spawn(move || {
                    barrier.wait();
                    Instance::claim_or_forward(&endpoint, &Ok(None), move |request| {
                        tx.send(()).unwrap();
                        request.handled();
                        true
                    })
                    .unwrap()
                })
            })
            .collect();
        let owners: Vec<_> = launches
            .into_iter()
            .map(|launch| launch.join().unwrap())
            .collect();
        assert_eq!(owners.iter().filter(|owner| owner.is_some()).count(), 1);
        assert_eq!(rx.try_iter().count(), 3);
    }

    #[test]
    fn shutdown_interrupts_an_incomplete_client() {
        let endpoint = unique_endpoint();
        let owner =
            Instance::claim_or_forward(&endpoint, &Ok(None), |_| panic!("incomplete request"))
                .unwrap()
                .unwrap();
        let _client = Stream::connect(endpoint.to_ns_name::<GenericNamespaced>().unwrap()).unwrap();
        thread::sleep(POLL * 2);
        let start = Instant::now();
        drop(owner);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
