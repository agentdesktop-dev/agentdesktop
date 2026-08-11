use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::windows::io::AsRawSocket;

use anyhow::{Context, Result, bail};
use tokio::net::TcpStream;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, SIO_QUERY_WFP_CONNECTION_REDIRECT_CONTEXT, SOCKET_ERROR, WSAGetLastError,
    WSAIoctl,
};

use crate::session::windows::UserSid;

const MAGIC: &[u8; 4] = b"AGWF";
const VERSION: u16 = 1;
const HEADER_LEN: usize = 16;
const MAX_CONTEXT_LEN: usize = 256;
const SOCKADDR_IN_LEN: usize = 16;
const SOCKADDR_IN6_LEN: usize = 28;
const FLOW_NATIVE: u16 = 1;
const FLOW_CAPTURED: u16 = 2;

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
    use super::{HEADER_LEN, MAGIC, VERSION, parse_redirect_context};

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
