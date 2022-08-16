use gst::glib;
#[derive(Debug, Eq, PartialEq, Clone, Copy, glib::Enum, Default)]
#[repr(u32)]
#[enum_type(name = "GstWebRTCSignallerMode")]
pub enum WebRTCSignallerMode {
    #[default]
    Consumer,
    Producer,
    Listener,
}

#[gobject::class(final, extends(gst::Object), implements(super::Signallable), sync)]
mod implement {
    use crate::signaller::{prelude::*, Signallable};
    use crate::utils::{gvalue_to_json, serialize_json_object};
    use anyhow::{anyhow, Error};
    use async_std::{future::timeout, task};
    use async_tungstenite::tungstenite::Message as WsMessage;
    use futures::channel::mpsc;
    use futures::prelude::*;
    use gst::glib;
    use gst::glib::prelude::*;
    use gst::subclass::prelude::*;
    use std::str::FromStr;
    use std::sync::Mutex;
    use std::time::Duration;
    use url::Url;
    use webrtcsink_protocol as p;

    use super::super::CAT;

    pub struct Signaller {
        state: Mutex<State>,

        // FIXME Use a Mutex<Settings> nested structure when storage() is
        // supported for overridden props
        #[property(get, set, override_iface = "Signallable")]
        address: Mutex<String>,

        #[property(get, set, override_iface = "Signallable")]
        cafile: Mutex<Option<String>>,

        #[property(get, set, enum)]
        mode: Mutex<super::WebRTCSignallerMode>,
    }

    impl Default for Signaller {
        fn default() -> Self {
            Self {
                state: Default::default(),
                address: Mutex::new("ws://127.0.0.1:8443".to_string()),
                cafile: Default::default(),
                mode: Default::default(),
            }
        }
    }

    #[derive(Default)]
    struct State {
        /// Sender for the websocket messages
        websocket_sender: Option<mpsc::Sender<p::IncomingMessage>>,
        send_task_handle: Option<task::JoinHandle<Result<(), Error>>>,
        receive_task_handle: Option<task::JoinHandle<()>>,
    }

    impl Signaller {
        fn uri(&self) -> Result<Url, Error> {
            Url::from_str(&self.instance().address()).map_err(|err| anyhow!("{err:?}"))
        }

