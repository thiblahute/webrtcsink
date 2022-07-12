use gst::prelude::*;

const RTP_TWCC_URI: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

use crate::webrtcsrc::signaller::{prelude::*, Signallable, Signaller};
use anyhow::{Context, Error};
use gst::glib;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use std::ops::ControlFlow;
use std::str::FromStr;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use url::Url;

const DEFAULT_STUN_SERVER: Option<&str> = Some("stun://stun.l.google.com:19302");

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtcsrc",
        gst::DebugColorFlags::empty(),
        Some("WebRTC src"),
    )
});

pub struct WebRTCSrc {
    stun_server: Mutex<Option<String>>,
    signaller: Mutex<Signallable>,
    meta: Mutex<Option<gst::Structure>>,
    video_codecs: Mutex<gst::Array>,
    audio_codecs: Mutex<gst::Array>,

    n_pads: AtomicU16,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCSrc {
    const NAME: &'static str = "GstWebRTCSrc";
    type Type = super::WebRTCSrc;
    type ParentType = gst::Bin;
    type Interfaces = (gst::URIHandler, gst::ChildProxy);
}

impl ObjectImpl for WebRTCSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("stun-server")
                    .flags(glib::ParamFlags::READWRITE)
                    .default_value(DEFAULT_STUN_SERVER)
                    .build(),
                glib::ParamSpecObject::builder::<Signallable>("signaller")
                    .flags(glib::ParamFlags::READWRITE)
                    .blurb("The Signallable object to use to handle WebRTC Signalling")
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("meta")
                    .flags(glib::ParamFlags::READWRITE)
                    .blurb("Free form metadata about the producer")
                    .build(),
                gst::ParamSpecArray::builder("video-codecs")
                    .flags(glib::ParamFlags::READWRITE)
                    .blurb("Names of usable video codecs")
                    .build(),
                gst::ParamSpecArray::builder("audio-codecs")
                    .flags(glib::ParamFlags::READWRITE)
                    .blurb("Names of usable audio codecs")
                    .build(),
            ]
        });

        PROPS.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "signaller" => {
                *self.signaller.lock().unwrap() =
                    value.get::<Signallable>().expect("type checked upstream");
            }
            "video-codecs" => {
                *self.video_codecs.lock().unwrap() =
                    value.get::<gst::Array>().expect("type checked upstream");
            }
            "audio-codecs" => {
                *self.audio_codecs.lock().unwrap() =
                    value.get::<gst::Array>().expect("type checked upstream");
            }
            "stun-server" => {
                *self.stun_server.lock().unwrap() = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
            }
            "meta" => {
                *self.meta.lock().unwrap() = value
                    .get::<Option<gst::Structure>>()
                    .expect("type checked upstream")
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "signaller" => self.signaller.lock().unwrap().to_value(),
            "video-codecs" => self.video_codecs.lock().unwrap().to_value(),
            "audio-codecs" => self.audio_codecs.lock().unwrap().to_value(),
            "stun-server" => self.stun_server.lock().unwrap().to_value(),
            "meta" => self.meta.lock().unwrap().to_value(),
            name => panic!("{} getter not implemented", name),
        }
    }

    fn constructed(&self, obj: &super::WebRTCSrc) {
        self.parent_constructed(obj);
        let signaller = self.signaller.lock().unwrap().clone();

        self.connect_signaller(&signaller);

        obj.connect_pad_removed(|instance, pad| {
            let imp = instance.imp();
            imp.state.lock().unwrap().flow_combiner.add_pad(pad);
            imp.n_pads.fetch_sub(1, Ordering::SeqCst);
        });

        obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
        obj.set_element_flags(gst::ElementFlags::SOURCE);
    }
}

