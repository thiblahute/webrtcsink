use crate::protocol as p;
use anyhow::{anyhow, Error};
use std::collections::{HashMap, HashSet};
use tracing::{debug, info, instrument, warn};

pub trait MessageHandler: Sync + Send {
    /// Handle a message, use the sender to reply or communicate
    /// with other peers
    fn handle_message(
        &mut self,
        sender: &mut dyn MessageSender,
        message: &str,
        peer_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>>;

    /// A connection was closed, remove the peer and clean up
    fn remove_peer(&mut self, sender: &mut dyn MessageSender, peer_id: &str);
}

pub trait MessageSender {
    /// Send a message to a peer
    fn send_message(&mut self, message: String, peer_id: &str);
}

#[derive(Default)]
/// Implementation of the default protocol
pub struct DefaultMessageHandler {
    producers: HashMap<String, HashSet<String>>,
    consumers: HashMap<String, Option<String>>,
    listeners: HashSet<String>,
}

impl DefaultMessageHandler {
    #[instrument(level = "debug", skip(self, sender))]
    /// End a session between two peers
    pub fn end_session(
        &mut self,
        sender: &mut dyn MessageSender,
        peer_id: &str,
        other_peer_id: &str,
    ) -> Result<(), Error> {
        info!(peer_id=%peer_id, other_peer_id=%other_peer_id, "endsession request");

        if let Some(ref mut consumers) = self.producers.get_mut(peer_id) {
            if consumers.remove(other_peer_id) {
                info!(producer_id=%peer_id, consumer_id=%other_peer_id, "ended session");

                sender.send_message(
                    serde_json::to_string(&p::OutgoingMessage::EndSession {
                        peer_id: peer_id.to_string(),
                    })
                    .unwrap(),
                    other_peer_id,
                );

                self.consumers.insert(other_peer_id.to_string(), None);

                Ok(())
            } else {
                Err(anyhow!(
                    "Producer {} has no consumer {}",
                    peer_id,
                    other_peer_id
                ))
            }
        } else if let Some(Some(producer_id)) = self.consumers.get(peer_id) {
            if producer_id == other_peer_id {
                info!(producer_id=%other_peer_id, consumer_id=%peer_id, "ended session");

                self.consumers.insert(peer_id.to_string(), None);
                self.producers
                    .get_mut(other_peer_id)
                    .unwrap()
                    .remove(peer_id);

                sender.send_message(
                    serde_json::to_string(&p::OutgoingMessage::EndSession {
                        peer_id: peer_id.to_string(),
                    })
                    .unwrap(),
                    other_peer_id,
                );

                Ok(())
            } else {
                Err(anyhow!(
                    "Consumer {} is not in a session with {}",
                    peer_id,
                    other_peer_id
                ))
            }
        } else {
            Err(anyhow!(
                "No session between {} and {}",
                peer_id,
                other_peer_id
            ))
        }
    }

    /// List producer peers
    #[instrument(level = "debug", skip(self, sender))]
    pub fn list_producers(
        &mut self,
        sender: &mut dyn MessageSender,
        peer_id: &str,
    ) -> Result<(), Error> {
        sender.send_message(
            serde_json::to_string(&p::OutgoingMessage::List {
                producers: self.producers.keys().map(|s| s.clone()).collect(),
            })
            .unwrap(),
            peer_id,
        );

        Ok(())
    }

    /// Handle ICE candidate sent by one peer to another peer
    #[instrument(level = "debug", skip(self, sender))]
    pub fn handle_ice(
        &mut self,
        sender: &mut dyn MessageSender,
        candidate: String,
        sdp_mline_index: u32,
        peer_id: &str,
        other_peer_id: &str,
    ) -> Result<(), Error> {
        if let Some(consumers) = self.producers.get(peer_id) {
            if consumers.contains(other_peer_id) {
                sender.send_message(
                    serde_json::to_string(&p::OutgoingMessage::Peer(p::PeerMessage {
                        peer_id: peer_id.to_string(),
                        peer_message: p::PeerMessageInner::Ice {
                            candidate,
                            sdp_mline_index,
                        },
                    }))
                    .unwrap(),
                    other_peer_id,
                );
                Ok(())
            } else {
                Err(anyhow!(
                    "cannot forward ICE from {} to {} as they are not in a session",
                    peer_id,
                    other_peer_id
                ))
            }
        } else if let Some(producer) = self.consumers.get(peer_id) {
            if &Some(other_peer_id.to_string()) == producer {
                sender.send_message(
                    serde_json::to_string(&p::OutgoingMessage::Peer(p::PeerMessage {
                        peer_id: peer_id.to_string(),
                        peer_message: p::PeerMessageInner::Ice {
                            candidate,
                            sdp_mline_index,
                        },
                    }))
                    .unwrap(),
                    other_peer_id,
                );

                Ok(())
            } else {
                Err(anyhow!(
                    "cannot forward ICE from {} to {} as they are not in a session",
                    peer_id,
                    other_peer_id
                ))
            }
        } else {
            Err(anyhow!(
                "cannot forward ICE from {} to {} as they are not in a session",
                peer_id,
                other_peer_id,
            ))
        }
    }

