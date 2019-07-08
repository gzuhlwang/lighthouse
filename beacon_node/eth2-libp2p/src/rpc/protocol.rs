use super::methods::*;
use libp2p::core::{upgrade, InboundUpgrade, OutboundUpgrade, UpgradeInfo};
use ssz::{impl_decode_via_from, impl_encode_via_from, ssz_encode, Decode, Encode};
use ssz_derive::{Decode, Encode};
use std::hash::{Hash, Hasher};
use std::io;
use std::iter;
use tokio::io::{AsyncRead, AsyncWrite};

/// The maximum bytes that can be sent across the RPC.
const MAX_READ_SIZE: usize = 4_194_304; // 4M

/// Implementation of the `ConnectionUpgrade` for the rpc protocol.
#[derive(Debug, Clone)]
pub struct RPCProtocol;

impl UpgradeInfo for RPCProtocol {
    type Info = &'static [u8];
    type InfoIter = iter::Once<Self::Info>;

    #[inline]
    fn protocol_info(&self) -> Self::InfoIter {
        iter::once(b"/eth/serenity/rpc/1.0.0")
    }
}

impl Default for RPCProtocol {
    fn default() -> Self {
        RPCProtocol
    }
}

/// A monotonic counter for ordering `RPCRequest`s.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestId(u64);

impl RequestId {
    /// Increment the request id.
    pub fn increment(&mut self) {
        self.0 += 1
    }

    /// Return the previous id.
    pub fn previous(self) -> Self {
        Self(self.0 - 1)
    }
}

impl Eq for RequestId {}

impl PartialEq for RequestId {
    fn eq(&self, other: &RequestId) -> bool {
        self.0 == other.0
    }
}

impl Hash for RequestId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl From<u64> for RequestId {
    fn from(x: u64) -> RequestId {
        RequestId(x)
    }
}

impl Into<u64> for RequestId {
    fn into(self) -> u64 {
        self.0
    }
}

impl_encode_via_from!(RequestId, u64);
impl_decode_via_from!(RequestId, u64);

/// The RPC types which are sent/received in this protocol.
#[derive(Debug, Clone)]
pub enum RPCEvent {
    Request {
        id: RequestId,
        method_id: u16,
        body: RPCRequest,
    },
    Response {
        id: RequestId,
        method_id: u16, //TODO: Remove and process decoding upstream
        result: RPCResponse,
    },
}

impl UpgradeInfo for RPCEvent {
    type Info = &'static [u8];
    type InfoIter = iter::Once<Self::Info>;

    #[inline]
    fn protocol_info(&self) -> Self::InfoIter {
        iter::once(b"/eth/serenity/rpc/1.0.0")
    }
}

type FnDecodeRPCEvent = fn(Vec<u8>, ()) -> Result<RPCEvent, DecodeError>;

impl<TSocket> InboundUpgrade<TSocket> for RPCProtocol
where
    TSocket: AsyncRead + AsyncWrite,
{
    type Output = RPCEvent;
    type Error = DecodeError;
    type Future = upgrade::ReadOneThen<upgrade::Negotiated<TSocket>, (), FnDecodeRPCEvent>;

    fn upgrade_inbound(self, socket: upgrade::Negotiated<TSocket>, _: Self::Info) -> Self::Future {
        upgrade::read_one_then(socket, MAX_READ_SIZE, (), |packet, ()| Ok(decode(packet)?))
    }
}

/// A helper structed used to obtain SSZ serialization for RPC messages.
#[derive(Encode, Decode, Default)]
struct SszContainer {
    /// Note: the `is_request` field is not included in the spec.
    ///
    /// We are unable to determine a request from a response unless we add some flag to the
    /// packet. Here we have added a bool (encoded as 1 byte) which is set to `1` if the
    /// message is a request.
    is_request: bool,
    id: u64,
    other: u16,
    bytes: Vec<u8>,
}

