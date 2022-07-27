use anyhow::{anyhow, Error};
use futures::prelude::*;
use futures::ready;
use pin_project_lite::pin_project;
use std::collections::{HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::{info, instrument, warn};
use webrtcsink_protocol as p;

type PeerId = String;

pin_project! {
    #[must_use = "streams do nothing unless polled"]
    pub struct Handler {
        #[pin]
        stream: Pin<Box<dyn Stream<Item=(String, Option<p::IncomingMessage>)> + Send>>,
        items: VecDeque<(String, p::OutgoingMessage)>,
        producers: HashMap<PeerId, HashSet<PeerId>>,
        consumers: HashMap<PeerId, HashSet<PeerId>>,
        listeners: HashSet<PeerId>,
        meta: HashMap<PeerId, Option<serde_json::Value>>,
    }
}

impl Handler {
    #[instrument(level = "debug", skip(stream))]
    /// Create a handler
    pub fn new(
        stream: Pin<Box<dyn Stream<Item = (String, Option<p::IncomingMessage>)> + Send>>,
    ) -> Self {
        Self {
            stream,
            items: VecDeque::new(),
            producers: HashMap::new(),
            consumers: HashMap::new(),
            listeners: HashSet::new(),
            meta: HashMap::new(),
        }
    }

    #[instrument(level = "trace", skip(self))]
    fn handle(
        mut self: Pin<&mut Self>,
        peer_id: &str,
        msg: p::IncomingMessage,
    ) -> Result<(), Error> {
        match msg {
            p::IncomingMessage::Register(message) => match message {
                p::RegisterMessage::Producer { meta } => self.register_producer(peer_id, meta),
                p::RegisterMessage::Consumer { meta } => self.register_consumer(peer_id, meta),
                p::RegisterMessage::Listener { meta } => self.register_listener(peer_id, meta),
            },
            p::IncomingMessage::Unregister(message) => {
                let meta = self.meta.get(peer_id).unwrap_or_else(|| &None).clone();
                let answer = match message {
                    p::UnregisterMessage::Producer => {
                        self.remove_producer_peer(peer_id);
                        p::UnregisteredMessage::Producer {
                            peer_id: peer_id.into(),
                            meta,
                        }
                    }
                    p::UnregisterMessage::Consumer => {
                        self.remove_consumer_peer(peer_id);
                        p::UnregisteredMessage::Consumer {
                            peer_id: peer_id.into(),
                            meta,
                        }
                    }
                    p::UnregisterMessage::Listener => {
                        self.remove_listener_peer(peer_id);
                        p::UnregisteredMessage::Listener {
                            peer_id: peer_id.into(),
                            meta,
                        }
                    }
                };

                self.items.push_back((
                    peer_id.into(),
                    p::OutgoingMessage::Unregistered(answer.clone()),
                ));

                // We don't notify listeners about listeners activity
                match message {
                    p::UnregisterMessage::Producer | p::UnregisterMessage::Consumer => {
                        let mut messages = self
                            .listeners
                            .iter()
                            .map(|listener| {
                                (
                                    listener.to_string(),
                                    p::OutgoingMessage::Unregistered(answer.clone()),
                                )
                            })
                            .collect::<VecDeque<(String, p::OutgoingMessage)>>();

                        self.items.append(&mut messages);
                    }
                    _ => (),
                }

                Ok(())
            }
            p::IncomingMessage::StartSession(message) => {
                self.start_session(&message.peer_id, peer_id)
            }
            p::IncomingMessage::Peer(peermsg) => {
                let (from_consumer, peer_info) = match peermsg {
                    p::PeerMessage::Consumer(info) => (true, info),
                    p::PeerMessage::Producer(info) => (false, info),
                };

                match peer_info.peer_message {
                    p::PeerMessageInner::Ice {
                        candidate,
                        sdp_m_line_index,
                    } => self.handle_ice(
                        from_consumer,
                        candidate,
                        sdp_m_line_index,
                        peer_id,
                        &peer_info.peer_id,
                    ),

                    p::PeerMessageInner::Sdp(sdp_message) => match sdp_message {
                        p::SdpMessage::Offer { sdp } => {
                            self.handle_sdp_offer(from_consumer, sdp, peer_id, &peer_info.peer_id)
                        }
                        p::SdpMessage::Answer { sdp } => {
                            self.handle_sdp_answer(from_consumer, sdp, peer_id, &peer_info.peer_id)
                        }
                    },
                }
            }
            p::IncomingMessage::List => self.list_producers(peer_id),
            p::IncomingMessage::EndSession(p::EndSessionMessage::Consumer {
                peer_id: other_peer_id,
            }) => self.end_session(true, peer_id, &other_peer_id),
            p::IncomingMessage::EndSession(p::EndSessionMessage::Producer {
                peer_id: other_peer_id,
            }) => self.end_session(false, peer_id, &other_peer_id),
        }
    }

    #[instrument(level = "debug", skip(self))]
    /// Remove a peer, this can cause sessions to be ended
    fn remove_listener_peer(&mut self, peer_id: &str) {
        self.listeners.remove(peer_id);
    }

    #[instrument(level = "debug", skip(self))]
    /// Remove a peer, this can cause sessions to be ended
    fn remove_peer(&mut self, peer_id: &str) {
        info!(peer_id = %peer_id, "removing peer");

        self.remove_listener_peer(peer_id);
        self.remove_producer_peer(peer_id);
        self.remove_consumer_peer(peer_id);
    }

    #[instrument(level = "debug", skip(self))]
    fn remove_producer_peer(&mut self, peer_id: &str) {
        if let Some(consumers) = self.producers.remove(peer_id) {
            for consumer_id in &consumers {
                info!(producer_id=%peer_id, consumer_id=%consumer_id, "ended session");
                // Relation between the consumer and producer will be removed
                // when handling EndSession
                self.items.push_back((
                    consumer_id.to_string(),
                    p::OutgoingMessage::EndSession(p::EndSessionMessage::Producer {
                        peer_id: peer_id.to_string(),
                    }),
                ));
            }

            for listener in &self.listeners {
                self.items.push_back((
                    listener.to_string(),
                    p::OutgoingMessage::ProducerRemoved {
                        peer_id: peer_id.to_string(),
                        meta: match self.meta.get(peer_id) {
                            Some(meta) => meta.clone(),
                            None => Default::default(),
                        },
                    },
                ));
            }
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn remove_consumer_peer(&mut self, peer_id: &str) {
        self.consumers.remove(peer_id).map(|producers| {
            producers.iter().for_each(|producer_id| {
                info!(producer_id=%producer_id, consumer_id=%peer_id, "ended session");

                // The producer can have been removed and we are finalizing the session
                // afterward
                if let Some(consumers) = self.producers.get_mut(producer_id) {
                    consumers.remove(peer_id);
                }

                self.items.push_back((
                    producer_id.to_string(),
                    p::OutgoingMessage::EndSession(p::EndSessionMessage::Consumer {
                        peer_id: peer_id.to_string(),
                    }),
                ));
            })
        });

        let _ = self.meta.remove(peer_id);
    }

    #[instrument(level = "debug", skip(self))]
    /// End a session between two peers
    fn end_session(
        &mut self,
        from_consumer: bool,
        peer_id: &str,
        other_peer_id: &str,
    ) -> Result<(), Error> {
        if from_consumer {
            if let Some(producers) = self.consumers.get(peer_id) {
                if producers.contains(other_peer_id) {
                    info!(producer_id=%other_peer_id, consumer_id=%peer_id, "ended session");

                    self.consumers
                        .get_mut(&peer_id.to_string())
                        .unwrap()
                        .remove(other_peer_id);
                    // The producer can have been removed and we are finalizing the session
                    // afterward
                    if let Some(producers) = self.producers.get_mut(other_peer_id) {
                        producers.remove(peer_id);
                    }

                    self.items.push_back((
                        other_peer_id.to_string(),
                        p::OutgoingMessage::EndSession(p::EndSessionMessage::Consumer {
                            peer_id: peer_id.to_string(),
                        }),
                    ));

                    return Ok(());
                }
            }
        } else if let Some(ref mut consumers) = self.producers.get_mut(peer_id) {
            if consumers.remove(other_peer_id) {
                info!(producer_id=%peer_id, consumer_id=%other_peer_id, "ended session");

                self.items.push_back((
                    other_peer_id.to_string(),
                    p::OutgoingMessage::EndSession(p::EndSessionMessage::Producer {
                        peer_id: peer_id.to_string(),
                    }),
                ));

                self.consumers
                    .get_mut(other_peer_id)
                    .unwrap()
                    .remove(peer_id);
                return Ok(());
            }
        }

        return Err(anyhow!(
            "{} '{}' is not in a session with '{}'",
            if from_consumer {
                "Consumer"
            } else {
                "Producer"
            },
            peer_id,
            other_peer_id
        ));
    }

    /// List producer peers
    #[instrument(level = "debug", skip(self))]
    fn list_producers(&mut self, peer_id: &str) -> Result<(), Error> {
        self.items.push_back((
            peer_id.to_string(),
            p::OutgoingMessage::List {
                producers: self
                    .producers
                    .keys()
                    .cloned()
                    .map(|peer_id| p::Peer::Producer {
                        id: peer_id.clone(),
                        meta: self
                            .meta
                            .get(&peer_id)
                            .map_or_else(|| Default::default(), |m| m.clone()),
                    })
                    .collect(),
            },
        ));

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_consumer_ice(
        &mut self,
        candidate: String,
        sdp_m_line_index: u32,
        consumer_id: &str,
        producer_id: &str,
    ) -> Result<(), Error> {
        let err_fn = || -> Error {
            anyhow!(
                "cannot forward ICE from consumer={} to producer={} as they are not in a session",
                consumer_id,
                producer_id
            )
        };

        let producers = self.consumers.get(consumer_id).ok_or_else(|| err_fn())?;
        if !producers.contains(producer_id) {
            warn!("{producer_id} not contained");
            return Err(err_fn());
        }

        let info = p::PeerMessageInfo {
            peer_id: consumer_id.to_string(),
            peer_message: p::PeerMessageInner::Ice {
                candidate,
                sdp_m_line_index,
            },
        };
        self.items.push_back((
            producer_id.to_string(),
            p::OutgoingMessage::Peer(p::PeerMessage::Consumer(info)),
        ));

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_producer_ice(
        &mut self,
        candidate: String,
        sdp_m_line_index: u32,
        producer_id: &str,
        consumer_id: &str,
    ) -> Result<(), Error> {
        let err_fn = || -> Error {
            warn!(
                    "cannot forward ICE from producer={} to consumer={} as they are not in a session -- {:?}",
                    producer_id,
                    consumer_id,
                    self.consumers,
                );
            anyhow!(
                "cannot forward ICE from producer={} to consumer={} as they are not in a session",
                producer_id,
                consumer_id,
            )
        };

        let consumers = self.producers.get(producer_id).ok_or_else(|| err_fn())?;
        if !consumers.contains(consumer_id) {
            return Err(err_fn());
        }

        let info = p::PeerMessageInfo {
            peer_id: producer_id.to_string(),
            peer_message: p::PeerMessageInner::Ice {
                candidate,
                sdp_m_line_index,
            },
        };

        self.items.push_back((
            consumer_id.to_string(),
            p::OutgoingMessage::Peer(p::PeerMessage::Producer(info)),
        ));

        Ok(())
    }

    /// Handle ICE candidate sent by one peer to another peer
    #[instrument(level = "debug", skip(self))]
    fn handle_ice(
        &mut self,
        from_consumer: bool,
        candidate: String,
        sdp_m_line_index: u32,
        peer_id: &str,
        other_peer_id: &str,
    ) -> Result<(), Error> {
        if from_consumer {
            self.handle_consumer_ice(candidate, sdp_m_line_index, peer_id, other_peer_id)
        } else {
            self.handle_producer_ice(candidate, sdp_m_line_index, peer_id, other_peer_id)
        }
    }
    /// Handle SDP offered by one peer to another peer
    #[instrument(level = "debug", skip(self))]
    fn handle_sdp_offer(
        &mut self,
        from_consumer: bool,
        sdp: String,
        producer_id: &str,
        consumer_id: &str,
    ) -> Result<(), Error> {
        if let Some(consumers) = self.producers.get(producer_id) {
            if consumers.contains(consumer_id) {
                let info = p::PeerMessageInfo {
                    peer_id: producer_id.to_string(),
                    peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer { sdp }),
                };

                self.items.push_back((
                    consumer_id.to_string(),
                    p::OutgoingMessage::Peer(if from_consumer {
                        p::PeerMessage::Consumer(info)
                    } else {
                        p::PeerMessage::Producer(info)
                    }),
                ));
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
    #[instrument(level = "debug", skip(self))]
    fn handle_sdp_answer(
        &mut self,
        from_consumer: bool,
        sdp: String,
        consumer_id: &str,
        producer_id: &str,
    ) -> Result<(), Error> {
        if let Some(producers) = self.consumers.get(consumer_id) {
            if producers.contains(producer_id) {
                let info = p::PeerMessageInfo {
                    peer_id: consumer_id.to_string(),
                    peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Answer { sdp }),
                };
                self.items.push_back((
                    producer_id.to_string(),
                    p::OutgoingMessage::Peer(if from_consumer {
                        p::PeerMessage::Consumer(info)
                    } else {
                        p::PeerMessage::Producer(info)
                    }),
                ));
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
    #[instrument(level = "debug", skip(self))]
    fn register_producer(
        &mut self,
        peer_id: &str,
        meta: Option<serde_json::Value>,
    ) -> Result<(), Error> {
        if self.producers.contains_key(peer_id) {
            Err(anyhow!("{} is already registered as a producer", peer_id))
        } else {
            self.producers.insert(peer_id.to_string(), HashSet::new());

            for listener in &self.listeners {
                self.items.push_back((
                    listener.to_string(),
                    p::OutgoingMessage::ProducerAdded {
                        peer_id: peer_id.to_string(),
                        meta: meta.clone(),
                    },
                ));
            }

            self.items.push_back((
                peer_id.to_string(),
                p::OutgoingMessage::Registered(p::RegisteredMessage::Producer {
                    peer_id: peer_id.to_string(),
                    meta: meta.clone(),
                }),
            ));

            self.meta.insert(peer_id.to_string(), meta);

            info!(peer_id = %peer_id, "registered as a producer");

            Ok(())
        }
    }

    /// Register peer as a consumer
    #[instrument(level = "debug", skip(self))]
    fn register_consumer(
        &mut self,
        peer_id: &str,
        meta: Option<serde_json::Value>,
    ) -> Result<(), Error> {
        if self.consumers.contains_key(peer_id) {
            Err(anyhow!("{} is already registered as a consumer", peer_id))
        } else {
            self.consumers
                .insert(peer_id.to_string(), Default::default());

            self.items.push_back((
                peer_id.to_string(),
                p::OutgoingMessage::Registered(p::RegisteredMessage::Consumer {
                    peer_id: peer_id.to_string(),
                    meta: meta.clone(),
                }),
            ));

            self.meta.insert(peer_id.to_string(), meta);

            info!(peer_id = %peer_id, "registered as a consumer");

            Ok(())
        }
    }

    /// Register peer as a listener
    #[instrument(level = "debug", skip(self))]
    fn register_listener(
        &mut self,
        peer_id: &str,
        meta: Option<serde_json::Value>,
    ) -> Result<(), Error> {
        if !self.listeners.insert(peer_id.to_string()) {
            Err(anyhow!("{} is already registered as a listener", peer_id))
        } else {
            self.items.push_back((
                peer_id.to_string(),
                p::OutgoingMessage::Registered(p::RegisteredMessage::Listener {
                    peer_id: peer_id.to_string(),
                    meta: meta.clone(),
                }),
            ));

            self.meta.insert(peer_id.to_string(), meta);

            info!(peer_id = %peer_id, "registered as a listener");

            Ok(())
        }
    }

    /// Start a session between two peers
    #[instrument(level = "debug", skip(self))]
    fn start_session(&mut self, producer_id: &str, consumer_id: &str) -> Result<(), Error> {
        if !self.consumers.contains_key(consumer_id) {
            return Err(anyhow!(
                "Peer with id {} is not registered as a consumer",
                consumer_id
            ));
        }

        if let Some(producers) = self.consumers.get(consumer_id) {
            if producers.contains(producer_id) {
                return Err(anyhow!(
                    "Consumer with id {} is already in a session with producer {}",
                    consumer_id,
                    producer_id,
                ));
            }
        }

        if !self.producers.contains_key(producer_id) {
            return Err(anyhow!(
                "Peer with id {} is not registered as a producer",
                producer_id
            ));
        }

        self.consumers
            .get_mut(consumer_id)
            .unwrap()
            .insert(producer_id.to_string());
        self.producers
            .get_mut(producer_id)
            .unwrap()
            .insert(consumer_id.to_string());

        self.items.push_back((
            producer_id.to_string(),
            p::OutgoingMessage::StartSession {
                peer_id: consumer_id.to_string(),
            },
        ));

        info!(producer_id = %producer_id, consumer_id = %consumer_id, "started a session");

        Ok(())
    }
}

impl Stream for Handler {
    type Item = (String, p::OutgoingMessage);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let this = self.as_mut().project();

            if let Some(item) = this.items.pop_front() {
                break Poll::Ready(Some(item));
            }

            match ready!(this.stream.poll_next(cx)) {
                Some((peer_id, msg)) => {
                    if let Some(msg) = msg {
                        if let Err(err) = self.as_mut().handle(&peer_id, msg) {
                            self.items.push_back((
                                peer_id.to_string(),
                                p::OutgoingMessage::Error {
                                    details: err.to_string(),
                                },
                            ));
                        }
                    } else {
                        self.remove_peer(&peer_id);
                    }
                }
                None => {
                    break Poll::Ready(None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc;
    use serde_json::json;

    #[async_std::test]
    async fn test_register_producer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });

        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Registered(p::RegisteredMessage::Producer {
                peer_id: "producer".to_string(),
                meta: Default::default(),
            })
        );
    }

    #[async_std::test]
    async fn test_list_producers() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Some(json!( {"display-name": "foobar".to_string() })),
        });

        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();

        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::List;

        tx.send(("listener".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::List {
                producers: vec![p::Peer::Producer {
                    id: "producer".to_string(),
                    meta: Some(json!(
                        {"display-name": "foobar".to_string()
                    })),
                }]
            }
        );
    }

    #[async_std::test]
    async fn test_register_consumer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });

        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Registered(p::RegisteredMessage::Consumer {
                peer_id: "consumer".to_string(),
                meta: Default::default()
            })
        );
    }

    #[async_std::test]
    async fn test_register_producer_twice() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Error {
                details: "producer is already registered as a producer".into()
            }
        );
    }

    #[async_std::test]
    async fn test_listener() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Listener {
            meta: Default::default(),
        });
        tx.send(("listener".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Some(json!({
                "display-name": "foobar".to_string(),
            })),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::ProducerAdded {
                peer_id: "producer".to_string(),
                meta: Some(json!({
                    "display-name": Some("foobar".to_string()),
                }))
            }
        );
    }

    #[async_std::test]
    async fn test_start_session() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::StartSession {
                peer_id: "consumer".to_string()
            }
        );
    }

    #[async_std::test]
    async fn test_remove_peer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Listener {
            meta: Default::default(),
        });
        tx.send(("listener".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        handler.remove_peer("producer");
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage::Producer {
                peer_id: "producer".to_string()
            })
        );

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::ProducerRemoved {
                peer_id: "producer".to_string(),
                meta: Default::default()
            }
        );
    }

    #[async_std::test]
    async fn test_end_session_consumer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage::Consumer {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage::Consumer {
                peer_id: "consumer".to_string()
            })
        );
    }

    #[async_std::test]
    async fn test_end_session_producer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage::Producer {
            peer_id: "consumer".to_string(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage::Producer {
                peer_id: "producer".to_string()
            })
        );
    }

    #[async_std::test]
    async fn test_end_session_twice() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        // The consumer ends the session
        let message = p::IncomingMessage::EndSession(p::EndSessionMessage::Consumer {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage::Consumer {
                peer_id: "consumer".to_string(),
            })
        );

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage::Consumer {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Error {
                details: "Consumer 'consumer' is not in a session with 'producer'".into()
            }
        );
    }

    #[async_std::test]
    async fn test_sdp_exchange() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage::Consumer(p::PeerMessageInfo {
            peer_id: "consumer".to_string(),
            peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                sdp: "offer".to_string(),
            }),
        }));
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Peer(p::PeerMessage::Consumer(p::PeerMessageInfo {
                peer_id: "producer".to_string(),
                peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                    sdp: "offer".to_string()
                })
            }))
        );
    }

    #[async_std::test]
    async fn test_ice_exchange() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage::Producer(p::PeerMessageInfo {
            peer_id: "consumer".to_string(),
            peer_message: p::PeerMessageInner::Ice {
                candidate: "candidate".to_string(),
                sdp_m_line_index: 42,
            },
        }));
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Peer(p::PeerMessage::Producer(p::PeerMessageInfo {
                peer_id: "producer".to_string(),
                peer_message: p::PeerMessageInner::Ice {
                    candidate: "candidate".to_string(),
                    sdp_m_line_index: 42
                }
            }))
        );

        let message = p::IncomingMessage::Peer(p::PeerMessage::Consumer(p::PeerMessageInfo {
            peer_id: "producer".to_string(),
            peer_message: p::PeerMessageInner::Ice {
                candidate: "candidate".to_string(),
                sdp_m_line_index: 42,
            },
        }));
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Peer(p::PeerMessage::Consumer(p::PeerMessageInfo {
                peer_id: "consumer".to_string(),
                peer_message: p::PeerMessageInner::Ice {
                    candidate: "candidate".to_string(),
                    sdp_m_line_index: 42
                }
            }))
        );
    }

    #[async_std::test]
    async fn test_sdp_exchange_wrong_direction_offer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage::Producer(p::PeerMessageInfo {
            peer_id: "producer".to_string(),
            peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                sdp: "offer".to_string(),
            }),
        }));
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(sent_message,
            p::OutgoingMessage::Error {
                details: "cannot forward offer from consumer to producer as they are not in a session or consumer is not the producer".into()
            }
        );
    }

    #[async_std::test]
    async fn test_start_session_no_producer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Error {
                details: "Peer with id producer is not registered as a producer".into()
            }
        );
    }

    #[async_std::test]
    async fn test_unregistering() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::StartSession {
                peer_id: "consumer".to_string()
            }
        );

        let message = p::IncomingMessage::Unregister(p::UnregisterMessage::Producer);
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage::Producer {
                peer_id: "producer".to_string()
            })
        );

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Unregistered(p::UnregisteredMessage::Producer {
                peer_id: "producer".into(),
                meta: Default::default()
            })
        );
    }

    #[async_std::test]
    async fn test_unregistering_with_listenners() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Listener {
            meta: Default::default(),
        });
        tx.send(("listener".to_string(), Some(message)))
            .await
            .unwrap();
        let (l, _) = handler.next().await.unwrap();
        assert_eq!(l, "listener");

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Some(json!({"some": "meta"})),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::ProducerAdded {
                peer_id: "producer".to_string(),
                meta: Some(json!({"some": "meta"})),
            }
        );

        let (peer_id, _msg) = handler.next().await.unwrap();
        assert_eq!(peer_id, "producer");

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, _msg) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::StartSession {
                peer_id: "consumer".to_string()
            }
        );

        let message = p::IncomingMessage::Unregister(p::UnregisterMessage::Producer);
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage::Producer {
                peer_id: "producer".to_string()
            })
        );

        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::ProducerRemoved {
                peer_id: "producer".into(),
                meta: Some(json!({"some": "meta"}))
            }
        );
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Unregistered(p::UnregisteredMessage::Producer {
                peer_id: "producer".into(),
                meta: Some(json!({"some": "meta"}))
            })
        );
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Unregistered(p::UnregisteredMessage::Producer {
                peer_id: "producer".into(),
                meta: Some(json!({"some": "meta"})),
            })
        );
    }

    #[async_std::test]
    async fn test_start_session_no_consumer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Error {
                details: "Peer with id consumer is not registered as a consumer".into()
            }
        );
    }

    #[async_std::test]
    async fn test_start_session_twice() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Error {
                details: "Consumer with id consumer is already in a session with producer producer"
                    .into()
            }
        );
    }
}
