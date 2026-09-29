//! Trust between RDM and the local programs that talk to its bridge (127.0.0.1): the native
//! connector started by the browsers, `rdm --quit` (the installers), a second launch.
//!
//! - **They prove they are the user's**: RDM writes a random token, renewed at each start, in a
//!   file only the user can read (the per-user settings folder: private profile on Windows, 0700
//!   on Linux); they send it back (`x-rdm-token`). Another account of the computer, or any program
//!   that cannot read the user's files, is refused — before, "no `Origin`" was enough.
//! - **RDM proves it is RDM**: before sending anything (the extension's requests carry the page's
//!   cookies), they check that the program listening on the bridge's port runs as the same user.
//!   Another account starting a program on that port first (before RDM) receives nothing.
//! - **Only the user's programs get in at all**: the bridge closes at once a connection whose
//!   other end is a program of another account ([`peer_is_mine`]). A program, unlike a browser, can
//!   forge the extension's `Origin`: without this, another account could hand RDM downloads.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::OnceLock,
};

use crate::settings::BRIDGE_PORT;

/// The header carrying the token.
pub const TOKEN_HEADER: &str = "x-rdm-token";
const TOKEN_FILE: &str = "bridge.token";

/// The token the running bridge accepts (set once, at start).
static SERVED: OnceLock<String> = OnceLock::new();

/// Where the token lives: the user's own settings folder — always the default one, whatever
/// `RDM_CONFIG_DIR` says, since the browsers start the connector without it.
fn token_file() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "rdm", "rdm").map(|d| d.config_dir().join(TOKEN_FILE))
}

/// At start, once the bridge's port is ours: a new token, written for the local programs.
pub fn issue_token() -> String {
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    if let Some(path) = token_file() {
        if let Some(dir) = path.parent() {
            let _ = crate::settings::create_private_dir(dir);
        }
        let tmp = crate::settings::with_suffix(&path, ".tmp");
        let replaced = crate::settings::write_private(&tmp, token.as_bytes()).and_then(|()| std::fs::rename(&tmp, &path));
        // Never left with the previous session's token (every local program would be refused).
        if replaced.is_err() {
            let _ = std::fs::remove_file(&tmp);
            let _ = crate::settings::write_private(&path, token.as_bytes());
        }
    }
    let _ = SERVED.set(token.clone());
    token
}

/// The token the bridge accepts, once issued.
pub fn served_token() -> Option<&'static str> {
    SERVED.get().map(String::as_str)
}

/// The running RDM's token, for a local program talking to it (read at each request: RDM may have
/// restarted meanwhile). `None` when RDM has not written one (an older RDM does not need it).
pub fn read_token() -> Option<String> {
    let text = std::fs::read_to_string(token_file()?).ok()?;
    let token = text.trim();
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())).then(|| token.to_owned())
}

