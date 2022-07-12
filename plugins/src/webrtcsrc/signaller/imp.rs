use crate::utils::{gvalue_to_json, serialize_json_object};
use crate::webrtcsrc::signaller::{prelude::*, Signallable};
use anyhow::{anyhow, Error};
use async_std::{future::timeout, task};
use async_tungstenite::tungstenite::Message as WsMessage;
use futures::channel::mpsc;
use futures::prelude::*;
use gst::glib;
use gst::glib::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;
use url::Url;
use webrtcsink_protocol as p;

use super::CAT;

#[derive(Debug, Eq, PartialEq, Clone, Copy, glib::Enum, Default)]
#[repr(u32)]
#[enum_type(name = "GstWebRTCSignallerRole")]
pub enum WebRTCSignallerRole {
    #[default]
    Consumer,
    Producer,
    Listener,
}

pub struct Settings {
    address: String,
    cafile: Option<String>,
    role: WebRTCSignallerRole,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            address: "ws://127.0.0.1:8443".to_string(),
            cafile: Default::default(),
            role: Default::default(),
        }
    }
}

#[derive(Default)]
pub struct Signaller {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

#[derive(Default)]
struct State {
    /// Sender for the websocket messages
    websocket_sender: Option<mpsc::Sender<p::IncomingMessage>>,
    send_task_handle: Option<task::JoinHandle<Result<(), Error>>>,
    receive_task_handle: Option<task::JoinHandle<()>>,
    producers: HashSet<String>,
}

impl Signaller {
    fn uri(&self) -> Result<Url, Error> {
        Url::from_str(&self.instance().property::<String>("address"))
            .map_err(|err| anyhow!("{err:?}"))
    }