impl Default for WebRTCSrc {
    fn default() -> Self {
        let signaller = Signaller::default();

        Self {
            stun_server: Mutex::new(DEFAULT_STUN_SERVER.map(|v| v.to_string())),
            state: Default::default(),
            n_pads: Default::default(),
            signaller: Mutex::new(signaller.upcast()),
            meta: Default::default(),
            audio_codecs: Mutex::new(gst::Array::from_values(["OPUS".into()])),
            video_codecs: Mutex::new(gst::Array::from_values([
                "VP8".into(),
                "H264".into(),
                "VP9".into(),
                "H265".into(),
            ])),
        }
    }
}

#[allow(dead_code)]
struct SignallerSignals {
    error: glib::SignalHandlerId,
    session_started: glib::SignalHandlerId,
    session_ended: glib::SignalHandlerId,
    request_meta: glib::SignalHandlerId,
    sdp_offer: glib::SignalHandlerId,
    handle_ice: glib::SignalHandlerId,
}

impl WebRTCSrc {
    // --------------------------
    // Implementation
    fn webrtcbin(&self) -> gst::Bin {
        let state = self.state.lock().unwrap();
        let webrtcbin = state
            .webrtcbin
            .as_ref()
            .expect("We should never call `.webrtcbin()` when state not > Ready")
            .clone()
            .downcast::<gst::Bin>()
            .unwrap();

        webrtcbin
    }

    fn signaller(&self) -> Signallable {
        self.signaller.lock().unwrap().clone()
    }