    /// Handle SDP offered by one peer to another peer
    #[instrument(level = "debug", skip(self, sender))]
    pub fn handle_sdp_offer(
        &mut self,
        sender: &mut dyn MessageSender,
        sdp: String,
        producer_id: &str,
        consumer_id: &str,
    ) -> Result<(), Error> {
        if let Some(consumers) = self.producers.get(producer_id) {
            if consumers.contains(consumer_id) {
                sender.send_message(
                    serde_json::to_string(&p::OutgoingMessage::Peer(p::PeerMessage {
                        peer_id: producer_id.to_string(),
                        peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer { sdp }),
                    }))
                    .unwrap(),
                    consumer_id,
                );
                Ok(())
            } else {
                Err(anyhow!(
                    "cannot forward offer from {} to {} as they are not in a session",
                    producer_id,
                    consumer_id
                ))
            }
        } else {
            Err(anyhow!(
                "cannot forward offer from {} to {} as they are not in a session or {} is not the producer",
                producer_id,
                consumer_id,
                producer_id,
            ))
        }
    }

    /// Handle the SDP answer from one peer to another peer
    #[instrument(level = "debug", skip(self, sender))]
    pub fn handle_sdp_answer(
        &mut self,
        sender: &mut dyn MessageSender,
        sdp: String,
        consumer_id: &str,
        producer_id: &str,
    ) -> Result<(), Error> {
        if let Some(producer) = self.consumers.get(consumer_id) {
            if &Some(producer_id.to_string()) == producer {
                sender.send_message(
                    serde_json::to_string(&p::OutgoingMessage::Peer(p::PeerMessage {
                        peer_id: consumer_id.to_string(),
                        peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Answer { sdp }),
                    }))
                    .unwrap(),
                    producer_id,
                );
                Ok(())
            } else {
                Err(anyhow!(
                    "cannot forward answer from {} to {} as they are not in a session",
                    consumer_id,
                    producer_id
                ))
            }
        } else {
            Err(anyhow!(
                "cannot forward answer from {} to {} as they are not in a session",
                consumer_id,
                producer_id
            ))
        }
    }

    /// Register peer as a producer
    #[instrument(level = "debug", skip(self, sender))]
    pub fn register_producer(
        &mut self,
        sender: &mut dyn MessageSender,
        peer_id: &str,
    ) -> Result<(), Error> {
        if self.producers.contains_key(peer_id) {
            Err(anyhow!("{} is already registered as a producer", peer_id))
        } else {
            self.producers.insert(peer_id.to_string(), HashSet::new());

            for listener in &self.listeners {
                sender.send_message(
                    serde_json::to_string(&p::OutgoingMessage::ProducerAdded {
                        peer_id: peer_id.to_string(),
                    })
                    .unwrap(),
                    &listener,
                );
            }

            sender.send_message(
                serde_json::to_string(&p::OutgoingMessage::Registered(
                    p::RegisteredMessage::Producer,
                ))
                .unwrap(),
                peer_id,
            );

            info!(peer_id = %peer_id, "registered as a producer");

            Ok(())
        }
    }

    /// Register peer as a consumer
    #[instrument(level = "debug", skip(self, sender))]
    pub fn register_consumer(
        &mut self,
        sender: &mut dyn MessageSender,
        peer_id: &str,
    ) -> Result<(), Error> {
        if self.consumers.contains_key(peer_id) {
            Err(anyhow!("{} is already registered as a consumer", peer_id))
        } else {
            self.consumers.insert(peer_id.to_string(), None);

            sender.send_message(
                serde_json::to_string(&p::OutgoingMessage::Registered(
                    p::RegisteredMessage::Consumer,
                ))
                .unwrap(),
                peer_id,
            );

            info!(peer_id = %peer_id, "registered as a consumer");

            Ok(())
        }
    }

    /// Register peer as a listener
    #[instrument(level = "debug", skip(self, sender))]
    pub fn register_listener(
        &mut self,
        sender: &mut dyn MessageSender,
        peer_id: &str,
    ) -> Result<(), Error> {
        if !self.listeners.insert(peer_id.to_string()) {
            Err(anyhow!("{} is already registered as a listener", peer_id))
        } else {
            sender.send_message(
                serde_json::to_string(&p::OutgoingMessage::Registered(
                    p::RegisteredMessage::Listener,
                ))
                .unwrap(),
                peer_id,
            );

            info!(peer_id = %peer_id, "registered as a listener");

            Ok(())
        }
    }

    /// Start a session between two peers
    #[instrument(level = "debug", skip(self, sender))]
    pub fn start_session(
        &mut self,
        sender: &mut dyn MessageSender,
        producer_id: &str,
        consumer_id: &str,
    ) -> Result<(), Error> {
        if !self.consumers.contains_key(consumer_id) {
            return Err(anyhow!(
                "Peer with id {} is not registered as a consumer",
                consumer_id
            ));
        }

        if let Some(producer_id) = self.consumers.get(consumer_id).unwrap() {
            return Err(anyhow!(
                "Consumer with id {} is already in a session with producer {}",
                consumer_id,
                producer_id,
            ));
        }

        if !self.producers.contains_key(producer_id) {
            return Err(anyhow!(
                "Peer with id {} is not registered as a producer",
                producer_id
            ));
        }

        self.consumers
            .insert(consumer_id.to_string(), Some(producer_id.to_string()));
        self.producers
            .get_mut(producer_id)
            .unwrap()
            .insert(consumer_id.to_string());

        sender.send_message(
            serde_json::to_string(&p::OutgoingMessage::StartSession {
                peer_id: consumer_id.to_string(),
            })
            .unwrap(),
            producer_id,
        );

        info!(producer_id = %producer_id, consumer_id = %consumer_id, "started a session");

        Ok(())
    }
}

