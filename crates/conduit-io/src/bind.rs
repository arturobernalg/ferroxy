//! Synchronous bind helper: socket2 lets us set `SO_REUSEPORT` (mandatory
//! for thread-per-core sharding) and `SO_REUSEADDR` before `bind(2)`.
//! monoio's `TcpListener::bind` does not expose those options; we use it
//! only via `from_std` after this helper has done the work.

use std::io;
use std::net::SocketAddr;

use socket2::{Domain, Protocol, Socket, Type};

/// Bind a TCP listener with our standard options.
///
/// `reuse_port = true` enables `SO_REUSEPORT` (Linux semantics: kernel-side
/// load balancing across all binders on the same `(addr, port)`). On
/// non-Unix targets this is a hard error since the engineering charter
/// targets Linux 6.6+.
pub(crate) fn bind(
    addr: SocketAddr,
    backlog: u32,
    reuse_port: bool,
) -> io::Result<std::net::TcpListener> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

    socket.set_nonblocking(true)?;
    socket.set_reuse_address(true)?;

    if reuse_port {
        #[cfg(unix)]
        {
            socket.set_reuse_port(true)?;
        }
        #[cfg(not(unix))]
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "SO_REUSEPORT requires a Unix host",
            ));
        }
    }

    socket.bind(&addr.into())?;
    let backlog_i32 = i32::try_from(backlog)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "backlog overflows i32"))?;
    socket.listen(backlog_i32)?;

    Ok(std::net::TcpListener::from(socket))
}