        async fn connect(&self) -> Result<(), Error> {
            let instance = self.instance();

            if let super::WebRTCSignallerMode::Consumer = *self.mode.lock().unwrap() {
                self.producer_peer_id()
                    .ok_or_else(|| anyhow!("No target producer peer id set"))?;
            }

            let connector = if let Some(path) = instance.cafile() {
                let cert = async_std::fs::read_to_string(&path).await?;
                let cert = async_native_tls::Certificate::from_pem(cert.as_bytes())?;
                let connector = async_native_tls::TlsConnector::new();
                Some(connector.add_root_certificate(cert))
            } else {
                None
            };

            let mut uri = self.uri()?;

            uri.set_query(None);
            let (ws, _) = timeout(
                // FIXME: Make the timeout configurable
                Duration::from_secs(20),
                async_tungstenite::async_std::connect_async_with_tls_connector(
                    uri.to_string(),
                    connector,
                ),
            )
            .await??;

            let instance = self.instance();

            gst::info!(CAT, obj: &instance, "connected");

            // Channel for asynchronously sending out websocket message
            let (mut ws_sink, mut ws_stream) = ws.split();

            // 1000 is completely arbitrary, we simply don't want infinite piling
            // up of messages as with unbounded
            let (mut websocket_sender, mut websocket_receiver) =
                mpsc::channel::<p::IncomingMessage>(1000);
            let instance_clone = instance.downgrade();
            let send_task_handle = task::spawn(async move {
                while let Some(msg) = websocket_receiver.next().await {
                    gst::log!(CAT, "Sending websocket message {:?}", msg);
                    ws_sink
                        .send(WsMessage::Text(serde_json::to_string(&msg).unwrap()))
                        .await?;
                }

                if let Some(instance) = instance_clone.upgrade() {
                    gst::info!(CAT, obj: &instance, "Done sending");
                }

                ws_sink.send(WsMessage::Close(None)).await?;
                ws_sink.close().await?;

                Ok::<(), Error>(())
            });

            gst::error!(CAT, obj: &instance, "Emit request meta");
            let meta = if let Some(meta) = instance.emit_request_meta() {
                gvalue_to_json(&meta.to_value())
            } else {
                None
            };

            websocket_sender.send(self.register_message(&meta)).await?;

            let winstance = instance.downgrade();
            let receive_task_handle = task::spawn(async move {
                while let Some(msg) = async_std::stream::StreamExt::next(&mut ws_stream).await {
                    if let Some(instance) = winstance.upgrade() {
                        match msg {
                            Ok(WsMessage::Text(msg)) => {
                                gst::trace!(CAT, obj: &instance, "Received message {}", msg);

                                if let Ok(msg) = serde_json::from_str::<p::OutgoingMessage>(&msg) {
                                    match msg {
                                        p::OutgoingMessage::Registered(
                                            p::RegisteredMessage::Consumer { .. },
                                        ) => {
                                            let imp = instance.imp();
                                            imp.start_session();
                                        }
                                        p::OutgoingMessage::ProducerAdded { peer_id, meta } => {
                                            let meta = meta.and_then(|m| match m {
                                                serde_json::Value::Object(v) => {
                                                    Some(serialize_json_object(&v))
                                                }
                                                _ => {
                                                    gst::error!(CAT, "Invalid json value: {m:?}");
                                                    None
                                                }
                                            });
                                            instance.emit_producer_added(&peer_id, meta);
                                        }
                                        p::OutgoingMessage::ProducerRemoved { peer_id, meta } => {
                                            let meta = meta.and_then(|m| match m {
                                                serde_json::Value::Object(v) => {
                                                    Some(serialize_json_object(&v))
                                                }
                                                _ => {
                                                    gst::error!(CAT, "Invalid json value: {m:?}");
                                                    None
                                                }
                                            });
                                            instance.emit_producer_removed(&peer_id, meta);
                                        }
                                        p::OutgoingMessage::Registered(register_info) => {
                                            gst::info!(
                                                CAT,
                                                "Got new registered user: {:?}",
                                                register_info
                                            )
                                        }
                                        p::OutgoingMessage::StartSession { peer_id } => {
                                            assert!(matches!(
                                                instance.mode(),
                                                super::WebRTCSignallerMode::Producer
                                            ));

                                            instance.emit_session_requested(&peer_id);
                                        }
                                        p::OutgoingMessage::EndSession(msg) => {
                                            let (peer_id, peer_type) = match msg {
                                                p::EndSessionMessage::Producer { peer_id } => {
                                                    (peer_id, "producer")
                                                }
                                                p::EndSessionMessage::Consumer { peer_id } => {
                                                    (peer_id, "consumer")
                                                }
                                            };
                                            gst::info!(
                                                CAT,
                                                obj: &instance,
                                                "Session {peer_type}: {peer_id} ended"
                                            );

                                            instance.emit_session_ended(&peer_id);
                                        }
                                        p::OutgoingMessage::Peer(
                                            p::PeerMessage::Consumer(info)
                                            | p::PeerMessage::Producer(info),
                                        ) => match info.peer_message {
                                            p::PeerMessageInner::Sdp(p::SdpMessage::Answer {
                                                sdp,
                                            }) => {
                                                let sdp = match gst_sdp::SDPMessage::parse_buffer(
                                                    sdp.as_bytes(),
                                                ) {
                                                    Ok(sdp) => sdp,
                                                    Err(err) => {
                                                        instance.emit_error(&format!(
                                                            "Error parsing SDP: {sdp} {err:?}"
                                                        ));

                                                        break;
                                                    }
                                                };

                                                let answer =
                                                    gst_webrtc::WebRTCSessionDescription::new(
                                                        gst_webrtc::WebRTCSDPType::Answer,
                                                        sdp,
                                                    );
                                                instance.emit_sdp_answer(&info.peer_id, &answer);
                                            }
                                            p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                                                sdp,
                                            }) => {
                                                let sdp = match gst_sdp::SDPMessage::parse_buffer(
                                                    sdp.as_bytes(),
                                                ) {
                                                    Ok(sdp) => sdp,
                                                    Err(err) => {
                                                        instance.emit_error(&format!(
                                                            "Error parsing SDP: {sdp} {err:?}"
                                                        ));

                                                        break;
                                                    }
                                                };

                                                let offer =
                                                    gst_webrtc::WebRTCSessionDescription::new(
                                                        gst_webrtc::WebRTCSDPType::Offer,
                                                        sdp,
                                                    );
                                                instance.emit_sdp_offer(&info.peer_id, &offer);
                                            }
                                            p::PeerMessageInner::Ice {
                                                candidate,
                                                sdp_m_line_index,
                                            } => {
                                                let sdp_mid: Option<String> = None;
                                                instance.emit_handle_ice(
                                                    &info.peer_id,
                                                    sdp_m_line_index,
                                                    sdp_mid,
                                                    &candidate,
                                                );
                                            }
                                        },
                                        p::OutgoingMessage::Error { details } => {
                                            instance.emit_error(&format!(
                                                "Error message from server: {details}"
                                            ));
                                        }
                                        _ => {
                                            gst::warning!(
                                                CAT,
                                                obj: &instance,
                                                "Ignoring unsupported message {:?}",
                                                msg
                                            );
                                        }
                                    }
                                } else {
                                    gst::error!(
                                        CAT,
                                        obj: &instance,
                                        "Unknown message from server: {}",
                                        msg
                                    );

                                    instance.emit_error(&format!(
                                        "Unknown message from server: {}",
                                        msg
                                    ));
                                }
                            }
                            Ok(WsMessage::Close(reason)) => {
                                gst::info!(
                                    CAT,
                                    obj: &instance,
                                    "websocket connection closed: {:?}",
                                    reason
                                );
                                break;
                            }
                            Ok(_) => (),
                            Err(err) => {
                                instance.emit_error(&format!("Error receiving: {}", err));
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }

                if let Some(instance) = winstance.upgrade() {
                    gst::info!(CAT, obj: &instance, "Stopped websocket receiving");
                }
            });

            let mut state = self.state.lock().unwrap();
            state.websocket_sender = Some(websocket_sender);
            state.send_task_handle = Some(send_task_handle);
            state.receive_task_handle = Some(receive_task_handle);

            Ok(())
        }

        fn register_message(&self, meta: &Option<serde_json::Value>) -> p::IncomingMessage {
            p::IncomingMessage::Register(match *self.mode.lock().unwrap() {
                super::WebRTCSignallerMode::Consumer => {
                    p::RegisterMessage::Consumer { meta: meta.clone() }
                }
                super::WebRTCSignallerMode::Producer => {
                    p::RegisterMessage::Producer { meta: meta.clone() }
                }
                super::WebRTCSignallerMode::Listener => {
                    p::RegisterMessage::Listener { meta: meta.clone() }
                }
            })
        }

        #[public]
        fn producer_peer_id(&self) -> Option<String> {
            if let Ok(ref uri) = self.uri() {
                if let Ok(id) = uri.query_pairs().find(|(k, _)| k == "peer-id").map_or_else(
                    || Err(anyhow!("No `peer-id` set in url")),
                    |v| Ok(v.1.to_string()),
                ) {
                    Some(id)
                } else {
                    None
                }
            } else {
                None
            }
        }

        pub fn send(&self, msg: p::IncomingMessage) {
            let state = self.state.lock().unwrap();
            if let Some(mut sender) = state.websocket_sender.clone() {
                let instance = self.instance().downgrade();
                task::spawn(async move {
                    if let Err(err) = sender.send(msg).await {
                        if let Some(instance) = instance.upgrade() {
                            instance.emit_error(&format!("Error: {}", err));
                        }
                    }
                });
            }
        }

        pub fn start_session(&self) {
            let instance = self.instance();

            match *self.mode.lock().unwrap() {
                super::WebRTCSignallerMode::Consumer => {
                    let target_producer = self.producer_peer_id().unwrap();

                    self.send(p::IncomingMessage::StartSession(p::StartSessionMessage {
                        peer_id: target_producer.clone(),
                    }));

                    gst::info!(
                        CAT,
                        obj: &instance,
                        "We are registered with the server, our peer id is {}, now registering as listener",
                        target_producer
                    );
                }
                _ => (),
            }
        }
    }

