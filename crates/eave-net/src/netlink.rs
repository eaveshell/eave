//! Blocking netlink sockets, on rustix. The packet crates only build and parse messages.

use std::fmt::Debug;
use std::io;

use netlink_packet_core::{
    NLM_F_ACK, NLM_F_DUMP, NLM_F_REQUEST, NetlinkBuffer, NetlinkDeserializable, NetlinkHeader,
    NetlinkMessage, NetlinkPayload, NetlinkSerializable,
};
use rustix::fd::OwnedFd;
use rustix::net::{self, AddressFamily, Protocol, SocketFlags, SocketType, netlink};

/// A netlink socket of `protocol` (`None` is `NETLINK_ROUTE`), joined to the multicast `groups` (a bitmask, 0 for none).
pub fn open(protocol: Option<Protocol>, groups: u32) -> io::Result<OwnedFd> {
    let socket = net::socket_with(
        AddressFamily::NETLINK,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        protocol,
    )?;
    net::bind(&socket, &netlink::SocketAddrNetlink::new(0, groups))?;
    Ok(socket)
}

/// A request socket: sends one request at a time and reads its replies.
pub struct Requests {
    socket: OwnedFd,
    sequence: u32,
    buffer: Vec<u8>,
}

impl Requests {
    pub fn new(protocol: Option<Protocol>) -> io::Result<Self> {
        let socket = open(protocol, 0)?;
        // The kernel answers at once; a reply that never comes mustn't hold the thread.
        net::sockopt::set_socket_timeout(
            &socket,
            net::sockopt::Timeout::Recv,
            Some(std::time::Duration::from_secs(5)),
        )?;
        Ok(Self {
            socket,
            sequence: 0,
            // Dump replies come in chunks of up to a page or so each; this fits any of them.
            buffer: vec![0; 32 * 1024],
        })
    }

    /// Every reply to a dump request.
    pub fn dump<T>(&mut self, message_type: u16, payload: T) -> io::Result<Vec<T>>
    where
        T: NetlinkSerializable + NetlinkDeserializable + Debug,
    {
        self.request(message_type, NLM_F_REQUEST | NLM_F_DUMP, payload)
    }

    /// The reply to a request for one object.
    pub fn get<T>(&mut self, message_type: u16, payload: T) -> io::Result<Option<T>>
    where
        T: NetlinkSerializable + NetlinkDeserializable + Debug,
    {
        let replies = self.request(message_type, NLM_F_REQUEST | NLM_F_ACK, payload)?;
        Ok(replies.into_iter().next())
    }

    fn request<T>(&mut self, message_type: u16, flags: u16, payload: T) -> io::Result<Vec<T>>
    where
        T: NetlinkSerializable + NetlinkDeserializable + Debug,
    {
        self.sequence = self.sequence.wrapping_add(1);
        let mut header = NetlinkHeader::default();
        header.message_type = message_type;
        header.flags = flags;
        header.sequence_number = self.sequence;
        let mut message = NetlinkMessage::new(header, NetlinkPayload::InnerMessage(payload));
        message.finalize();
        // `finalize` takes the type from the payload; generic netlink needs the resolved one.
        message.header.message_type = message_type;
        let mut out = vec![0; message.buffer_len()];
        message.serialize(&mut out);
        net::sendto(
            &self.socket,
            &out,
            net::SendFlags::empty(),
            &netlink::SocketAddrNetlink::new(0, 0),
        )?;

        let mut replies = Vec::new();
        loop {
            let (length, _) = net::recv(&self.socket, &mut self.buffer, net::RecvFlags::empty())?;
            let mut rest = &self.buffer[..length];
            while !rest.is_empty() {
                let size = NetlinkBuffer::new_checked(rest)
                    .map_err(io::Error::other)?
                    .length() as usize;
                let (bytes, tail) = rest.split_at(size.min(rest.len()));
                rest = &tail[tail.len().min(padding(size))..];
                let reply = NetlinkMessage::<T>::deserialize(bytes).map_err(io::Error::other)?;
                if reply.header.sequence_number != self.sequence {
                    continue;
                }
                match reply.payload {
                    NetlinkPayload::InnerMessage(inner) => replies.push(inner),
                    NetlinkPayload::Error(error) => {
                        return match error.code {
                            // A zero code is the ACK that ends a non-dump request.
                            None => Ok(replies),
                            Some(_) => Err(error.to_io()),
                        };
                    }
                    NetlinkPayload::Done(_) => return Ok(replies),
                    _ => {}
                }
            }
        }
    }
}

/// Messages are 4-byte aligned; the gap after one that isn't.
fn padding(size: usize) -> usize {
    (4 - size % 4) % 4
}
