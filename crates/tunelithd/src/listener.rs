//! Where clients connect: a Unix domain socket, or a named pipe on Windows.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use tokio::net::{UnixListener, UnixStream};
    use tokio::signal::unix::{SignalKind, signal};

    use super::*;

    pub type Connection = UnixStream;

    pub struct Listener {
        listener: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        /// Binds the socket, taking over a stale one but not one in use, and
        /// lets the owner and `group` connect to it.
        pub async fn bind(path: &Path, group: &str) -> io::Result<Self> {
            if UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("tunelithd is already listening on {}", path.display()),
                ));
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
            let listener = UnixListener::bind(path)?;
            share(path, group)?;
            Ok(Self {
                listener,
                path: path.to_owned(),
            })
        }

        pub async fn accept(&mut self) -> io::Result<Connection> {
            Ok(self.listener.accept().await?.0)
        }

        pub fn close(self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn share(path: &Path, group: &str) -> io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
        if group.is_empty() {
            return Ok(());
        }
        match gid(group)? {
            Some(gid) => std::os::unix::fs::chown(path, None, Some(gid)),
            None => {
                eprintln!("no group {group}: the socket is for its owner alone");
                Ok(())
            }
        }
    }

    /// The id of `group`.
    // ponytail: /etc/group alone; groups from NSS (LDAP and the like) need
    // getgrnam.
    fn gid(group: &str) -> io::Result<Option<u32>> {
        let groups = std::fs::read_to_string("/etc/group")?;
        Ok(groups.lines().find_map(|line| {
            let mut fields = line.split(':');
            (fields.next() == Some(group))
                .then(|| fields.nth(1)?.parse().ok())
                .flatten()
        }))
    }

    /// Returns on Ctrl-C or SIGTERM.
    pub async fn shutdown() -> io::Result<()> {
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    use super::*;

    pub type Connection = NamedPipeServer;

    /// A named pipe, with the instance the next client connects to.
    pub struct Listener {
        path: OsString,
        next: NamedPipeServer,
    }

    impl Listener {
        /// Creates the pipe, failing if another process serves it.
        // ponytail: the default ACL of a pipe; restricting it to a group, as
        // the socket is on Unix, takes a security descriptor.
        pub async fn bind(path: &Path, _group: &str) -> io::Result<Self> {
            let path = path.as_os_str().to_owned();
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&path)?;
            Ok(Self { path, next })
        }

        pub async fn accept(&mut self) -> io::Result<Connection> {
            self.next.connect().await?;
            let next = ServerOptions::new().create(&self.path)?;
            Ok(std::mem::replace(&mut self.next, next))
        }

        pub fn close(self) {}
    }

    /// Returns on Ctrl-C.
    pub async fn shutdown() -> io::Result<()> {
        tokio::signal::ctrl_c().await
    }
}