    fn add_pad(
        &self,
        instance: &super::WebRTCSrc,
        pad: &gst::Pad,
    ) -> Result<(), glib::error::BoolError> {
        let caps = pad.current_caps().unwrap();
        gst::info!(CAT, "Pad added with caps: {}", caps.to_string());
        let template = instance.pad_template("src_%u").unwrap();
        let pad_num = self.n_pads.fetch_add(1, Ordering::SeqCst);
        let name = format!("src_{}", pad_num);
        let src_pad = gst::GhostPad::builder_with_template(&template, Some(&name))
            .proxy_pad_chain_function(glib::clone!(@weak instance => @default-panic, move
                |pad, parent, buffer| {
                    let this = instance.imp();
                    let padret = pad.chain_default(parent, buffer);
                    let ret = this.state.lock().unwrap().flow_combiner.update_flow(padret);

                    ret
                }
            ))
            .proxy_pad_event_function(glib::clone!(@weak instance => @default-panic, move
                |pad, parent, event| {
                let stream_start = match event.view() {
                    gst::EventView::StreamStart(stream_start) => stream_start,

                    _ => return pad.event_default(parent, event),
                };

                let collection = match instance
                    .imp()
                    .state
                    .lock()
                    .unwrap()
                    .stream_collection
                    .clone()
                {
                    Some(c) => c,
                    _ => return pad.event_default(parent, event),
                };

                // Match the pad with a GstStream in our collection
                let stream = collection.iter().find(|stream| {
                    match stream.caps() {
                        Some(stream_caps) => {
                            if !caps.is_subset(&stream_caps) {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                    instance.src_pads().iter().any(|pad| {
                        let mut already_used = false;

                        pad.sticky_events_foreach(|event| {
                            if let gst::EventView::StreamStart(stream_start) = event.view() {
                                if Some(stream_start.stream_id().into()) == stream.stream_id() {
                                    already_used = true;
                                    ControlFlow::Break(gst::EventForeachAction::Keep)
                                } else {
                                    ControlFlow::Continue(gst::EventForeachAction::Keep)
                                }
                            } else {
                                ControlFlow::Continue(gst::EventForeachAction::Keep)
                            }
                        });

                        !already_used
                    })
                });

                let event = stream.map_or_else(
                    || event.clone(),
                    |stream| {
                        let stream_id = stream.stream_id().unwrap();
                        gst::event::StreamStart::builder(stream_id.as_str())
                            .stream(stream)
                            .seqnum(stream_start.seqnum())
                            .group_id(
                                stream_start.group_id().unwrap_or_else(gst::GroupId::next),
                            )
                            .build()
                    },
                );

                pad.event_default(parent, event)
            }))
            .build_with_target(pad)
            .unwrap();

        let res = instance.add_pad(&src_pad);

        if res.is_ok() {
            let mut state = self.state.lock().unwrap();

            state.flow_combiner.add_pad(&src_pad);
            if let Some(collection) = state.stream_collection.clone() {
                drop(state);
                src_pad.push_event(gst::event::StreamCollection::builder(&collection).build());
                let num_media_streams = collection
                    .iter()
                    .filter(|stream| {
                        !(stream.stream_type() & (gst::StreamType::VIDEO | gst::StreamType::AUDIO))
                            .is_empty()
                    })
                    .count() as u16;

                if pad_num + 1 == num_media_streams {
                    instance.no_more_pads();
                }
            }
        }

        res
    }

    fn prepare(&self) -> Result<(), Error> {
        let webrtcbin = gst::ElementFactory::make("webrtcbin", None)
            .with_context(|| "Failed to make element webrtcbin".to_string())
            .unwrap();

        webrtcbin.set_property_from_str("bundle-policy", "max-bundle");
        webrtcbin
            .set_property_from_value("stun-server", &self.stun_server.lock().unwrap().to_value());
        let instance = self.instance();
        webrtcbin.connect_pad_added(glib::clone!(@weak instance => move |_webrtcbin, pad| {
            if let Err(err) = instance.imp().add_pad(&instance, pad) {
                gst::error!(CAT, "Could not add pad {}: {err:?}", pad.name());
            }
        }));

        webrtcbin.connect_closure(
            "on-ice-candidate",
            false,
            glib::closure!(@watch instance => move |
                    _webrtcbin: gst::Bin,
                    sdp_m_line_index: u32,
                    candidate: String| {
                let this = Self::from_instance(instance);
                this.on_ice_candidate(sdp_m_line_index, candidate);
            }),
        );

        instance
            .add(&webrtcbin)
            .expect("Could not add `webrtcbin`?");

        self.state.lock().unwrap().webrtcbin.replace(webrtcbin);

        Ok(())
    }

    fn unprepare(&self, instance: &super::WebRTCSrc) -> Result<(), Error> {
        gst::info!(CAT, obj: instance, "unpreparing");

        self.maybe_stop_signaller();
        self.state.lock().unwrap().session_id = None;
        while let Ok(Some(pad)) = instance.iterate_pads().next() {
            instance
                .remove_pad(&pad)
                .map_err(|err| anyhow::anyhow!("Couldn't remove pad? {err:?}"))?;
        }

        Ok(())
    }

    fn connect_signaller(&self, signaler: &Signallable) {
        let instance = self.instance();

        let _ = self
            .state
            .lock()
            .unwrap()
            .signaller_signals
            .insert(SignallerSignals {
                error: signaler.connect_closure(
                    "error",
                    false,
                    glib::closure!(@watch instance => move |
                    _signaler: glib::Object, error: String| {
                        gst::element_error!(
                            instance,
                            gst::StreamError::Failed,
                            ["Signalling error: {}", error]
                        );
                    }),
                ),

                session_started: signaler.connect_closure(
                    "session-started",
                    false,
                    glib::closure!(@watch instance => move |
                            _signaler: glib::Object,
                            session_id: &str,
                            _peer_id: &str| {
                        gst::info!(CAT, "Session started: {session_id}");
                        instance.imp().state.lock().unwrap().session_id =
                            Some(session_id.to_string());
                    }),
                ),

                session_ended: signaler.connect_closure(
                    "session-ended",
                    false,
                    glib::closure!(@watch instance => move |
                        _signaller: glib::Object, _peer_id: &str| {
                        instance.imp().state.lock().unwrap().session_id = None;
                        gst::debug!(CAT, "Session ended.");
                        gst::element_error!(
                            instance,
                            gst::StreamError::Failed,
                            ["Peer ended session"]
                        );
                    }),
                ),

                request_meta: signaler.connect_closure(
                    "request-meta",
                    false,
                    glib::closure!(@watch instance => move |
                        _signaler: glib::Object| -> Option<gst::Structure> {
                        let meta = instance.imp().meta.lock().unwrap().clone();

                        meta
                    }),
                ),

                sdp_offer: signaler.connect_closure(
                    "sdp-offer",
                    false,
                    glib::closure!(@watch instance => move |
                            _signaler: glib::Object,
                            _peer_id: &str,
                            offer: &gst_webrtc::WebRTCSessionDescription| {
                        instance.imp().handle_offer(offer);
                    }),
                ),

                // sdp_mid is exposed for future proofing, see
                // https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/1174,
                // at the moment sdp_m_line_index must be Some
                handle_ice: signaler.connect_closure(
                    "handle-ice",
                    false,
                    glib::closure!(@watch instance => move |
                            _signaler: glib::Object,
                            peer_id: &str,
                            sdp_m_line_index: u32,
                            _sdp_mid: Option<String>,
                            candidate: &str| {
                        instance
                            .imp()
                            .handle_ice(peer_id, Some(sdp_m_line_index), None, candidate);
                    }),
                ),
            });

        // previous signals are disconnected when dropping the old structure
    }

    fn handle_offer(&self, offer: &gst_webrtc::WebRTCSessionDescription) {
        let sdp = offer.sdp();
        let direction = gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly;

        let webrtcbin = self.webrtcbin();
        for media in sdp.medias() {
            let all_caps = media
                .formats()
                .filter_map(|format| {
                    format
                        .parse::<i32>()
                        .map(|pt| {
                            let mut tmpcaps = media.caps_from_media(pt).unwrap();
                            tmpcaps
                                .get_mut()
                                .unwrap()
                                .structure_mut(0)
                                .unwrap()
                                .set_name("application/x-rtp");

                            // Activated twcc extension if offered
                            for attribute in media.attributes() {
                                if let Some(value) = attribute.value() {
                                    if let [k, v] =
                                        value.split(' ').into_iter().collect::<Vec<&str>>()[..]
                                    {
                                        if !v.contains(RTP_TWCC_URI) {
                                            continue;
                                        }

                                        if let Ok(twcc_idx) = k.parse::<u32>() {
                                            tmpcaps.get_mut().unwrap().iter_mut().for_each(|s| {
                                                s.set("rtcp-fb-transport-cc", &true);
                                                s.set(
                                                    &format!("extmap-{}", twcc_idx),
                                                    RTP_TWCC_URI,
                                                );
                                            });

                                            break;
                                        }
                                    }
                                }
                            }

                            tmpcaps
                        })
                        .ok()
                })
                .collect::<Vec<gst::Caps>>();

            let mut caps = gst::Caps::new_empty();
            for codec in self
                .video_codecs
                .lock()
                .unwrap()
                .iter()
                .chain(self.audio_codecs.lock().unwrap().iter())
            {
                for c in &all_caps {
                    if c.structure(0).unwrap().get::<&str>("encoding-name")
                        == Ok(codec.get::<&str>().unwrap())
                    {
                        caps.get_mut().unwrap().append(c.clone());
                    }
                }
            }
            gst::info!(CAT, "Adding transceiver with caps: {}", caps.to_string());

            let transceiver =
                webrtcbin.emit_by_name::<gst::Object>("add-transceiver", &[&direction, &caps]);
            transceiver.set_property("do-nack", &true);
        }

        gst::log!(CAT, "Got offer {}", offer.sdp().to_string());
        webrtcbin.emit_by_name::<()>("set-remote-description", &[&offer, &None::<gst::Promise>]);

        let instance = self.instance();
        let promise = gst::Promise::with_change_func(glib::clone!(@weak instance => move |reply| {
                instance.imp().on_answer_created(reply);
            }
        ));

        webrtcbin.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
    }

    fn on_answer_created(&self, reply: Result<Option<&gst::StructureRef>, gst::PromiseError>) {
        let reply = match reply {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                todo!("Answer creation future got no response");
            }
            Err(err) => {
                todo!("FIXME {reply:?}: {err:?}");
            }
        };

        let answer = reply
            .value("answer")
            .unwrap()
            .get::<gst_webrtc::WebRTCSessionDescription>()
            .expect("Invalid argument");

        self.webrtcbin()
            .emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);

        let sdp = answer.sdp();
        gst::log!(CAT, "Answer: {}", sdp.to_string());

        let streams = &sdp
            .medias()
            .map(|media| {
                let caps = media.formats().find_map(|format| {
                    format.parse::<i32>().map_or(None, |pt| {
                        media.caps_from_media(pt).and_then(|mut caps| {
                            // apt: associated payload type. The value of this parameter is the
                            // payload type of the associated original stream.
                            // Sometimes "apt" == "format" meaning that the format is
                            // the original stream
                            let apt = caps.structure(0).unwrap().get::<&str>("apt").ok();
                            if format != apt.unwrap_or(format) {
                                None
                            } else {
                                caps.get_mut()
                                    .unwrap()
                                    .structure_mut(0)
                                    .unwrap()
                                    .set_name("application/x-rtp");
                                Some(caps)
                            }
                        })
                    })
                });

                let stream_type = match media.media() {
                    Some("video") => gst::StreamType::VIDEO,
                    Some("audio") => gst::StreamType::AUDIO,
                    _ => gst::StreamType::UNKNOWN,
                };

                if caps.is_none() && stream_type != gst::StreamType::UNKNOWN {
                    gst::warning!(CAT, "No caps for known stream type: {:?}", media);
                }

                gst::Stream::new(None, caps.as_ref(), stream_type, gst::StreamFlags::empty())
            })
            .collect::<Vec<gst::Stream>>();

        let collection = gst::StreamCollection::builder(None)
            .streams(streams)
            .build();

        if let Err(err) = self
            .instance()
            .post_message(gst::message::StreamCollection::new(&collection))
        {
            gst::error!(CAT, "Could not post stream collection: {:?}", err);
        }

        let session_id = {
            let mut state = self.state.lock().unwrap();
            state.stream_collection = Some(collection);

            match &state.session_id {
                Some(id) => id.to_string(),
                _ => {
                    gst::element_error!(
                        self.instance(),
                        gst::StreamError::Failed,
                        ["Signalling error, no session started while requesting to send an SDP offer"]
                    );

                    return;
                }
            }
        };

        let signaller = self.signaller();
        signaller.send_sdp(session_id, &answer);
    }

    fn on_ice_candidate(&self, sdp_m_line_index: u32, candidate: String) {
        let signaller = self.signaller();
        let session_id = match self.state.lock().unwrap().session_id.as_ref() {
            Some(id) => id.to_string(),
            _ => {
                gst::element_error!(
                        self.instance(),
                        gst::StreamError::Failed,
                        ["Signalling error, no session started while requesting to propose ice candidates"]
                    );

                return;
            }
        };
        signaller.add_ice(
            session_id,
            &candidate,
            Some(sdp_m_line_index),
            None::<String>,
        );
    }

    /// Called by the signaller with an ice candidate
    fn handle_ice(
        &self,
        peer_id: &str,
        sdp_m_line_index: Option<u32>,
        _sdp_mid: Option<String>,
        candidate: &str,
    ) {
        let sdp_m_line_index = match sdp_m_line_index {
            Some(m_line) => m_line,
            None => {
                gst::error!(CAT, "No mandatory mline");
                return;
            }
        };
        gst::log!(CAT, obj: &self.instance(), "Got ice from {peer_id}: {candidate}");

        self.webrtcbin()
            .emit_by_name::<()>("add-ice-candidate", &[&sdp_m_line_index, &candidate]);
    }

    fn maybe_start_signaller(&self) {
        let instance = self.instance();
        let mut state = self.state.lock().unwrap();
        if state.signaller_state == SignallerState::Stopped
            && instance.current_state() >= gst::State::Paused
        {
            self.signaller().vstart();

            gst::info!(CAT, "Started signaller");
            state.signaller_state = SignallerState::Started;
        }
    }

    fn maybe_stop_signaller(&self) {
        let mut state = self.state.lock().unwrap();
        if state.signaller_state == SignallerState::Started {
            self.signaller().vstop();
            state.signaller_state = SignallerState::Stopped;
            gst::info!(CAT, "Stopped signaller");
        }
    }
}

impl ElementImpl for WebRTCSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "WebRTCSrc",
                "Source/Network/WebRTC",
                "WebRTC src",
                "Thibault Saunier <tsaunier@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            vec![gst::PadTemplate::new(
                "src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &gst::Caps::new_empty_simple("application/x-rtp"),
            )
            .unwrap()]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        instance: &super::WebRTCSrc,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if let gst::StateChange::NullToReady = transition {
            if let Err(err) = self.prepare() {
                gst::element_error!(
                    instance,
                    gst::StreamError::Failed,
                    ["Failed to prepare: {}", err]
                );
                return Err(gst::StateChangeError);
            }
        }

        let mut ret = self.parent_change_state(instance, transition);

        match transition {
            gst::StateChange::PausedToReady => {
                if let Err(err) = self.unprepare(instance) {
                    gst::element_error!(
                        instance,
                        gst::StreamError::Failed,
                        ["Failed to unprepare: {}", err]
                    );
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::ReadyToPaused => {
                ret = Ok(gst::StateChangeSuccess::NoPreroll);
            }
            gst::StateChange::PausedToPlaying => {
                self.maybe_start_signaller();
            }
            _ => (),
        }

        ret
    }
}