impl MessageHandler for DefaultMessageHandler {
    #[instrument(level = "debug", skip(self, sender))]
    fn remove_peer(&mut self, sender: &mut dyn MessageSender, peer_id: &str) {
        info!(peer_id = %peer_id, "removing peer");

        self.listeners.remove(peer_id);

        if let Some(consumers) = self.producers.remove(peer_id) {
            for consumer_id in &consumers {
                info!(producer_id=%peer_id, consumer_id=%consumer_id, "ended session");
                self.consumers.insert(consumer_id.clone(), None);
                sender.send_message(
                    serde_json::to_string(&p::OutgoingMessage::EndSession {
                        peer_id: peer_id.to_string(),
                    })
                    .unwrap(),
                    &consumer_id,
                );
            }

            for listener in &self.listeners {
                sender.send_message(
                    serde_json::to_string(&p::OutgoingMessage::ProducerRemoved {
                        peer_id: peer_id.to_string(),
                    })
                    .unwrap(),
                    &listener,
                );
            }
        }

        if let Some(Some(producer_id)) = self.consumers.remove(peer_id) {
            info!(producer_id=%producer_id, consumer_id=%peer_id, "ended session");

            self.producers
                .get_mut(&producer_id)
                .unwrap()
                .remove(peer_id);

            sender.send_message(
                serde_json::to_string(&p::OutgoingMessage::EndSession {
                    peer_id: peer_id.to_string(),
                })
                .unwrap(),
                &producer_id,
            );
        }
    }

    #[instrument(level = "trace", skip(self, sender))]
    fn handle_message(
        &mut self,
        sender: &mut dyn MessageSender,
        message: &str,
        peer_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        debug!("Handling {}", message);

        if let Ok(message) = serde_json::from_str::<p::IncomingMessage>(message) {
            match message {
                p::IncomingMessage::Register(message) => match message {
                    p::RegisterMessage::Producer => self.register_producer(sender, peer_id),
                    p::RegisterMessage::Consumer => self.register_consumer(sender, peer_id),
                    p::RegisterMessage::Listener => self.register_listener(sender, peer_id),
                },
                p::IncomingMessage::StartSession(message) => {
                    self.start_session(sender, &message.peer_id, peer_id)
                }
                p::IncomingMessage::Peer(p::PeerMessage {
                    peer_id: other_peer_id,
                    peer_message,
                }) => match peer_message {
                    p::PeerMessageInner::Ice {
                        candidate,
                        sdp_mline_index,
                    } => self.handle_ice(
                        sender,
                        candidate,
                        sdp_mline_index,
                        &peer_id,
                        &other_peer_id,
                    ),
                    p::PeerMessageInner::Sdp(sdp_message) => match sdp_message {
                        p::SdpMessage::Offer { sdp } => {
                            self.handle_sdp_offer(sender, sdp, &peer_id, &other_peer_id)
                        }
                        p::SdpMessage::Answer { sdp } => {
                            self.handle_sdp_answer(sender, sdp, &peer_id, &other_peer_id)
                        }
                    },
                },
                p::IncomingMessage::List => self.list_producers(sender, peer_id),
                p::IncomingMessage::EndSession(p::EndSessionMessage {
                    peer_id: other_peer_id,
                }) => self.end_session(sender, &peer_id, &other_peer_id),
            }
            .map_err(|err| err.into())
        } else {
            Err(anyhow!("Unsupported message: {}", message).into())
        }
    }
}

