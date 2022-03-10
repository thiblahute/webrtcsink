/// The default protocol used by the signalling server
use serde_derive::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "peer-type")]
#[serde(rename_all = "lowercase")]
/// Confirms registration
pub enum RegisteredMessage {
    /// Registered as a producer
    Producer { id: String },
    /// Registered as a consumer
    Consumer { id: String },
    /// Registered as a listener
    Listener { id: String },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
/// Messages sent from the server to peers
pub enum OutgoingMessage {
    /// Confirms registration
    Registered(RegisteredMessage),
    /// Notifies listeners that a producer was registered
    ProducerAdded { peer_id: String },
    /// Notifies listeners that a producer was removed
    ProducerRemoved { peer_id: String },
    /// Instructs a peer to generate an offer
    StartSession { peer_id: String },
    /// Signals that the session the peer was in was ended
    EndSession { peer_id: String },
    /// Messages directly forwarded from one peer to another
    Peer(PeerMessage),
    /// Provides the current list of consumer peers
    List { producers: Vec<String> },
    /// Notifies that an error occurred with the peer's current session
    Error { details: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "peer-type")]
#[serde(rename_all = "lowercase")]
/// Register with a peer type
pub enum RegisterMessage {
    /// Register as a producer
    Producer,
    /// Register as a consumer
    Consumer,
    /// Register as a listener
    Listener,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
/// Ask the server to start a session with a producer peer
pub struct StartSessionMessage {
    /// Identifies the peer
    pub peer_id: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
/// Conveys a SDP
pub enum SdpMessage {
    /// Conveys an offer
    Offer {
        /// The SDP
        sdp: String,
    },
    /// Conveys an answer
    Answer {
        /// The SDP
        sdp: String,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
/// Contents of the peer message
pub enum PeerMessageInner {
    /// Conveys an ICE candidate
    Ice {
        /// The candidate string
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        /// The mline index the candidate applies to
        sdp_mline_index: u32,
    },
    Sdp(SdpMessage),
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
/// Messages directly forwarded from one peer to another
pub struct PeerMessage {
    /// The identifier of the peer, which must be in a session with the sender
    pub peer_id: String,
    /// The contents of the message
    pub peer_message: PeerMessageInner,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
/// End a session
pub struct EndSessionMessage {
    /// The identifier of the peer to end the session with
    pub peer_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
/// Messages received by the server from peers
pub enum IncomingMessage {
    /// Register as a peer type
    Register(RegisterMessage),
    /// Start a session with a producer peer
    StartSession(StartSessionMessage),
    /// End an existing session
    EndSession(EndSessionMessage),
    /// Send a message to a peer the sender is currently in session with
    Peer(PeerMessage),
    /// Retrieve the current list of producers
    List,
}