/// Whether `given` is `expected`, compared in constant time (no timing hint on how much matched).
pub fn token_matches(expected: &str, given: Option<&[u8]>) -> bool {
    let Some(given) = given else { return false };
    let expected = expected.as_bytes();
    given.len() == expected.len() && given.iter().zip(expected).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// Whether whatever listens on the bridge's port (127.0.0.1 or every address) runs as this user —
/// or nothing listens. `false` when another account's program holds it: nothing may be sent there.
pub fn bridge_is_ours() -> bool {
    listeners_are_mine(BRIDGE_PORT)
}

/// See [`bridge_is_ours`]. When the system cannot tell (no such information), `true`.
pub fn listeners_are_mine(port: u16) -> bool {
    imp::listeners_are_mine(port)
}

/// Whether the program that connected from `peer` to this computer's `server_port` (a connection
/// the bridge just accepted) runs as this user. When the system cannot tell (its connection table
/// is unreadable), `true`; a connection missing from a table that could be read, `false`.
pub fn peer_is_mine(peer: SocketAddr, server_port: u16) -> bool {
    imp::peer_is_mine(peer, server_port)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::net::{IpAddr, SocketAddr};

    /// `/proc/net/tcp` and `tcp6` list every socket of this network namespace with its owner's uid.
    /// Both: an IPv6 socket on every address (`[::]`, dual-stack) receives 127.0.0.1's connections too.
    fn tables() -> Vec<String> {
        ["/proc/net/tcp", "/proc/net/tcp6"].iter().filter_map(|t| std::fs::read_to_string(t).ok()).collect()
    }

    fn my_uid() -> u32 {
        // SAFETY: `getuid` cannot fail and has no side effect.
        unsafe { libc::getuid() }
    }

    pub fn listeners_are_mine(port: u16) -> bool {
        let tables = tables();
        tables.is_empty() || tables.iter().flat_map(|table| owners(table, port)).all(|uid| uid == my_uid())
    }

    pub fn peer_is_mine(peer: SocketAddr, server_port: u16) -> bool {
        let tables = tables();
        tables.is_empty() || tables.iter().find_map(|table| peer_owner(table, peer, server_port)) == Some(my_uid())
    }

    /// An address as these tables write it: the raw bytes in hexadecimal, 32 bits at a time in the
    /// machine's order (127.0.0.1 is `0100007F`).
    fn hex(bytes: &[u8]) -> String {
        bytes.chunks(4).map(|w| format!("{:08X}", u32::from_ne_bytes([w[0], w[1], w[2], w[3]]))).collect()
    }

    /// Owner (uid) of the socket at the other end of a connection from `peer` to `server_port`: the
    /// row whose local address is `peer` (as IPv4, or mapped into IPv6) and whose remote port is
    /// `server_port`. A socket in TIME_WAIT (06) belongs to nobody any more: skipped.
    pub fn peer_owner(table: &str, peer: SocketAddr, server_port: u16) -> Option<u32> {
        let ip = match peer.ip() {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(v6) => v6.to_ipv4_mapped()?,
        };
        let port = format!(":{:04X}", peer.port());
        let local = [hex(&ip.octets()) + &port, hex(&ip.to_ipv6_mapped().octets()) + &port];
        let remote = format!(":{server_port:04X}");
        table.lines().skip(1).find_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (&address, &to, &state, &uid) = (fields.get(1)?, fields.get(2)?, fields.get(3)?, fields.get(7)?);
            (local.iter().any(|l| l == address) && to.ends_with(&remote) && state != "06").then(|| uid.parse().ok())?
        })
    }

    /// Owners (uid) of the sockets listening on `port` where a connection to 127.0.0.1 can land:
    /// 127.0.0.1, every IPv4 address, every IPv6 address, or 127.0.0.1 mapped into IPv6.
    pub fn owners(table: &str, port: u16) -> Vec<u32> {
        // The raw bytes in hexadecimal, 32 bits at a time in the machine's order: 127.0.0.1 is 0100007F.
        const REACHABLE: [&str; 4] = ["0100007F", "00000000", "00000000000000000000000000000000", "0000000000000000FFFF00000100007F"];
        let port = format!(":{port:04X}");
        table
            .lines()
            .skip(1)
            .filter_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let (&local, &state, &uid) = (fields.get(1)?, fields.get(3)?, fields.get(7)?);
                let address = local.strip_suffix(port.as_str())?;
                // 0A: LISTEN.
                (state == "0A" && REACHABLE.contains(&address)).then(|| uid.parse().ok())?
            })
            .collect()
    }
}