fn decode(packet: Vec<u8>) -> Result<RPCEvent, DecodeError> {
    let msg = SszContainer::from_ssz_bytes(&packet)?;

    if msg.is_request {
        let body = match RPCMethod::from(msg.other) {
            RPCMethod::Hello => RPCRequest::Hello(HelloMessage::from_ssz_bytes(&msg.bytes)?),
            RPCMethod::Goodbye => RPCRequest::Goodbye(GoodbyeReason::from_ssz_bytes(&msg.bytes)?),
            RPCMethod::BeaconBlockRoots => {
                RPCRequest::BeaconBlockRoots(BeaconBlockRootsRequest::from_ssz_bytes(&msg.bytes)?)
            }
            RPCMethod::BeaconBlockHeaders => RPCRequest::BeaconBlockHeaders(
                BeaconBlockHeadersRequest::from_ssz_bytes(&msg.bytes)?,
            ),
            RPCMethod::BeaconBlockBodies => {
                RPCRequest::BeaconBlockBodies(BeaconBlockBodiesRequest::from_ssz_bytes(&msg.bytes)?)
            }
            RPCMethod::BeaconChainState => {
                RPCRequest::BeaconChainState(BeaconChainStateRequest::from_ssz_bytes(&msg.bytes)?)
            }
            RPCMethod::Unknown => return Err(DecodeError::UnknownRPCMethod),
        };

        Ok(RPCEvent::Request {
            id: RequestId::from(msg.id),
            method_id: msg.other,
            body,
        })
    }
    // we have received a response
    else {
        let result = match RPCMethod::from(msg.other) {
            RPCMethod::Hello => RPCResponse::Hello(HelloMessage::from_ssz_bytes(&msg.bytes)?),
            RPCMethod::BeaconBlockRoots => {
                RPCResponse::BeaconBlockRoots(BeaconBlockRootsResponse::from_ssz_bytes(&msg.bytes)?)
            }
            RPCMethod::BeaconBlockHeaders => RPCResponse::BeaconBlockHeaders(
                BeaconBlockHeadersResponse::from_ssz_bytes(&msg.bytes)?,
            ),
            RPCMethod::BeaconBlockBodies => RPCResponse::BeaconBlockBodies(
                BeaconBlockBodiesResponse::from_ssz_bytes(&msg.bytes)?,
            ),
            RPCMethod::BeaconChainState => {
                RPCResponse::BeaconChainState(BeaconChainStateResponse::from_ssz_bytes(&msg.bytes)?)
            }
            // We should never receive a goodbye response; it is invalid.
            RPCMethod::Goodbye => return Err(DecodeError::UnknownRPCMethod),
            RPCMethod::Unknown => return Err(DecodeError::UnknownRPCMethod),
        };

        Ok(RPCEvent::Response {
            id: RequestId::from(msg.id),
            method_id: msg.other,
            result,
        })
    }
}

impl<TSocket> OutboundUpgrade<TSocket> for RPCEvent
where
    TSocket: AsyncWrite,
{
    type Output = ();
    type Error = io::Error;
    type Future = upgrade::WriteOne<upgrade::Negotiated<TSocket>>;

    #[inline]
    fn upgrade_outbound(self, socket: upgrade::Negotiated<TSocket>, _: Self::Info) -> Self::Future {
        let bytes = ssz_encode(&self);
        upgrade::write_one(socket, bytes)
    }
}

impl Encode for RPCEvent {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        let container = match self {
            RPCEvent::Request {
                id,
                method_id,
                body,
            } => SszContainer {
                is_request: true,
                id: (*id).into(),
                other: *method_id,
                bytes: match body {
                    RPCRequest::Hello(body) => body.as_ssz_bytes(),
                    RPCRequest::Goodbye(body) => body.as_ssz_bytes(),
                    RPCRequest::BeaconBlockRoots(body) => body.as_ssz_bytes(),
                    RPCRequest::BeaconBlockHeaders(body) => body.as_ssz_bytes(),
                    RPCRequest::BeaconBlockBodies(body) => body.as_ssz_bytes(),
                    RPCRequest::BeaconChainState(body) => body.as_ssz_bytes(),
                },
            },
            RPCEvent::Response {
                id,
                method_id,
                result,
            } => SszContainer {
                is_request: false,
                id: (*id).into(),
                other: *method_id,
                bytes: match result {
                    RPCResponse::Hello(response) => response.as_ssz_bytes(),
                    RPCResponse::BeaconBlockRoots(response) => response.as_ssz_bytes(),
                    RPCResponse::BeaconBlockHeaders(response) => response.as_ssz_bytes(),
                    RPCResponse::BeaconBlockBodies(response) => response.as_ssz_bytes(),
                    RPCResponse::BeaconChainState(response) => response.as_ssz_bytes(),
                },
            },
        };

        container.ssz_append(buf)
    }
}

#[derive(Debug)]
pub enum DecodeError {
    ReadError(upgrade::ReadOneError),
    SSZDecodeError(ssz::DecodeError),
    UnknownRPCMethod,
}

impl From<upgrade::ReadOneError> for DecodeError {
    #[inline]
    fn from(err: upgrade::ReadOneError) -> Self {
        DecodeError::ReadError(err)
    }
}

impl From<ssz::DecodeError> for DecodeError {
    #[inline]
    fn from(err: ssz::DecodeError) -> Self {
        DecodeError::SSZDecodeError(err)
    }
}