    async fn connect(&self) -> Result<(), Error> {
        let instance = self.instance();

        let role = self.settings.lock().unwrap().role;
        if let super::WebRTCSignallerRole::Consumer = role {
            self.producer_peer_id()
                .ok_or_else(|| anyhow!("No target producer peer id set"))?;
        }

        let connector = if let Some(path) = instance.property::<Option<String>>("cafile") {
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
        let (websocket_sender, mut websocket_receiver) = mpsc::channel::<p::IncomingMessage>(1000);
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

        let meta = if let Some(meta) =
            instance.emit_by_name::<Option<gst::Structure>>("request-meta", &[])
        {
            gvalue_to_json(&meta.to_value())
        } else {
            None
        };

        let winstance = instance.downgrade();
        let receive_task_handle = task::spawn(async move {
            while let Some(msg) = async_std::stream::StreamExt::next(&mut ws_stream).await {
                if let Some(instance) = winstance.upgrade() {
                    match msg {
                        Ok(WsMessage::Text(msg)) => {
                            gst::trace!(CAT, obj: &instance, "Received message {}", msg);

                            if let Ok(msg) = serde_json::from_str::<p::OutgoingMessage>(&msg) {
                                match msg {
                                    p::OutgoingMessage::Welcome { peer_id } => {
                                        let imp = instance.imp();
                                        imp.set_status(&meta, &peer_id);
                                        imp.start_session();
                                    }
                                    p::OutgoingMessage::PeerStatusChanged(p::PeerStatus {
                                        meta,
                                        roles,
                                        peer_id,
                                    }) => {
                                        let meta = meta.and_then(|m| match m {
                                            serde_json::Value::Object(v) => {
                                                Some(serialize_json_object(&v))
                                            }
                                            _ => {
                                                gst::error!(CAT, "Invalid json value: {m:?}");
                                                None
                                            }
                                        });

                                        let peer_id = peer_id.expect(
                                            "Status changed should always contain a peer ID",
                                        );
                                        let mut state = instance.imp().state.lock().unwrap();
                                        if roles.iter().any(|r| matches!(r, p::PeerRole::Producer))
                                        {
                                            if !state.producers.contains(&peer_id) {
                                                state.producers.insert(peer_id.clone());
                                                drop(state);

                                                instance.emit_by_name::<()>(
                                                    "producer-added",
                                                    &[&peer_id, &meta],
                                                );
                                            }
                                        } else if state.producers.remove(&peer_id) {
                                            drop(state);

                                            instance.emit_by_name::<()>(
                                                "producer-removed",
                                                &[&peer_id, &meta],
                                            );
                                        }
                                    }
                                    p::OutgoingMessage::SessionStarted {
                                        peer_id,
                                        session_id,
                                    } => {
                                        instance.emit_by_name::<()>(
                                            "session-started",
                                            &[&session_id, &peer_id],
                                        );
                                    }
                                    p::OutgoingMessage::StartSession { peer_id, .. } => {
                                        assert!(matches!(
                                            instance.property::<WebRTCSignallerRole>("role"),
                                            super::WebRTCSignallerRole::Producer
                                        ));

                                        instance
                                            .emit_by_name::<()>("session-requested", &[&peer_id]);
                                    }
                                    p::OutgoingMessage::EndSession(p::EndSessionMessage {
                                        session_id,
                                    }) => {
                                        gst::info!(
                                            CAT,
                                            obj: &instance,
                                            "Session {session_id} ended"
                                        );

                                        instance
                                            .emit_by_name::<()>("session-ended", &[&session_id]);
                                    }
                                    p::OutgoingMessage::Peer(p::PeerMessage {
                                        session_id,
                                        peer_message,
                                    }) => match peer_message {
                                        p::PeerMessageInner::Sdp(p::SdpMessage::Answer { sdp }) => {
                                            let sdp = match gst_sdp::SDPMessage::parse_buffer(
                                                sdp.as_bytes(),
                                            ) {
                                                Ok(sdp) => sdp,
                                                Err(err) => {
                                                    instance.emit_by_name::<()>(
                                                        "error",
                                                        &[&format!(
                                                            "Error parsing SDP: {sdp} {err:?}"
                                                        )],
                                                    );

                                                    break;
                                                }
                                            };

                                            let answer = gst_webrtc::WebRTCSessionDescription::new(
                                                gst_webrtc::WebRTCSDPType::Answer,
                                                sdp,
                                            );
                                            instance.emit_by_name::<()>(
                                                "sdp-answer",
                                                &[&session_id, &answer],
                                            );
                                        }
                                        p::PeerMessageInner::Sdp(p::SdpMessage::Offer { sdp }) => {
                                            let sdp = match gst_sdp::SDPMessage::parse_buffer(
                                                sdp.as_bytes(),
                                            ) {
                                                Ok(sdp) => sdp,
                                                Err(err) => {
                                                    instance.emit_by_name::<()>(
                                                        "error",
                                                        &[&format!(
                                                            "Error parsing SDP: {sdp} {err:?}"
                                                        )],
                                                    );

                                                    break;
                                                }
                                            };

                                            let offer = gst_webrtc::WebRTCSessionDescription::new(
                                                gst_webrtc::WebRTCSDPType::Offer,
                                                sdp,
                                            );
                                            instance.emit_by_name::<()>(
                                                "sdp-offer",
                                                &[&session_id, &offer],
                                            );
                                        }
                                        p::PeerMessageInner::Ice {
                                            candidate,
                                            sdp_m_line_index,
                                        } => {
                                            let sdp_mid: Option<String> = None;
                                            instance.emit_by_name::<()>(
                                                "handle-ice",
                                                &[
                                                    &session_id,
                                                    &sdp_m_line_index,
                                                    &sdp_mid,
                                                    &candidate,
                                                ],
                                            );
                                        }
                                    },
                                    p::OutgoingMessage::Error { details } => {
                                        instance.emit_by_name::<()>(
                                            "error",
                                            &[&format!("Error message from server: {details}")],
                                        );
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

                                instance.emit_by_name::<()>(
                                    "error",
                                    &[&format!("Unknown message from server: {}", msg)],
                                );
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
                            instance.emit_by_name::<()>(
                                "error",
                                &[&format!("Error receiving: {}", err)],
                            );
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

    fn set_status(&self, meta: &Option<serde_json::Value>, peer_id: &str) {
        let role = self.settings.lock().unwrap().role;
        self.send(p::IncomingMessage::SetPeerStatus(match role {
            super::WebRTCSignallerRole::Consumer => p::PeerStatus {
                meta: meta.clone(),
                peer_id: Some(peer_id.to_string()),
                roles: vec![],
            },
            super::WebRTCSignallerRole::Producer => p::PeerStatus {
                meta: meta.clone(),
                peer_id: Some(peer_id.to_string()),
                roles: vec![p::PeerRole::Producer],
            },
            super::WebRTCSignallerRole::Listener => p::PeerStatus {
                meta: meta.clone(),
                peer_id: Some(peer_id.to_string()),
                roles: vec![p::PeerRole::Listener],
            },
        }));
    }

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
                        instance.emit_by_name::<()>("error", &[&format!("Error: {}", err)]);
                    }
                }
            });
        }
    }

    pub fn start_session(&self) {
        let instance = self.instance();

        let role = self.settings.lock().unwrap().role;
        if matches!(role, super::WebRTCSignallerRole::Consumer) {
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
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Signaller {
    const NAME: &'static str = "GstWebRTCSignaller";
    type Type = super::Signaller;
    type ParentType = gst::Object;
    type Interfaces = (Signallable,);
}

impl ObjectImpl for Signaller {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("address")
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecString::builder("cafile")
                    .flags(glib::ParamFlags::READWRITE)
                    .build(),
                glib::ParamSpecEnum::builder::<super::WebRTCSignallerRole>(
                    "role",
                    WebRTCSignallerRole::Consumer,
                )
                .flags(glib::ParamFlags::READWRITE)
                .build(),
            ]
        });

        PROPS.as_ref()
    }

    fn set_property(
        &self,
        _obj: &<Self as glib::subclass::types::ObjectSubclass>::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "address" => settings.address = value.get::<String>().expect("type checked upstream"),
            "cafile" => {
                settings.cafile = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
            }
            "role" => {
                settings.role = value
                    .get::<WebRTCSignallerRole>()
                    .expect("type checked upstream")
            }
            _ => unimplemented!(),
        }
    }

    fn property(
        &self,
        _obj: &<Self as glib::subclass::types::ObjectSubclass>::Type,
        _id: usize,
        pspec: &glib::ParamSpec,
    ) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "address" => settings.address.to_value(),
            "cafile" => settings.cafile.to_value(),
            "role" => settings.role.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl SignallableImpl for Signaller {
    fn vstart(&self, instance: &Self::Type) {
        let instance = instance.clone();
        gst::info!(CAT, "Starting");
        task::spawn(async move {
            let this = Self::from_instance(&instance);
            if let Err(err) = this.connect().await {
                instance.emit_by_name::<()>("error", &[&format!("Error receiving: {}", err)]);
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
        session_id: String,
        sdp: &gst_webrtc::WebRTCSessionDescription,
    ) {
        let role = self.settings.lock().unwrap().role;
        let is_consumer = match role {
            super::WebRTCSignallerRole::Consumer => true,
            super::WebRTCSignallerRole::Producer => false,
            _ => unreachable!(),
        };

        let msg = p::IncomingMessage::Peer(p::PeerMessage {
            session_id,
            peer_message: p::PeerMessageInner::Sdp(if is_consumer {
                p::SdpMessage::Answer {
                    sdp: sdp.sdp().as_text().unwrap(),
                }
            } else {
                p::SdpMessage::Offer {
                    sdp: sdp.sdp().as_text().unwrap(),
                }
            }),
        });

        self.send(msg);
    }

    fn add_ice(
        &self,
        _obj: &Self::Type,
        session_id: String,
        candidate: &str,
        sdp_m_line_index: Option<u32>,
        _sdp_mid: Option<String>,
    ) {
        let msg = p::IncomingMessage::Peer(p::PeerMessage {
            session_id,
            peer_message: p::PeerMessageInner::Ice {
                candidate: candidate.to_string(),
                sdp_m_line_index: sdp_m_line_index.unwrap(),
            },
        });

        self.send(msg);
    }

    fn end_session(&self, obj: &Self::Type, session_id: &str) {
        gst::debug!(CAT, obj: obj, "Signalling session done {}", session_id);

        let state = self.state.lock().unwrap();
        let instance = obj.downgrade();
        let session_id = session_id.to_string();
        if let Some(mut sender) = state.websocket_sender.clone() {
            task::spawn(async move {
                if let Err(err) = sender
                    .send(p::IncomingMessage::EndSession(p::EndSessionMessage {
                        session_id,
                    }))
                    .await
                {
                    if let Some(instance) = instance.upgrade() {
                        instance.emit_by_name::<()>("error", &[&format!("Error: {}", err)]);
                    }
                }
            });
        }
    }
}
impl GstObjectImpl for Signaller {}
