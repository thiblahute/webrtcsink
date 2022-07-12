/// The default protocol used by the signalling server
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "peerType")]
#[serde(rename_all = "camelCase")]
/// Confirms registration
pub enum RegisteredMessage {
    /// Registered as a producer
    #[serde(rename_all = "camelCase")]
    Producer {
        peer_id: String,
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
    /// Registered as a consumer
    #[serde(rename_all = "camelCase")]
    Consumer {
        peer_id: String,
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
    /// Registered as a listener
    #[serde(rename_all = "camelCase")]
    Listener {
        peer_id: String,
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "peerType")]
#[serde(rename_all = "camelCase")]
/// Confirms registration
pub enum UnregisteredMessage {
    /// Unregistered as a producer
    #[serde(rename_all = "camelCase")]
    Producer {
        peer_id: String,
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
    /// Unregistered as a consumer
    #[serde(rename_all = "camelCase")]
    Consumer {
        peer_id: String,
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
    /// Unregistered as a listener
    #[serde(rename_all = "camelCase")]
    Listener {
        peer_id: String,
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "peerType")]
#[serde(rename_all = "camelCase")]
pub enum Peer {
    /// Unregistered as a producer
    #[serde(rename_all = "camelCase")]
    Producer {
        id: String,
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
    /// Unregistered as a consumer
    #[serde(rename_all = "camelCase")]
    Consumer {
        id: String,
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
/// Messages sent from the server to peers
pub enum OutgoingMessage {
    /// Confirms registration
    Registered(RegisteredMessage),
    /// Confirms registration
    Unregistered(UnregisteredMessage),
    /// Notifies listeners that a producer was registered
    #[serde(rename_all = "camelCase")]
    ProducerAdded {
        peer_id: String,
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
    /// Notifies listeners that a producer was removed
    #[serde(rename_all = "camelCase")]
    ProducerRemoved {
        peer_id: String,
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
    /// Instructs a peer to generate an offer
    #[serde(rename_all = "camelCase")]
    StartSession { peer_id: String },
    /// Signals that the session the peer was in was ended
    #[serde(rename_all = "camelCase")]
    EndSession(EndSessionMessage),
    /// Messages directly forwarded from one peer to another
    Peer(PeerMessage),
    /// Provides the current list of consumer peers
    List { producers: Vec<Peer> },
    /// Notifies that an error occurred with the peer's current session
    Error { details: String },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "peerType")]
#[serde(rename_all = "camelCase")]
/// Register with a peer type
pub enum RegisterMessage {
    /// Register as a producer
    #[serde(rename_all = "camelCase")]
    Producer {
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
    /// Register as a consumer
    #[serde(rename_all = "camelCase")]
    Consumer {
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
    /// Register as a listener
    #[serde(rename_all = "camelCase")]
    Listener {
        #[serde(default)]
        meta: Option<serde_json::Value>,
    },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "peerType")]
#[serde(rename_all = "camelCase")]
/// Register with a peer type
pub enum UnregisterMessage {
    /// Unregister a producer
    #[serde(rename_all = "camelCase")]
    Producer,
    /// Unregister a consumer
    #[serde(rename_all = "camelCase")]
    Consumer,
    /// Unregister a listener
    #[serde(rename_all = "camelCase")]
    Listener,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
/// Ask the server to start a session with a producer peer
pub struct StartSessionMessage {
    /// Identifies the peer
    pub peer_id: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
/// Contents of the peer message
pub enum PeerMessageInner {
    /// Conveys an ICE candidate
    #[serde(rename_all = "camelCase")]
    Ice {
        /// The candidate string
        candidate: String,
        /// The mline index the candidate applies to
        sdp_m_line_index: u32,
    },
    Sdp(SdpMessage),
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PeerMessageInfo {
    pub peer_id: String,
    #[serde(flatten)]
    pub peer_message: PeerMessageInner,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "senderType")]
#[serde(rename_all = "camelCase")]
/// Messages directly forwarded from one peer to another
pub enum PeerMessage {
    Producer(PeerMessageInfo),
    Consumer(PeerMessageInfo),
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "senderType")]
#[serde(rename_all = "camelCase")]
/// End a session
pub enum EndSessionMessage {
    #[serde(rename_all = "camelCase")]
    Consumer{peer_id: String},
    #[serde(rename_all = "camelCase")]
    Producer{peer_id: String},
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
/// Messages received by the server from peers
pub enum IncomingMessage {
    /// Register as a peer type
    Register(RegisterMessage),
    /// Unregister as a peer type
    Unregister(UnregisterMessage),
    /// Start a session with a producer peer
    StartSession(StartSessionMessage),
    /// End an existing session
    EndSession(EndSessionMessage),
    /// Send a message to a peer the sender is currently in session with
    Peer(PeerMessage),
    /// Retrieve the current list of producers
    List,
}
