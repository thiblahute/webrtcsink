use anyhow::{anyhow, Error};
use async_std::{future::timeout, task};
use async_tungstenite::tungstenite::Message as WsMessage;
use futures::channel::mpsc;
use futures::prelude::*;
use gst::glib::prelude::*;
use gst::glib::{self, Type};
use gst::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;
use url::Url;
use webrtcsink_protocol as p;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtcsrc-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC src signaller"),
    )
});

#[derive(Default)]
struct State {
    /// Sender for the websocket messages
    websocket_sender: Option<mpsc::Sender<p::IncomingMessage>>,
    send_task_handle: Option<task::JoinHandle<Result<(), Error>>>,
    receive_task_handle: Option<task::JoinHandle<()>>,
}

#[derive(Clone, Default)]
struct Settings {
    uri: Option<Url>,
    cafile: Option<PathBuf>,
}

#[derive(Default)]
pub struct SourceSignaller {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl SourceSignaller {
    async fn connect(&self) -> Result<(), Error> {
        self.peer_id().ok_or_else(|| anyhow!("No peer id set"))?;
        let settings = self.settings.lock().unwrap().clone();

        let connector = if let Some(path) = settings.cafile {
            let cert = async_std::fs::read_to_string(&path).await?;
            let cert = async_native_tls::Certificate::from_pem(cert.as_bytes())?;
            let connector = async_native_tls::TlsConnector::new();
            Some(connector.add_root_certificate(cert))
        } else {
            None
        };

        let mut uri = settings.uri.unwrap().clone();
        uri.set_query(None);
        let (ws, _) = timeout(
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

        let meta =
            if let Some(meta) = instance.emit_by_name::<Option<gst::Structure>>("get-meta", &[]) {
                gvalue_to_json(&meta.to_value())
            } else {
                None
            };

        websocket_sender
            .send(p::IncomingMessage::Register(p::RegisterMessage::Consumer {
                meta: meta.clone(),
            }))
            .await?;

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
                                        p::RegisteredMessage::Consumer { peer_id, .. },
                                    ) => {
                                        let imp = instance.imp();
                                        imp.start_session();
                                        gst::info!(
                                            CAT,
                                            obj: &instance,
                                            "We are registered with the server, our peer id is {}, now registering as listener",
                                            peer_id
                                        );

                                        imp.send(p::IncomingMessage::Register(
                                            p::RegisterMessage::Listener { meta: meta.clone() },
                                        ));
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
                                        instance.emit_by_name::<()>(
                                            "producer-added",
                                            &[&peer_id, &meta],
                                        );
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
                                        instance.emit_by_name::<()>(
                                            "producer-removed",
                                            &[&peer_id, &meta],
                                        );
                                    }
                                    p::OutgoingMessage::Registered(register_info) => gst::info!(
                                        CAT,
                                        "Got new registered user: {:?}",
                                        register_info
                                    ),
                                    p::OutgoingMessage::StartSession { .. } => unreachable!(),
                                    p::OutgoingMessage::EndSession(
                                        p::EndSessionMessage::Producer { peer_id },
                                    ) => {
                                        gst::error!(
                                            CAT,
                                            obj: &instance,
                                            "Session {} ended",
                                            peer_id
                                        );

                                        instance.emit_by_name::<()>("end-session", &[&peer_id]);
                                    }
                                    p::OutgoingMessage::Peer(
                                        p::PeerMessage::Consumer(info)
                                        | p::PeerMessage::Producer(info),
                                    ) => match info.peer_message {
                                        p::PeerMessageInner::Sdp(p::SdpMessage::Answer {
                                            ..
                                        }) => unreachable!(),
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
                                            instance.emit_by_name::<()>("sdp-offer", &[&offer]);
                                        }
                                        p::PeerMessageInner::Ice {
                                            candidate,
                                            sdp_m_line_index,
                                        } => {
                                            let sdp_mid: Option<String> = None;
                                            instance.emit_by_name::<()>(
                                                "handle-ice",
                                                &[
                                                    &info.peer_id,
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

    pub fn peer_id(&self) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        if let Some(ref uri) = settings.uri {
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

    pub fn start(&self) {
        let instance = self.instance();
        task::spawn(async move {
            let this = Self::from_instance(&instance);
            if let Err(err) = this.connect().await {
                instance.emit_by_name::<()>("error", &[&format!("Error receiving: {}", err)]);
            }
        });
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
        let target_producer = self.peer_id().unwrap();

        self.send(p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: target_producer,
        }));
    }

    pub fn handle_sdp(&self, sdp: &gst_webrtc::WebRTCSessionDescription) {
        let peer_id = self.peer_id();
        let msg = p::IncomingMessage::Peer(p::PeerMessage::Producer(p::PeerMessageInfo {
            peer_id: peer_id.unwrap(),
            peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Answer {
                sdp: sdp.sdp().as_text().unwrap(),
            }),
        }));

        self.send(msg);
    }

    /// sdp_mid is exposed for future proofing, see
    /// https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/1174,
    /// at the moment sdp_m_line_index will always be Some and sdp_mid will always
    /// be None
    pub fn handle_ice(
        &self,
        candidate: &str,
        sdp_m_line_index: Option<u32>,
        _sdp_mid: Option<String>,
    ) {
        let msg = p::IncomingMessage::Peer(p::PeerMessage::Consumer(p::PeerMessageInfo {
            peer_id: self.peer_id().unwrap(),
            peer_message: p::PeerMessageInner::Ice {
                candidate: candidate.to_string(),
                sdp_m_line_index: sdp_m_line_index.unwrap(),
            },
        }));

        self.send(msg);
    }

    pub fn uri(&self) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        settings.uri.as_ref().map(|uri| uri.to_string())
    }

    pub fn set_uri(&self, uri: &str) -> Result<(), Error> {
        gst::info!(CAT, "Setting uri: {uri}");
        match Url::from_str(uri) {
            Ok(uri) => {
                let mut settings = self.settings.lock().unwrap();
                settings.uri = Some(uri);

                Ok(())
            }
            Err(err) => Err(anyhow!("{err:?}")),
        }
    }

    pub fn stop(&self) {
        let instance = self.instance();
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
}

#[glib::object_subclass]
impl ObjectSubclass for SourceSignaller {
    const NAME: &'static str = "RsWebRTCSrcSignaller";
    type Type = super::SourceSignaller;
    type ParentType = gst::Object;
}

impl ObjectImpl for SourceSignaller {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::new(
                    "address",
                    "Address",
                    "Address of the signalling server",
                    Some("ws://127.0.0.1:8443"),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecString::new(
                    "cafile",
                    "CA file",
                    "Path to a Certificate file to add to the set of roots the TLS connector will trust",
                    None,
                    glib::ParamFlags::READWRITE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "address" => {
                let address: Option<_> = value.get().expect("type checked upstream");

                if let Some(address) = address {
                    gst::info!(CAT, "Signaller address set to {}", address);

                    if let Err(err) = self.set_uri(address) {
                        gst::error!(CAT, "{err:?}");
                    }
                } else {
                    gst::error!(CAT, "address can't be None");
                }
            }
            "cafile" => {
                let value: String = value.get().unwrap();
                let mut settings = self.settings.lock().unwrap();
                settings.cafile = Some(value.into());
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "address" => self
                .settings
                .lock()
                .unwrap()
                .uri
                .as_ref()
                .map(|uri| uri.to_string())
                .to_value(),
            "cafile" => {
                let settings = self.settings.lock().unwrap();
                let cafile = settings.cafile.as_ref();
                cafile.and_then(|file| file.to_str()).to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                glib::subclass::Signal::builder(
                    "end-session",
                    &[String::static_type().into()],
                    glib::types::Type::UNIT.into(),
                )
                .build(),
                glib::subclass::Signal::builder(
                    "producer-added",
                    &[
                        String::static_type().into(),
                        gst::Structure::static_type().into(),
                    ],
                    glib::types::Type::UNIT.into(),
                )
                .build(),
                glib::subclass::Signal::builder(
                    "producer-removed",
                    &[
                        String::static_type().into(),
                        gst::Structure::static_type().into(),
                    ],
                    glib::types::Type::UNIT.into(),
                )
                .build(),
                glib::subclass::Signal::builder(
                    "error",
                    &[String::static_type().into()],
                    glib::types::Type::UNIT.into(),
                )
                .build(),
                glib::subclass::Signal::builder(
                    "get-meta",
                    &[],
                    gst::Structure::static_type().into(),
                )
                .build(),
                glib::subclass::Signal::builder(
                    "handle-sdp",
                    &[gst_webrtc::WebRTCSessionDescription::static_type().into()],
                    glib::types::Type::UNIT.into(),
                )
                .action()
                .class_handler(|_, args| {
                    let instance = args[0].get::<super::SourceSignaller>().expect("signal arg");
                    let session_desc = args[1]
                        .get::<gst_webrtc::WebRTCSessionDescription>()
                        .unwrap();

                    instance.imp().handle_sdp(&session_desc);

                    None
                })
                .build(),
                glib::subclass::Signal::builder(
                    "add-ice",
                    &[
                        str::static_type().into(),
                        u32::static_type().into(),
                        String::static_type().into(),
                    ],
                    glib::types::Type::UNIT.into(),
                )
                .action()
                .class_handler(|_, args| {
                    let instance = args[0].get::<super::SourceSignaller>().expect("signal arg");
                    let candidate = args[1].get::<&str>().unwrap();
                    let sdp_m_line_index = args[2].get::<u32>().ok();
                    let sdp_mid = args[3].get::<Option<String>>().unwrap();

                    instance
                        .imp()
                        .handle_ice(candidate, sdp_m_line_index, sdp_mid);

                    None
                })
                .build(),
                glib::subclass::Signal::builder("start", &[], glib::types::Type::UNIT.into())
                    .action()
                    .class_handler(|_, args| {
                        let instance = args[0].get::<super::SourceSignaller>().expect("signal arg");
                        instance.imp().start();

                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("stop", &[], glib::types::Type::UNIT.into())
                    .action()
                    .class_handler(|_, args| {
                        let instance = args[0].get::<super::SourceSignaller>().expect("signal arg");

                        instance.imp().stop();

                        None
                    })
                    .build(),
                glib::subclass::Signal::builder(
                    "handle-ice",
                    &[
                        // peer_id
                        str::static_type().into(),
                        // sdp_m_line_index
                        u32::static_type().into(),
                        // sdp_mid
                        str::static_type().into(),
                        //candidate
                        str::static_type().into(),
                    ],
                    glib::types::Type::UNIT.into(),
                )
                .build(),
                glib::subclass::Signal::builder(
                    "sdp-offer",
                    &[gst_webrtc::WebRTCSessionDescription::static_type().into()],
                    glib::types::Type::UNIT.into(),
                )
                .build(),
            ]
        });

        SIGNALS.as_ref()
    }
}

impl GstObjectImpl for SourceSignaller {}

fn gvalue_to_json(val: &gst::glib::Value) -> Option<serde_json::Value> {
    match val.type_() {
        Type::STRING => Some(val.get::<String>().unwrap().into()),
        Type::BOOL => Some(val.get::<bool>().unwrap().into()),
        Type::I32 => Some(val.get::<i32>().unwrap().into()),
        Type::U32 => Some(val.get::<u32>().unwrap().into()),
        Type::I_LONG | Type::I64 => Some(val.get::<i64>().unwrap().into()),
        Type::U_LONG | Type::U64 => Some(val.get::<u64>().unwrap().into()),
        Type::F32 => Some(val.get::<f32>().unwrap().into()),
        Type::F64 => Some(val.get::<f64>().unwrap().into()),
        _ => {
            if let Ok(s) = val.get::<gst::Structure>() {
                serde_json::to_value(
                    s.iter()
                        .filter_map(|(name, value)| {
                            gvalue_to_json(value).map(|value| (name.to_string(), value))
                        })
                        .collect::<HashMap<String, serde_json::Value>>(),
                )
                .ok()
            } else if let Ok(a) = val.get::<gst::Array>() {
                serde_json::to_value(
                    a.iter()
                        .filter_map(|value| gvalue_to_json(value))
                        .collect::<Vec<serde_json::Value>>(),
                )
                .ok()
            } else if let Some((_klass, values)) = gst::glib::FlagsValue::from_value(val) {
                Some(
                    values
                        .iter()
                        .map(|value| value.nick())
                        .collect::<Vec<&str>>()
                        .join("+")
                        .into(),
                )
            } else if let Ok(value) = val.serialize() {
                Some(value.as_str().into())
            } else {
                None
            }
        }
    }
}

fn json_to_gststructure(val: &serde_json::Value) -> Option<glib::SendValue> {
    match val {
        serde_json::Value::Bool(v) => Some(v.to_send_value()),
        serde_json::Value::Number(n) => {
            if n.is_u64() {
                Some(n.as_u64().unwrap().to_send_value())
            } else if n.is_i64() {
                Some(n.as_i64().unwrap().to_send_value())
            } else if n.is_f64() {
                Some(n.as_f64().unwrap().to_send_value())
            } else {
                todo!("Unhandled case {n:?}");
            }
        }
        serde_json::Value::String(v) => Some(v.to_send_value()),
        serde_json::Value::Array(v) => {
            let array = v
                .iter()
                .filter_map(|v| json_to_gststructure(v))
                .collect::<Vec<glib::SendValue>>();
            Some(gst::Array::from_values(array).to_send_value())
        }
        serde_json::Value::Object(v) => Some(serialize_json_object(v).to_send_value()),
        _ => None,
    }
}

fn serialize_json_object(val: &serde_json::Map<String, serde_json::Value>) -> gst::Structure {
    let mut res = gst::Structure::new_empty("v");

    val.iter().for_each(|(k, v)| {
        if let Some(gvalue) = json_to_gststructure(&v) {
            res.set_value(k, gvalue);
        }
    });

    res
}
