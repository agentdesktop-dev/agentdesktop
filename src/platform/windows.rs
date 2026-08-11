use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::windows::io::AsRawSocket;

use anyhow::{Context, Result, bail};
use tokio::net::TcpStream;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, SIO_QUERY_WFP_CONNECTION_REDIRECT_CONTEXT, SOCKET_ERROR, WSAGetLastError,
    WSAIoctl,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::session::windows::UserSid;

const MAGIC: &[u8; 4] = b"AGWF";
const VERSION: u16 = 1;
const HEADER_LEN: usize = 16;
const MAX_CONTEXT_LEN: usize = 256;
const SOCKADDR_IN_LEN: usize = 16;
const SOCKADDR_IN6_LEN: usize = 28;
const FLOW_NATIVE: u16 = 1;
const FLOW_CAPTURED: u16 = 2;
const AGWFP_IOCTL_SET_CONFIGURATION: u32 = (0x12 << 16) | (2 << 14) | ((0x800 + 1) << 2);

#[repr(C)]
struct WfpEndpoint {
    family: u16,
    port: u16,
    address: [u8; 16],
    scope_id: u32,
    reserved: [u8; 8],
}

#[repr(C)]
struct WfpConfiguration {
    version: u32,
    size: u32,
    live_service_pid: u32,
    flags: u32,
    public_destination: WfpEndpoint,
    proxy_destination: WfpEndpoint,
}

#[derive(Debug, Eq, PartialEq)]
pub enum FlowKind {
    Native,
    Captured,
}

#[derive(Debug, Eq, PartialEq)]
pub struct RedirectContext {
    pub flow_kind: FlowKind,
    pub original_destination: SocketAddr,
    pub user_sid: UserSid,
}

pub fn configure_native_redirect(
    public_destination: SocketAddr,
    proxy_destination: SocketAddr,
) -> Result<()> {
    let config = WfpConfiguration {
        version: VERSION.into(),
        size: size_of::<WfpConfiguration>() as u32,
        live_service_pid: std::process::id(),
        flags: 0,
        public_destination: WfpEndpoint::from_socket_addr(public_destination),
        proxy_destination: WfpEndpoint::from_socket_addr(proxy_destination),
    };
    if config.public_destination.family != config.proxy_destination.family {
        bail!("public and WFP proxy listeners must use the same address family");
    }
    let path = r"\\.\AGWfp"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_GENERIC_WRITE,
            FILE_SHARE_NONE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("open Agent Desktop WFP driver");
    }
    let handle = DriverHandle(handle);
    let mut returned = 0_u32;
    let result = unsafe {
        DeviceIoControl(
            handle.0,
            AGWFP_IOCTL_SET_CONFIGURATION,
            (&config as *const WfpConfiguration).cast(),
            size_of::<WfpConfiguration>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("configure Agent Desktop WFP driver");
    }
    Ok(())
}

impl WfpEndpoint {
    fn from_socket_addr(address: SocketAddr) -> Self {
        let (family, bytes, scope_id) = match address {
            SocketAddr::V4(address) => {
                let mut bytes = [0_u8; 16];
                bytes[..4].copy_from_slice(&u32::from(*address.ip()).to_ne_bytes());
                (AF_INET, bytes, 0)
            }
            SocketAddr::V6(address) => (AF_INET6, address.ip().octets(), address.scope_id()),
        };
        Self {
            family,
            port: address.port(),
            address: bytes,
            scope_id,
            reserved: [0; 8],
        }
    }
}

struct DriverHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for DriverHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