    impl super::Signaller {
        #[constructor(infallible, default)]
        fn default() -> Self {}

        #[constructor(infallible)]
        pub fn new(mode: super::WebRTCSignallerMode) -> Self {}
    }

    impl SignallableImpl for Signaller {
        fn vstart(&self, instance: &Self::Type) {
            let instance = instance.clone();
            task::spawn(async move {
                let this = Self::from_instance(&instance);
                if let Err(err) = this.connect().await {
                    gst::error!(CAT, "-=-->? BOOM {err}");
                    instance.emit_error(&format!("Error receiving: {}", err));
                }
            });
        }

        fn vstop(&self, instance: &Self::Type) {
            let instance = instance.clone();
            gst::info!(CAT, obj: &instance, "Stopping now");

            let mut state = self.state.lock().unwrap();
            let send_task_handle = state.send_task_handle.take();
            let receive_task_handle = state.receive_task_handle.take();
            if let Some(mut sender) = state.websocket_sender.take() {
                task::block_on(async move {
                    sender.close_channel();

                    if let Some(handle) = send_task_handle {
                        if let Err(err) = handle.await {
                            gst::warning!(
                                CAT,
                                obj: &instance,
                                "Error while joining send task: {}",
                                err
                            );
                        }
                    }

                    if let Some(handle) = receive_task_handle {
                        handle.await;
                    }
                });
            }
        }