#[cfg(test)]
mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;
    use std::collections::VecDeque;

    #[derive(Debug)]
    struct MockSentMessage {
        message: String,
        peer_id: String,
    }

    #[derive(Default)]
    struct MockMessageSender {
        messages: VecDeque<MockSentMessage>,
    }

    impl MessageSender for MockMessageSender {
        fn send_message(&mut self, message: String, peer_id: &str) {
            self.messages.push_back(MockSentMessage {
                message,
                peer_id: peer_id.to_string(),
            });
        }
    }

    #[test]
    fn test_register_producer() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "producer");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::Registered(p::RegisteredMessage::Producer)
        );
    }

    #[test]
    fn test_list_producers() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::List;

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "listener",
            )
            .unwrap();

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "listener");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::List {
                producers: vec!["producer".to_string()]
            }
        );
    }

    #[test]
    fn test_register_consumer() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "consumer");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::Registered(p::RegisteredMessage::Consumer)
        );
    }

    #[test]
    #[should_panic(expected = "already registered as a producer")]
    fn test_register_producer_twice() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();
    }

    #[test]
    fn test_listener() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Listener);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "listener",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "listener");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::ProducerAdded {
                peer_id: "producer".to_string()
            }
        );
    }

    #[test]
    fn test_start_session() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "producer");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::StartSession {
                peer_id: "consumer".to_string()
            }
        );
    }

    #[test]
    fn test_remove_peer() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Listener);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "listener",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        handler.remove_peer(&mut sender, "producer");

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "consumer");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::EndSession {
                peer_id: "producer".to_string()
            }
        );

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "listener");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::ProducerRemoved {
                peer_id: "producer".to_string()
            }
        );
    }

    #[test]
    fn test_end_session_consumer() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "producer");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::EndSession {
                peer_id: "consumer".to_string()
            }
        );
    }

    #[test]
    fn test_end_session_producer() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            peer_id: "consumer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "consumer");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::EndSession {
                peer_id: "producer".to_string()
            }
        );
    }

    #[test]
    #[should_panic(expected = "producer has no consumer")]
    fn test_end_session_twice() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            peer_id: "consumer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            peer_id: "consumer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();
    }

    #[test]
    fn test_sdp_exchange() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            peer_id: "consumer".to_string(),
            peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                sdp: "offer".to_string(),
            }),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "consumer");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::Peer(p::PeerMessage {
                peer_id: "producer".to_string(),
                peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                    sdp: "offer".to_string()
                })
            })
        );
    }

    #[test]
    fn test_ice_exchange() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            peer_id: "consumer".to_string(),
            peer_message: p::PeerMessageInner::Ice {
                candidate: "candidate".to_string(),
                sdp_mline_index: 42,
            },
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "consumer");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::Peer(p::PeerMessage {
                peer_id: "producer".to_string(),
                peer_message: p::PeerMessageInner::Ice {
                    candidate: "candidate".to_string(),
                    sdp_mline_index: 42
                }
            })
        );

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            peer_id: "producer".to_string(),
            peer_message: p::PeerMessageInner::Ice {
                candidate: "candidate".to_string(),
                sdp_mline_index: 42,
            },
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        let sent_message = sender.messages.pop_front().unwrap();

        assert_eq!(sent_message.peer_id, "producer");
        assert_eq!(
            serde_json::from_str::<p::OutgoingMessage>(&sent_message.message).unwrap(),
            p::OutgoingMessage::Peer(p::PeerMessage {
                peer_id: "consumer".to_string(),
                peer_message: p::PeerMessageInner::Ice {
                    candidate: "candidate".to_string(),
                    sdp_mline_index: 42
                }
            })
        );
    }

    #[test]
    #[should_panic(expected = "they are not in a session")]
    fn test_sdp_exchange_wrong_direction_offer() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            peer_id: "producer".to_string(),
            peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                sdp: "offer".to_string(),
            }),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "not registered as a producer")]
    fn test_start_session_no_producer() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "not registered as a consumer")]
    fn test_start_session_no_consumer() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "already in a session with producer")]
    fn test_start_session_twice() {
        let mut sender = MockMessageSender::default();
        let mut handler = DefaultMessageHandler::default();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "producer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer);

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();

        sender.messages.pop_front().unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        handler
            .handle_message(
                &mut sender,
                &serde_json::to_string(&message).unwrap(),
                "consumer",
            )
            .unwrap();
    }
}
