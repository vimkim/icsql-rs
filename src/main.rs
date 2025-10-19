use libc::{TIOCGWINSZ, TIOCSWINSZ, ioctl, winsize};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl, open};
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::Winsize;
use nix::pty::{ForkptyResult, forkpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::stat::{Mode, SFlag, fstat};
use nix::sys::termios::Termios;
use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};
use nix::unistd::{Pid, close, dup, execvp, mkfifo, read, write};
use signal_hook::consts::signal::{SIGINT, SIGWINCH};
use signal_hook::iterator::Signals;
use std::env;
use std::ffi::CString;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::thread;
use std::time::Duration;

static DEFAULT_FIFO: &str = "/tmp/pyrepl.in";

fn ensure_fifo(path: &str) -> nix::Result<()> {
    let fd = open(path, OFlag::O_RDONLY | OFlag::O_NONBLOCK, Mode::empty())?;
    match fstat(fd) {
        Ok(st) => {
            if !SFlag::from_bits_truncate(st.st_mode).contains(SFlag::S_IFIFO) {
                return Err(Errno::EINVAL);
            }
            Ok(())
        }
        Err(_) => {
            // Create with 0666 like the Python version.
            mkfifo(path, Mode::from_bits_truncate(0o666))?;
            Ok(())
        }
    }
}

fn open_fifo_rw(path: &str) -> nix::Result<(OwnedFd, OwnedFd)> {
    // reader (nonblocking) + writer (nonblocking keepalive)
    let r = open(path, OFlag::O_RDONLY | OFlag::O_NONBLOCK, Mode::empty())?;
    let w = open(path, OFlag::O_WRONLY | OFlag::O_NONBLOCK, Mode::empty())?;
    Ok((r, w))
}