pub fn redirect_context(stream: &TcpStream) -> Result<RedirectContext> {
    let mut bytes = [0_u8; MAX_CONTEXT_LEN];
    let mut returned = 0_u32;
    let result = unsafe {
        WSAIoctl(
            stream.as_raw_socket() as usize,
            SIO_QUERY_WFP_CONNECTION_REDIRECT_CONTEXT,
            std::ptr::null(),
            0,
            bytes.as_mut_ptr().cast(),
            bytes.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if result == SOCKET_ERROR {
        return Err(std::io::Error::from_raw_os_error(unsafe {
            WSAGetLastError()
        }))
        .context("query WFP redirect context");
    }
    parse_redirect_context(&bytes[..returned as usize])
}

fn parse_redirect_context(bytes: &[u8]) -> Result<RedirectContext> {
    if bytes.len() < HEADER_LEN || &bytes[..4] != MAGIC {
        bail!("WFP redirect context has an invalid header");
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        bail!("unsupported WFP redirect context version {version}");
    }
    let flow_kind = match u16::from_le_bytes([bytes[6], bytes[7]]) {
        FLOW_NATIVE => FlowKind::Native,
        FLOW_CAPTURED => FlowKind::Captured,
        value => bail!("WFP redirect context has invalid flow kind {value}"),
    };
    if bytes[12..16] != [0; 4] {
        bail!("WFP redirect context reserved field is not zero");
    }
    let sockaddr_len = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
    let sid_len = u16::from_le_bytes([bytes[10], bytes[11]]) as usize;
    let expected_len = HEADER_LEN
        .checked_add(sockaddr_len)
        .and_then(|length| length.checked_add(sid_len))
        .context("WFP redirect context length overflow")?;
    if bytes.len() != expected_len || sid_len == 0 {
        bail!("WFP redirect context has invalid component lengths");
    }
    let sockaddr = &bytes[HEADER_LEN..HEADER_LEN + sockaddr_len];
    let user_sid = UserSid::from_bytes(bytes[HEADER_LEN + sockaddr_len..].to_vec())?;
    Ok(RedirectContext {
        flow_kind,
        original_destination: parse_sockaddr(sockaddr)?,
        user_sid,
    })
}

fn parse_sockaddr(bytes: &[u8]) -> Result<SocketAddr> {
    if bytes.len() < 2 {
        bail!("WFP redirect context has a truncated socket address");
    }
    match u16::from_ne_bytes([bytes[0], bytes[1]]) {
        AF_INET if bytes.len() == SOCKADDR_IN_LEN => Ok(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]),
            u16::from_be_bytes([bytes[2], bytes[3]]),
        ))),
        AF_INET6 if bytes.len() == SOCKADDR_IN6_LEN => {
            let mut address = [0_u8; 16];
            address.copy_from_slice(&bytes[8..24]);
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(address),
                u16::from_be_bytes([bytes[2], bytes[3]]),
                u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
                u32::from_ne_bytes(bytes[24..28].try_into().unwrap()),
            )))
        }
        family => bail!("WFP redirect context has unsupported address family {family}"),
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use super::{
        AGWFP_IOCTL_SET_CONFIGURATION, HEADER_LEN, MAGIC, VERSION, WfpConfiguration, WfpEndpoint,
        parse_redirect_context,
    };

    #[test]
    fn controller_abi_matches_driver_layout() {
        assert_eq!(AGWFP_IOCTL_SET_CONFIGURATION, 0x0012_a004);
        assert_eq!(size_of::<WfpEndpoint>(), 32);
        assert_eq!(size_of::<WfpConfiguration>(), 80);

        let endpoint = WfpEndpoint::from_socket_addr(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            8080,
        )));
        assert_eq!(
            endpoint.family,
            windows_sys::Win32::Networking::WinSock::AF_INET
        );
        assert_eq!(endpoint.port, 8080);
        assert_eq!(
            u32::from_ne_bytes(endpoint.address[..4].try_into().unwrap()),
            0x7f00_0001
        );
    }

    #[test]
    fn parses_ipv4_context_with_binary_sid() {
        let sid = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
        let mut sockaddr = [0_u8; 16];
        sockaddr[..2].copy_from_slice(
            &(windows_sys::Win32::Networking::WinSock::AF_INET as u16).to_ne_bytes(),
        );
        sockaddr[2..4].copy_from_slice(&15001_u16.to_be_bytes());
        sockaddr[4..8].copy_from_slice(&[127, 0, 0, 1]);
        let mut context = Vec::with_capacity(HEADER_LEN + sockaddr.len() + sid.len());
        context.extend_from_slice(MAGIC);
        context.extend_from_slice(&VERSION.to_le_bytes());
        context.extend_from_slice(&super::FLOW_NATIVE.to_le_bytes());
        context.extend_from_slice(&(sockaddr.len() as u16).to_le_bytes());
        context.extend_from_slice(&(sid.len() as u16).to_le_bytes());
        context.extend_from_slice(&0_u32.to_le_bytes());
        context.extend_from_slice(&sockaddr);
        context.extend_from_slice(&sid);

        let parsed = parse_redirect_context(&context).unwrap();

        assert_eq!(parsed.flow_kind, super::FlowKind::Native);
        assert_eq!(parsed.original_destination.to_string(), "127.0.0.1:15001");
    }

    #[test]
    fn rejects_trailing_or_truncated_context_data() {
        let mut context = Vec::from(*MAGIC);
        context.extend_from_slice(&VERSION.to_le_bytes());
        context.extend_from_slice(&super::FLOW_NATIVE.to_le_bytes());
        context.extend_from_slice(&16_u16.to_le_bytes());
        context.extend_from_slice(&12_u16.to_le_bytes());
        context.extend_from_slice(&0_u32.to_le_bytes());

        assert!(parse_redirect_context(&context).is_err());
    }
}