impl GstObjectImpl for WebRTCSrc {}

impl BinImpl for WebRTCSrc {}

// --------------------------
// Interfaces implementation
impl ChildProxyImpl for WebRTCSrc {
    fn child_by_index(&self, _object: &Self::Type, _index: u32) -> Option<glib::Object> {
        None
    }

    fn children_count(&self, _object: &Self::Type) -> u32 {
        0
    }

    fn child_by_name(&self, _object: &Self::Type, name: &str) -> Option<glib::Object> {
        match name {
            "signaller" => {
                gst::info!(CAT, "Getting signaller");
                Some(self.signaller().upcast())
            }
            _ => None,
        }
    }
}

impl URIHandlerImpl for WebRTCSrc {
    const URI_TYPE: gst::URIType = gst::URIType::Src;

    fn protocols() -> &'static [&'static str] {
        &["gstwebrtc", "gstwebrtcs"]
    }

    fn uri(&self, _instance: &Self::Type) -> Option<String> {
        self.signaller().property::<Option<String>>("address")
    }

    fn set_uri(&self, _instance: &Self::Type, uri: &str) -> Result<(), glib::Error> {
        let uri = Url::from_str(uri)
            .map_err(|err| glib::Error::new(gst::URIError::BadUri, &format!("{:?}", err)))?;

        let socket_scheme = match uri.scheme() {
            "gstwebrtc" => Ok("ws"),
            "gstwebrtcs" => Ok("wss"),
            _ => Err(glib::Error::new(
                gst::URIError::BadUri,
                &format!("Invalid protocol: {}", uri.scheme()),
            )),
        }?;

        let mut url_str = uri.to_string();

        url_str.replace_range(0..uri.scheme().len(), socket_scheme);

        self.signaller().set_property("address", &url_str);

        Ok(())
    }
}

#[derive(PartialEq)]
enum SignallerState {
    Started,
    Stopped,
}

struct State {
    session_id: Option<String>,
    signaller_state: SignallerState,
    webrtcbin: Option<gst::Element>,
    flow_combiner: gst_base::UniqueFlowCombiner,
    signaller_signals: Option<SignallerSignals>,
    stream_collection: Option<gst::StreamCollection>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            signaller_state: SignallerState::Stopped,
            session_id: None,
            webrtcbin: None,
            flow_combiner: Default::default(),
            signaller_signals: Default::default(),
            stream_collection: Default::default(),
        }
    }
}