#[cfg(windows)]
mod imp {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, NO_ERROR},
        NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_CLASS, TCP_TABLE_OWNER_PID_CONNECTIONS,
            TCP_TABLE_OWNER_PID_LISTENER,
        },
        Networking::WinSock::{AF_INET, AF_INET6},
        Security::{EqualSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser},
        System::{
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION},
        },
    };

    /// TCP states as Windows numbers them (`MIB_TCP_STATE`).
    const LISTEN: u32 = 2;
    const TIME_WAIT: u32 = 11;

    /// The listening sockets with their process; each process's account compared with ours.
    pub fn listeners_are_mine(port: u16) -> bool {
        listening_pids(port).is_none_or(|pids| pids.into_iter().all(pid_is_mine))
    }

    /// The socket at the other end of the connection: the one whose local address is `peer` (as
    /// IPv4, or mapped into IPv6) and whose remote port is `server_port`. A socket in TIME_WAIT
    /// belongs to nobody any more: skipped.
    pub fn peer_is_mine(peer: SocketAddr, server_port: u16) -> bool {
        let SocketAddr::V4(peer) = peer else { return false }; // the bridge listens on IPv4 only
        let v4 = rows::<6>(AF_INET, TCP_TABLE_OWNER_PID_CONNECTIONS, |&[state, address, local_port, _, remote_port, pid]| {
            let local = Ipv4Addr::from(address.to_ne_bytes()) == *peer.ip() && port(local_port) == peer.port();
            (local && port(remote_port) == server_port && state != TIME_WAIT).then_some(pid)
        });
        let Some(v4) = v4 else { return true }; // no table: cannot tell
        let pid = v4.first().copied().or_else(|| {
            // MIB_TCP6ROW_OWNER_PID: see `listening_pids`; the remote port is field 11.
            let v6 = rows::<14>(AF_INET6, TCP_TABLE_OWNER_PID_CONNECTIONS, |row| {
                let local = address6(row).to_ipv4_mapped() == Some(*peer.ip()) && port(row[5]) == peer.port();
                (local && port(row[11]) == server_port && row[12] != TIME_WAIT).then_some(row[13])
            });
            v6?.first().copied()
        });
        pid.is_some_and(pid_is_mine)
    }

    /// A port as the tables give it: in network byte order, in the low 16 bits.
    fn port(field: u32) -> u16 {
        u16::from_be(field as u16)
    }

    /// The local address of a `MIB_TCP6ROW_OWNER_PID` (its first 16 bytes).
    fn address6(row: &[u32; 14]) -> Ipv6Addr {
        let [a, b, c, d] = [row[0], row[1], row[2], row[3]].map(u32::to_ne_bytes);
        Ipv6Addr::from([a, b, c, d].concat::<u8>().try_into().unwrap_or([0xff; 16]))
    }

    /// Whether process `pid` runs under this process's account.
    fn pid_is_mine(pid: u32) -> bool {
        // SAFETY: plain Win32 calls on the pseudo-handle of this process and on a handle closed
        // below.
        unsafe {
            let me = user_of(GetCurrentProcess());
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !process.is_null() {
                let owner = user_of(process);
                CloseHandle(process);
                if let (Some(a), Some(b)) = (&me, &owner) {
                    return EqualSid(sid(a), sid(b)) != 0;
                }
            }
            // Its account cannot be read — RDM started "as administrator" by this user, whose
            // token a normal process may not open: then it must at least run in this session
            // (another user's session, or a service, is not ours).
            same_session(pid)
        }
    }

    fn same_session(pid: u32) -> bool {
        let (mut theirs, mut ours) = (u32::MAX, u32::MAX - 1);
        // SAFETY: plain queries writing a u32 each.
        unsafe { ProcessIdToSessionId(pid, &mut theirs) != 0 && ProcessIdToSessionId(GetCurrentProcessId(), &mut ours) != 0 && theirs == ours }
    }

    /// The processes listening on `port` where a connection to 127.0.0.1 can land: 127.0.0.1 or
    /// every IPv4 address, and in IPv6 every address (a dual-stack socket takes IPv4 too) or
    /// 127.0.0.1 mapped into IPv6. States and ports as Windows gives them: LISTEN is 2, the port
    /// and addresses in network byte order.
    pub(super) fn listening_pids(port: u16) -> Option<Vec<u32>> {
        const _: () = assert!(size_of::<MIB_TCPROW_OWNER_PID>() == 6 * 4 && size_of::<MIB_TCP6ROW_OWNER_PID>() == 14 * 4);
        let v4 = rows::<6>(AF_INET, TCP_TABLE_OWNER_PID_LISTENER, |&[state, address, local_port, _, _, pid]| {
            let address = Ipv4Addr::from(address.to_ne_bytes());
            let listening = state == LISTEN && self::port(local_port) == port;
            (listening && (address.is_loopback() || address.is_unspecified())).then_some(pid)
        })?;
        // MIB_TCP6ROW_OWNER_PID: local address (4 × u32), scope, port, remote address, scope, port, state, pid.
        let v6 = rows::<14>(AF_INET6, TCP_TABLE_OWNER_PID_LISTENER, |row| {
            let address = address6(row);
            let reaches = address.is_unspecified() || address.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback());
            (row[12] == LISTEN && self::port(row[5]) == port && reaches).then_some(row[13])
        })
        .unwrap_or_default(); // no IPv6 on this computer: nothing there
        Some([v4, v6].concat())
    }

    /// The sockets of one address family and `class` (listening, or connected), `N` u32 per row,
    /// each kept by `pick`.
    fn rows<const N: usize>(family: u16, class: TCP_TABLE_CLASS, pick: impl Fn(&[u32; N]) -> Option<u32>) -> Option<Vec<u32>> {
        let mut size = 0u32;
        // SAFETY: a null buffer only asks for the size needed.
        unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, u32::from(family), class, 0) };
        for _ in 0..4 {
            // u32 units: the table (a count, then rows of u32 fields and byte arrays) is 4-byte aligned.
            let mut table = vec![0u32; (size as usize).div_ceil(4) + 1];
            let mut bytes = u32::try_from(table.len() * 4).ok()?;
            // SAFETY: `table` holds `bytes` writable bytes.
            let status = unsafe { GetExtendedTcpTable(table.as_mut_ptr().cast(), &mut bytes, 0, u32::from(family), class, 0) };
            if status == ERROR_INSUFFICIENT_BUFFER {
                size = bytes;
                continue;
            }
            if status != NO_ERROR {
                return None;
            }
            let count = *table.first()? as usize;
            let (rows, _) = table.get(1..)?.as_chunks::<N>();
            return Some(rows.iter().take(count).filter_map(&pick).collect());
        }
        None
    }

    /// The account (a `TOKEN_USER`, kept in an aligned buffer) running `process`.
    unsafe fn user_of(process: HANDLE) -> Option<Vec<u64>> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `process` is valid for the call; the token handle is closed below.
        unsafe {
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut needed = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
            let ok = GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed);
            CloseHandle(token);
            (ok != 0 && needed as usize >= size_of::<TOKEN_USER>()).then_some(buffer)
        }
    }

    fn sid(user: &[u64]) -> *mut core::ffi::c_void {
        // SAFETY: filled by GetTokenInformation(TokenUser): a TOKEN_USER at its start.
        unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub fn listeners_are_mine(_: u16) -> bool {
        true
    }

    pub fn peer_is_mine(_: std::net::SocketAddr, _: u16) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_exactly() {
        let token = "ab".repeat(32);
        assert!(token_matches(&token, Some(token.as_bytes())));
        assert!(!token_matches(&token, Some("ab".repeat(31).as_bytes())));
        assert!(!token_matches(&token, Some(format!("{}ac", "ab".repeat(31)).as_bytes())));
        assert!(!token_matches(&token, None));
    }

    #[test]
    fn our_own_listener_is_ours() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(listeners_are_mine(listener.local_addr().unwrap().port()));
    }

    /// A connection this process makes to itself is found, and is ours; one from a port nobody
    /// connects from is not (it is not in the table).
    #[test]
    fn our_own_connection_is_ours() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server = listener.local_addr().unwrap().port();
        let _client = std::net::TcpStream::connect(("127.0.0.1", server)).unwrap();
        let (_accepted, peer) = listener.accept().unwrap();
        assert!(peer_is_mine(peer, server), "{peer}");
        #[cfg(any(windows, target_os = "linux"))]
        assert!(!peer_is_mine(SocketAddr::from(([127, 0, 0, 1], 1)), server), "no such connection");
    }

    /// The RPC endpoint mapper listens on 135 as NETWORK SERVICE on every Windows: not ours.
    #[cfg(windows)]
    #[test]
    fn a_system_service_listener_is_not_ours() {
        assert!(!listeners_are_mine(135));
    }

    /// Our own IPv6 dual-stack listener is found in the IPv6 table (and is ours).
    #[cfg(windows)]
    #[test]
    fn ipv6_listeners_are_read() {
        let Ok(listener) = std::net::TcpListener::bind("[::]:0") else { return }; // no IPv6 here
        let port = listener.local_addr().unwrap().port();
        assert_eq!(imp::listening_pids(port), Some(vec![std::process::id()]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_the_owners_of_a_port() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:258E 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1
   1: 00000000:258E 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 12346 1
   2: 0100007F:258E 0100007F:D431 01 00000000:00000000 00:00000000 00000000     0        0 12347 1
   3: 0200007F:258E 00000000:0000 0A 00000000:00000000 00:00000000 00000000     7        0 12348 1
   4: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     9        0 12349 1";
        assert_eq!(imp::owners(table, 9614), [1000, 1001], "listeners on 127.0.0.1 and 0.0.0.0 only");
        let table6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:258E 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 22 1
   1: 00000000000000000000000001000000:258E 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1002        0 23 1
   2: 0000000000000000FFFF00000100007F:258E 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1003        0 24 1";
        assert_eq!(imp::owners(table6, 9614), [1001, 1003], "[::] (dual-stack) and ::ffff:127.0.0.1, not ::1");
    }

    /// The other end of a connection to the bridge (port 9614 = 258E) from 127.0.0.1:54321 (D431):
    /// the row whose local address is the peer, not the bridge's own side of the connection.
    #[cfg(target_os = "linux")]
    #[test]
    fn reads_the_owner_of_a_connection() {
        let peer = SocketAddr::from(([127, 0, 0, 1], 0xD431));
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:258E 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 1 1
   1: 0100007F:D431 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1002        0 2 1
   2: 0100007F:D431 0100007F:258E 06 00000000:00000000 00:00000000 00000000     0        0 3 1
   3: 0100007F:D431 0100007F:258E 01 00000000:00000000 00:00000000 00000000  1001        0 4 1";
        assert_eq!(imp::peer_owner(table, peer, 9614), Some(1001), "the client's side, not in TIME_WAIT");
        let table6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0000000000000000FFFF00000100007F:D431 0000000000000000FFFF00000100007F:258E 01 00000000:00000000 00:00000000 00000000  1003        0 5 1";
        assert_eq!(imp::peer_owner(table6, peer, 9614), Some(1003), "an IPv6 socket, 127.0.0.1 mapped");
        assert_eq!(imp::peer_owner(table6, SocketAddr::from(([127, 0, 0, 1], 1)), 9614), None);
    }

    /// Another program on `[::]` (IPv6, dual-stack) receives 127.0.0.1's connections: it counts.
    #[test]
    fn a_dual_stack_listener_is_seen() {
        let Ok(listener) = std::net::TcpListener::bind("[::]:0") else { return }; // no IPv6 here
        assert!(listeners_are_mine(listener.local_addr().unwrap().port()), "ours, and found without panicking");
    }
}