fn reopen_fifo_reader(path: &str) -> nix::Result<OwnedFd> {
    loop {
        match open(path, OFlag::O_RDONLY | OFlag::O_NONBLOCK, Mode::empty()) {
            Ok(fd) => return Ok(fd),
            Err(e) if e == Errno::ENXIO || e == Errno::ENOENT => {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

fn set_nonblocking(fd: &impl AsFd, enable: bool) -> nix::Result<()> {
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
    let newf = if enable {
        flags | OFlag::O_NONBLOCK
    } else {
        flags & !OFlag::O_NONBLOCK
    };
    fcntl(fd, FcntlArg::F_SETFL(newf))?;
    Ok(())
}

fn get_winsz(fd: &impl AsFd) -> winsize {
    let mut ws = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: ioctl is unsafe by nature; args are correct for TIOCGWINSZ.
    let fd = fd.as_fd().as_raw_fd();
    let rc = unsafe { ioctl(fd, TIOCGWINSZ.into(), &mut ws) };
    if rc < 0 {
        winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    } else {
        ws
    }
}

fn set_winsz(fd: &OwnedFd, ws: &winsize) {
    // SAFETY: ioctl with TIOCSWINSZ and a valid winsize pointer.
    unsafe {
        let _ = ioctl(fd.as_raw_fd(), TIOCSWINSZ.into(), ws as *const winsize);
    }
}

// If you currently have a libc::winsize, convert it:
fn to_nix_winsize(ws: libc::winsize) -> Winsize {
    Winsize {
        ws_row: ws.ws_row,
        ws_col: ws.ws_col,
        ws_xpixel: ws.ws_xpixel,
        ws_ypixel: ws.ws_ypixel,
    }
}

fn main() -> anyhow::Result<()> {
    // --- config -----------------------------------------------------------------
    let fifo_path = env::var("PYREPL_FIFO").unwrap_or_else(|_| DEFAULT_FIFO.to_string());

    // --- ensure FIFO exists ------------------------------------------------------
    ensure_fifo(&fifo_path).map_err(|e| anyhow::anyhow!("ensure_fifo: {e}"))?;

    // --- fork pty and exec -------------------------------------------------------
    let mut tio_opt: Option<Termios> = None;
    let stdin = io::stdin();
    let stdin_fd = stdin.as_fd();

    // capture current termios to restore later
    let old_tio = tcgetattr(stdin_fd).ok();
    if let Ok(mut raw) = tcgetattr(stdin_fd) {
        cfmakeraw(&mut raw);
        tcsetattr(stdin_fd, SetArg::TCSANOW, &raw).ok();
        tio_opt = Some(raw);
    }

    set_nonblocking(&stdin_fd, true).ok();

    // initial winsize from stdin → pty
    let ws = get_winsz(&stdin_fd);
    let winsize_nix = to_nix_winsize(ws);

    // forkpty will wire up a controlling tty for the child
    let fork_res = unsafe { forkpty(&winsize_nix, tio_opt.as_ref()) }?;

    let (master_fd, child_pid) = match fork_res {
        ForkptyResult::Child => {
            // exec csql -Sudba testdb
            let prog = CString::new("csql")?;
            let arg0 = CString::new("csql")?;
            let arg1 = CString::new("-Sudba")?;
            let arg2 = CString::new("testdb")?;
            let args = [arg0.as_c_str(), arg1.as_c_str(), arg2.as_c_str()];
            execvp(&prog, &args).unwrap_or_else(|_| {
                // If exec fails, exit(1)
                unsafe { libc::_exit(1) }
            });

            // Unreachable, but satisfies type-checker even if `_exit` above changes
            #[allow(unreachable_code)]
            {
                unsafe { std::hint::unreachable_unchecked() }
            }
        }
        ForkptyResult::Parent { child, master } => (master, child),
    };

    // match Python: set initial winsize on master
    set_winsz(&master_fd, &ws);

    // --- open FIFO reader + keepalive writer ------------------------------------
    let (fifo_r, fifo_w) =
        open_fifo_rw(&fifo_path).map_err(|e| anyhow::anyhow!("open_fifo_rw: {e}"))?;

    // --- signal handling: SIGWINCH -> propagate; SIGINT -> forward to child -----
    let mut signals = Signals::new([SIGWINCH, SIGINT])?;
    let signals_master = dup(&master_fd)?;
    thread::spawn(move || {
        for sig in signals.forever() {
            match sig {
                SIGWINCH => {
                    let ws = get_winsz(&io::stdin());
                    set_winsz(&signals_master, &ws);
                }
                SIGINT => {
                    let _ = kill(Pid::from_raw(child_pid.as_raw()), Signal::SIGINT);
                }
                _ => {}
            }
        }
    });

    // --- poll loop bridging PTY <-> stdout and FIFO/stdin -> PTY ----------------
    let mut pollfds = vec![
        PollFd::new(master_fd.as_fd(), PollFlags::POLLIN),
        PollFd::new(fifo_r.as_fd(), PollFlags::POLLIN),
        PollFd::new(stdin_fd.as_fd(), PollFlags::POLLIN),
    ];

    let stdout_fd = io::stdout();
    let mut stdout_lock = io::stdout().lock();

    // ensure stdout is a dup’d raw fd writer for direct write
    let raw_fd = stdout_fd.as_raw_fd();
    let out_fd = unsafe { OwnedFd::from_raw_fd(libc::dup(raw_fd)) };

    // logs are optional; keep console clean like the Python version (which logs to file)
    loop {
        // wait indefinitely until something is readable
        poll(&mut pollfds, PollTimeout::NONE).map_err(|e| anyhow::anyhow!("poll: {e}"))?;

        // PTY -> stdout
        if let Some(revents) = pollfds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let mut buf = [0u8; 4096];
                match read(master_fd.as_fd(), &mut buf) {
                    Ok(0) => break, // child closed
                    Ok(n) => {
                        // write to stdout
                        let _ = write(out_fd.as_fd(), &buf[..n]);
                        let _ = stdout_lock.flush();
                    }
                    Err(e) if e == Errno::EIO => break, // child exited
                    Err(Errno::EAGAIN) => {}
                    Err(e) => return Err(anyhow::anyhow!("read pty: {e}")),
                }
            }
        }

        // FIFO -> PTY
        if let Some(revents) = pollfds[1].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let mut buf = [0u8; 4096];
                match read(fifo_r.as_fd(), &mut buf) {
                    Ok(0) => {
                        // writer side closed; reopen reader to accept the next writer
                        // unregister old fd in-place
                        pollfds[1] = PollFd::new(-1 as RawFd, PollFlags::empty());
                        let _ = close(fifo_r);
                        match reopen_fifo_reader(&fifo_path) {
                            Ok(newr) => {
                                fifo_r = newr;
                                pollfds[1] = PollFd::new(fifo_r.as_fd(), PollFlags::POLLIN);
                            }
                            Err(e) => return Err(anyhow::anyhow!("reopen fifo: {e}")),
                        }
                    }
                    Ok(n) => {
                        let _ = write(master_fd.as_fd(), &buf[..n]);
                    }
                    Err(Errno::EAGAIN) => {}
                    Err(e) => return Err(anyhow::anyhow!("read fifo: {e}")),
                }
            }
        }

        // STDIN -> PTY
        if let Some(revents) = pollfds[2].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let mut buf = [0u8; 4096];
                match read(stdin_fd, &mut buf) {
                    Ok(0) => break, // stdin closed
                    Ok(n) => {
                        let _ = write(master_fd.as_fd(), &buf[..n]);
                    }
                    Err(Errno::EAGAIN) => {}
                    Err(e) => return Err(anyhow::anyhow!("read stdin: {e}")),
                }
            }
        }
    }

    // --- cleanup ----------------------------------------------------------------
    if let Some(orig) = old_tio {
        let _ = tcsetattr(stdin_fd, SetArg::TCSADRAIN, &orig);
    }
    let _ = set_nonblocking(&stdin_fd, false);
    for fd in [master_fd, fifo_r, fifo_w, out_fd] {
        let _ = close(fd);
    }

    Ok(())
}