        fn send_sdp(
            &self,
            _instance: &Self::Type,
            peer_id: Option<&str>,
            sdp: &gst_webrtc::WebRTCSessionDescription,
        ) {
            let is_consumer = match *self.mode.lock().unwrap() {
                super::WebRTCSignallerMode::Consumer => true,
                super::WebRTCSignallerMode::Producer => false,
                _ => unreachable!(),
            };

            let peer_id =
                peer_id.map_or_else(|| self.producer_peer_id().unwrap(), |id| id.to_string());
            let info = p::PeerMessageInfo {
                peer_id,
                peer_message: p::PeerMessageInner::Sdp(if is_consumer {
                    p::SdpMessage::Answer {
                        sdp: sdp.sdp().as_text().unwrap(),
                    }
                } else {
                    p::SdpMessage::Offer {
                        sdp: sdp.sdp().as_text().unwrap(),
                    }
                }),
            };

            let msg = p::IncomingMessage::Peer(if is_consumer {
                p::PeerMessage::Consumer(info)
            } else {
                p::PeerMessage::Producer(info)
            });

            self.send(msg);
        }

        fn add_ice(
            &self,
            _obj: &Self::Type,
            peer_id: Option<&str>,
            candidate: &str,
            sdp_m_line_index: Option<u32>,
            _sdp_mid: Option<String>,
        ) {
            let peer_id =
                peer_id.map_or_else(|| self.producer_peer_id().unwrap(), |id| id.to_string());
            let info = p::PeerMessageInfo {
                peer_id,
                peer_message: p::PeerMessageInner::Ice {
                    candidate: candidate.to_string(),
                    sdp_m_line_index: sdp_m_line_index.unwrap(),
                },
            };

            let msg = p::IncomingMessage::Peer(match *self.mode.lock().unwrap() {
                super::WebRTCSignallerMode::Consumer => p::PeerMessage::Consumer(info),
                super::WebRTCSignallerMode::Producer => p::PeerMessage::Producer(info),
                _ => unreachable!(),
            });

            self.send(msg);
        }

        fn consumer_removed(&self, obj: &Self::Type, peer_id: &str) {
            gst::debug!(CAT, obj: obj, "Signalling consumer {} removed", peer_id);

            let state = self.state.lock().unwrap();
            let peer_id = peer_id.to_string();
            let instance = obj.downgrade();
            if let Some(mut sender) = state.websocket_sender.clone() {
                task::spawn(async move {
                    if let Err(err) = sender
                        .send(p::IncomingMessage::EndSession(
                            p::EndSessionMessage::Consumer {
                                peer_id: peer_id.to_string(),
                            },
                        ))
                        .await
                    {
                        if let Some(instance) = instance.upgrade() {
                            instance.emit_error(&format!("Error: {}", err));
                        }
                    }
                });
            }
        }
    }
    impl GstObjectImpl for Signaller {}
}
