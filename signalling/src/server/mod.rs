use anyhow::Error;
use async_std::net::{TcpListener, TcpStream};
use async_std::task;
use async_tungstenite::tungstenite::Message as WsMessage;
use futures::channel::mpsc;
use futures::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{info, trace, warn};

use crate::handlers::{MessageHandler, MessageSender};
use crate::protocol as p;

#[derive(Clone)]
/// The extendable signalling server
pub struct SignallingServer {
    state: Arc<Mutex<State>>,
}

struct Peer {
    receive_task_handle: task::JoinHandle<()>,
    send_task_handle: task::JoinHandle<Result<(), Error>>,
    sender: mpsc::Sender<String>,
}

#[derive(Default)]
struct DefaultMessageSender {
    peers: HashMap<String, Peer>,
}

impl MessageSender for DefaultMessageSender {
    fn send_message(&mut self, message: String, peer_id: &str) {
        if let Some(peer) = self.peers.get_mut(peer_id) {
            let mut sender = peer.sender.clone();
            let peer_id = peer_id.to_string();
            task::spawn(async move {
                if let Err(err) = sender.send(message).await {
                    warn!(peer_id = %peer_id, "Failed to send message: {}", err);
                }
            });
        }
    }
}

struct State {
    message_handler: Box<dyn MessageHandler>,
    message_sender: Option<DefaultMessageSender>,
}

#[derive(thiserror::Error, Debug)]
pub enum SignallingServerError {}

impl SignallingServer {
    /// Instantiate the signalling server with a MessageHandler
    /// implementation. Use DefaultMessageHandler for the default
    /// protocol, or implement your own handler
    pub fn new(message_handler: Box<dyn MessageHandler>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                message_handler,
                message_sender: Some(DefaultMessageSender::default()),
            })),
        }
    }

    fn remove_peer(state: Arc<Mutex<State>>, peer_id: &str) {
        {
            let mut state = state.lock().unwrap();
            let mut message_sender = state.message_sender.take().unwrap();

            state
                .message_handler
                .remove_peer(&mut message_sender, peer_id);

            state.message_sender = Some(message_sender);
        }

        if let Some(mut peer) = state
            .lock()
            .unwrap()
            .message_sender
            .as_mut()
            .unwrap()
            .peers
            .remove(peer_id)
        {
            let peer_id = peer_id.to_string();
            task::spawn(async move {
                peer.sender.close_channel();
                if let Err(err) = peer.send_task_handle.await {
                    trace!(peer_id = %peer_id, "Error while joining send task: {}", err);
                }
                peer.receive_task_handle.await;
            });
        }
    }

    /// Can be used to inject messages constructed locally
    pub fn handle_message(&self, msg: String, peer_id: &str) {
        let mut state = self.state.lock().unwrap();
        let mut message_sender = state.message_sender.take().unwrap();

        if let Err(err) = state
            .message_handler
            .handle_message(&mut message_sender, msg, peer_id)
        {
            warn!(this = %peer_id, "Error handling message: {:?}", err);
            message_sender.send_message(
                serde_json::to_string(&p::OutgoingMessage::Error {
                    details: err.to_string(),
                })
                .unwrap(),
                peer_id,
            );
        }

        state.message_sender = Some(message_sender);
    }

    async fn accept_connection(state: Arc<Mutex<State>>, stream: TcpStream) {
        let addr = stream
            .peer_addr()
            .expect("connected streams should have a peer address");
        info!("Peer address: {}", addr);

        let ws = match async_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(err) => {
                warn!("Error during the websocket handshake: {}", err);
                return;
            }
        };

        let this_id = uuid::Uuid::new_v4().to_string();
        info!(this_id = %this_id, "New WebSocket connection: {}", addr);

        // 1000 is completely arbitrary, we simply don't want infinite piling
        // up of messages as with unbounded
        let (websocket_sender, mut websocket_receiver) = mpsc::channel::<String>(1000);

        let this_id_clone = this_id.clone();
        let (mut ws_sink, mut ws_stream) = ws.split();
        let send_task_handle = task::spawn(async move {
            while let Some(msg) = websocket_receiver.next().await {
                trace!(this_id = %this_id_clone, "sending {}", msg);
                ws_sink.send(WsMessage::Text(msg)).await?;
            }

            ws_sink.send(WsMessage::Close(None)).await?;
            ws_sink.close().await?;

            Ok::<(), Error>(())
        });

        {}

        let state_clone = state.clone();
        let this_id_clone = this_id.clone();
        let receive_task_handle = task::spawn(async move {
            while let Some(msg) = async_std::stream::StreamExt::next(&mut ws_stream).await {
                let mut state = state_clone.lock().unwrap();

                match msg {
                    Ok(WsMessage::Text(msg)) => {
                        let mut message_sender = state.message_sender.take().unwrap();

                        if let Err(err) = state.message_handler.handle_message(
                            &mut message_sender,
                            msg,
                            &this_id_clone,
                        ) {
                            warn!(this = %this_id_clone, "Error handling message: {:?}", err);
                            message_sender.send_message(
                                serde_json::to_string(&p::OutgoingMessage::Error {
                                    details: err.to_string(),
                                })
                                .unwrap(),
                                &this_id_clone,
                            );
                        }

                        state.message_sender = Some(message_sender);
                    }
                    Ok(WsMessage::Close(reason)) => {
                        info!(this_id = %this_id_clone, "connection closed: {:?}", reason);
                        break;
                    }
                    Ok(_) => warn!(this_id = %this_id_clone, "Unsupported message type"),
                    Err(err) => {
                        warn!(this_id = %this_id_clone, "recv error: {}", err);
                        break;
                    }
                }
            }

            Self::remove_peer(state_clone, &this_id_clone);
        });

        let mut state = state.lock().unwrap();

        state.message_sender.as_mut().unwrap().peers.insert(
            this_id,
            Peer {
                receive_task_handle,
                send_task_handle,
                sender: websocket_sender,
            },
        );
    }

    /// Run the server
    pub async fn run(&self, host: &str, port: u16) -> Result<(), SignallingServerError> {
        let addr = format!("{}:{}", host, port);

        // Create the event loop and TCP listener we'll accept connections on.
        let try_socket = TcpListener::bind(&addr).await;
        let listener = try_socket.expect("Failed to bind");
        info!("Listening on: {}", addr);

        let state_clone = self.state.clone();
        while let Ok((stream, _)) = listener.accept().await {
            task::spawn(Self::accept_connection(state_clone.clone(), stream));
        }

        Ok(())
    }
}