//! Transfer systemd ownership before the exporting server can exit.
use std::io::{self, Read};
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram, UnixStream};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

pub(super) fn validate_handoff_fds(fds: &[std::os::fd::RawFd]) -> io::Result<()> {
    // getppid only reads process metadata; the exporter remains alive until commit.
    let parent = unsafe { libc::getppid() };
    let source = std::fs::read_dir(format!("/proc/{parent}/fdinfo"))?
        .map(|entry| entry.and_then(|e| tty_index(e.path())))
        .collect::<io::Result<Vec<_>>>()?;
    let received = fds
        .iter()
        .map(|fd| tty_index(PathBuf::from(format!("/proc/self/fdinfo/{fd}"))))
        .collect::<io::Result<Vec<_>>>()?;
    if source
        .iter()
        .flatten()
        .any(|id| !received.iter().flatten().any(|other| other == id))
    {
        return Err(io::Error::other(
            "handoff omits live PTYs (popup or float); source server retained",
        ));
    }
    Ok(())
}

fn tty_index(path: PathBuf) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .find_map(|line| line.strip_prefix("tty-index:").map(|s| s.trim().to_owned()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

static SOCKET: OnceLock<Option<PathBuf>> = OnceLock::new();

pub(super) fn initialize() {
    SOCKET.get_or_init(|| {
        let socket = std::env::var_os("NOTIFY_SOCKET").map(PathBuf::from);
        // Only server replacements inherit this capability, never pane commands.
        std::env::remove_var("NOTIFY_SOCKET");
        socket
    });
}

pub(super) fn configure_child(command: &mut std::process::Command) {
    if let Some(Some(socket)) = SOCKET.get() {
        command.env("NOTIFY_SOCKET", socket);
    }
}

pub(super) fn set_main_pid(pid: u32) -> io::Result<()> {
    let Some(Some(path)) = SOCKET.get() else {
        return Ok(());
    };
    notify(
        path.as_os_str().as_encoded_bytes(),
        &format!("MAINPID={pid}\nREADY=1"),
    )
}

pub(super) fn check_connection() -> io::Result<()> {
    let Some(Some(path)) = SOCKET.get() else {
        return Ok(());
    };
    notify(
        path.as_os_str().as_encoded_bytes(),
        "STATUS=Preparing live handoff",
    )
}

pub(super) fn commit_handoff(commit: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    // getppid does not access memory; the exporting parent waits for our ready/owned messages.
    let parent = unsafe { libc::getppid() } as u32;
    set_main_pid(std::process::id())?;
    let result = commit();
    if result.is_err() {
        set_main_pid(parent)?;
    }
    result
}

fn notify(path: &[u8], message: &str) -> io::Result<()> {
    let addr = if let Some(name) = path.strip_prefix(b"@") {
        SocketAddr::from_abstract_name(name)?
    } else {
        SocketAddr::from_pathname(std::ffi::OsStr::from_bytes(path))?
    };
    let socket = UnixDatagram::unbound()?;
    socket.connect_addr(&addr)?;
    socket.send(message.as_bytes())?;
    barrier(&socket)
}

fn barrier(socket: &UnixDatagram) -> io::Result<()> {
    let (mut receiver, sender) = UnixStream::pair()?;
    receiver.set_read_timeout(Some(Duration::from_secs(2)))?;
    let message = b"BARRIER=1";
    let mut iov = libc::iovec {
        iov_base: message.as_ptr().cast_mut().cast(),
        iov_len: message.len(),
    };
    // usize storage keeps the ancillary header aligned for CMSG_DATA.
    // CMSG_SPACE computes a size and does not dereference memory.
    let size = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as usize;
    let mut control = vec![0usize; size.div_ceil(size_of::<usize>())];
    // A zeroed msghdr is valid; all referenced buffers remain live through sendmsg.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = size;
    // The aligned buffer has CMSG_SPACE bytes, enough for one header and one fd.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast(), sender.as_raw_fd());
        if libc::sendmsg(socket.as_raw_fd(), &header, libc::MSG_NOSIGNAL) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    drop(sender);
    let mut byte = [0];
    match receiver.read(&mut byte)? {
        0 => Ok(()),
        _ => Err(io::Error::other("unexpected systemd barrier response")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_pid_notification_is_followed_by_a_barrier() {
        let name = format!("herdr-notify-test-{}", std::process::id());
        let address = SocketAddr::from_abstract_name(name.as_bytes()).expect("abstract address");
        let server = UnixDatagram::bind_addr(&address).expect("bind test notification socket");
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("receive timeout");
        std::thread::scope(|scope| {
            let client =
                scope.spawn(|| notify(format!("@{name}").as_bytes(), "MAINPID=1234\nREADY=1"));
            let mut bytes = [0; 128];
            let size = server.recv(&mut bytes).expect("main PID notification");
            assert_eq!(&bytes[..size], b"MAINPID=1234\nREADY=1");
            // recv discards SCM_RIGHTS, closing its descriptor and releasing the barrier.
            let size = server.recv(&mut bytes).expect("notification barrier");
            assert_eq!(&bytes[..size], b"BARRIER=1");
            client
                .join()
                .expect("notifier thread")
                .expect("barrier acknowledged");
        });
    }

    #[test]
    fn a_missing_manager_is_an_error() {
        let name = format!("@herdr-missing-manager-{}", std::process::id());
        assert!(notify(name.as_bytes(), "MAINPID=1234").is_err());
    }
}
