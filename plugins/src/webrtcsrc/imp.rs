use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base;
use url::Url;

use anyhow::{Context, Error};
use once_cell::sync::Lazy;
use std::str::FromStr;
use std::sync::Mutex;

use crate::webrtcsrc::signaller::SourceSignaller;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtcsrc",
        gst::DebugColorFlags::empty(),
        Some("WebRTC sink"),
    )
});

const DEFAULT_STUN_SERVER: Option<&str> = Some("stun://stun.l.google.com:19302");

/// User configuration
struct Settings {
    video_codecs: gst::Array,
    audio_codecs: gst::Array,
    meta: Option<gst::Structure>,
}

#[derive(PartialEq)]
enum SignallerState {
    Started,
    Stopped,
}

/* Our internal state */
struct State {
    signaller: gst::Object,
    signaller_state: SignallerState,
    webrtcbin: Option<gst::Element>,
    flow_combiner: gst_base::UniqueFlowCombiner,
}

/// Wrapper around `gst::ElementFactory::make` with a better error
/// message
pub fn make_element(element: &str, name: Option<&str>) -> Result<gst::Element, Error> {
    gst::ElementFactory::make(element, name)
        .with_context(|| format!("Failed to make element {}", element))
}

/// Our instance structure
#[derive(Default)]
pub struct WebRTCSrc {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            video_codecs: gst::Array::from_values([
                "VP8".into(),
                "H264".into(),
                "VP9".into(),
                "H265".into(),
            ]),
            audio_codecs: gst::Array::from_values(["OPUS".into()]),
            meta: None,
        }
    }
}

impl Default for State {
    fn default() -> Self {
        let signaller = SourceSignaller::default();

        Self {
            signaller: signaller.upcast(),
            signaller_state: SignallerState::Stopped,
            webrtcbin: None,
            flow_combiner: Default::default(),
        }
    }
}

impl State {
    fn maybe_start_signaller(&mut self, element: &super::WebRTCSrc) {
        if self.signaller_state == SignallerState::Stopped
            && element.current_state() >= gst::State::Paused
        {
            self.signaller.emit_by_name::<()>("start", &[]);

            gst::info!(CAT, "Started signaller");
            self.signaller_state = SignallerState::Started;
        }
    }

    fn maybe_stop_signaller(&mut self) {
        if self.signaller_state == SignallerState::Started {
            self.signaller.emit_by_name::<()>("stop", &[]);
            self.signaller_state = SignallerState::Stopped;
            gst::info!(CAT, "Stopped signaller");
        }
    }
}

impl WebRTCSrc {
    fn add_pad(
        &self,
        element: &super::WebRTCSrc,
        pad: &gst::Pad,
    ) -> Result<(), glib::error::BoolError> {
        gst::info!(
            CAT,
            "Pad added with caps: {}",
            pad.current_caps().unwrap().to_string()
        );
        let template = element.pad_template("src_%u").unwrap();
        let src_pad = gst::GhostPad::builder_with_template(&template, None)
            .proxy_pad_chain_function(
                glib::clone!(@weak element => @default-panic, move |pad, parent, buffer| {
                    let this = element.imp();
                    let padret = pad.chain_default(parent, buffer);
                    let ret = this.state.lock().unwrap().flow_combiner.update_flow(padret);

                    ret
                }),
            )
            .build_with_target(pad)
            .unwrap();

        element.add_pad(&src_pad)
    }

    fn prepare(&self, element: &super::WebRTCSrc) -> Result<(), Error> {
        let webrtcbin = make_element("webrtcbin", None)?;

        webrtcbin.set_property_from_str("bundle-policy", "max-bundle");
        webrtcbin.connect_pad_added(glib::clone!(@weak element => move |_webrtcbin, pad| {
            if let Err(err) = element.imp().add_pad(&element, pad) {
                gst::error!(CAT, "Could not add pad {}: {err:?}", pad.name());
            }
        }));

        webrtcbin.connect_closure(
            "on-ice-candidate",
            false,
            glib::closure!(@watch element =>
                move |_webrtcbin: gst::Bin, sdp_m_line_index: u32, candidate: String| {
                let this = Self::from_instance(element);
                this.on_ice_candidate(
                    sdp_m_line_index,
                    candidate,
                );
            }),
        );

        element.add(&webrtcbin).expect("Could not add `webrtcbin`?");

        self.state.lock().unwrap().webrtcbin.replace(webrtcbin);

        Ok(())
    }

    /// Unprepare by stopping consumers, then the signaller object.
    /// Might abort codec discovery
    fn unprepare(&self, element: &super::WebRTCSrc) -> Result<(), Error> {
        gst::info!(CAT, obj: element, "unpreparing");

        let mut state = self.state.lock().unwrap();
        state.maybe_stop_signaller();

        Ok(())
    }

    fn connect_signaller(&self, signaler: &gst::Object) {
        let element = self.instance();

        signaler.connect_closure("error", false,
            glib::closure!(@weak-allow-none element => move |_signaler: glib::Object, error: String| {
                let element = element.unwrap();
                gst::element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Signalling error: {}", error]
                );
            })
        );

        signaler.connect(
            "end-session",
            false,
            glib::clone!(@weak element => @default-panic, move |_values| {
                gst::debug!(CAT, "Session ended.");
                gst::element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Peer ended session"]
                );

                None
            }),
        );

        signaler.connect_closure(
            "get-meta",
            false,
            glib::closure!(@weak-allow-none element => move |_signaler: glib::Object| {
                element.unwrap().imp().settings.lock().unwrap().meta.clone()
            }),
        );

        signaler.connect_closure(
            "sdp-offer",
            false,
            glib::closure!(@weak-allow-none element => move
                    |_signaler: glib::Object, offer: gst_webrtc::WebRTCSessionDescription| {
                element.unwrap().imp().handle_offer(offer);
            }),
        );

        // sdp_mid is exposed for future proofing, see
        // https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/1174,
        // at the moment sdp_m_line_index must be Some
        signaler.connect_closure("handle-ice", false,
            glib::closure!(@weak-allow-none element => move
                    |_signaler: glib::Object, peer_id: &str, sdp_m_line_index: u32, _sdp_mid: Option<String>, candidate: &str, | {

                element.unwrap().imp().handle_ice( peer_id, Some(sdp_m_line_index), None, candidate);
            })
        );
    }

    /// When using a custom signaller
    pub fn set_signaller(&self, signaller: gst::Object) {
        let mut state = self.state.lock().unwrap();

        let sigobj = signaller.clone();
        self.connect_signaller(&sigobj);

        state.signaller = signaller;
    }

    /// Called by the signaller when it has encountered an error
    pub fn handle_signalling_error(&self, element: &super::WebRTCSrc, error: anyhow::Error) {
        gst::error!(CAT, obj: element, "Signalling error: {:?}", error);

        gst::element_error!(
            element,
            gst::StreamError::Failed,
            ["Signalling error: {:?}", error]
        );
    }

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

    pub fn handle_offer(&self, offer: gst_webrtc::WebRTCSessionDescription) {
        let sdp = offer.sdp();
        let direction = gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly;

        let webrtcbin = self.webrtcbin();
        for media in sdp.medias() {
            // let mut global_caps = gst::Caps::new_simple("application/x-unknown", &[]);
            // media.attributes_to_caps(global_caps.get_mut().unwrap()).unwrap();
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

                            media
                                .attributes_to_caps(tmpcaps.get_mut().unwrap())
                                .unwrap();

                            tmpcaps
                        })
                        .ok()
                })
                .collect::<Vec<gst::Caps>>();

            let settings = self.settings.lock().unwrap();
            let mut caps = gst::Caps::new_empty();
            for codec in settings
                .video_codecs
                .iter()
                .chain(settings.audio_codecs.iter())
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

            webrtcbin.emit_by_name::<gst::Object>("add-transceiver", &[&direction, &caps]);
        }

        gst::log!(CAT, "Got offer {}", offer.sdp().to_string());
        webrtcbin.emit_by_name::<()>("set-remote-description", &[&offer, &None::<gst::Promise>]);

        let element = self.instance();
        let promise = gst::Promise::with_change_func(glib::clone!(@weak element => move |reply| {
                element.imp().on_answer_created(reply);
            }
        ));

        webrtcbin.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
    }

    fn on_answer_created(&self, reply: Result<Option<&gst::StructureRef>, gst::PromiseError>) {
        let reply = match reply {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                todo!("Answer creation future got no reponse");
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

        gst::log!(CAT, "Answer: {}", answer.sdp().to_string());

        let signaller = self.state.lock().unwrap().signaller.clone();

        signaller.emit_by_name::<()>("handle-sdp", &[&answer]);
    }

    fn on_ice_candidate(&self, sdp_m_line_index: u32, candidate: String) {
        let signaller = self.state.lock().unwrap().signaller.clone();
        signaller.emit_by_name::<()>("add-ice", &[&candidate, &sdp_m_line_index, &None::<String>]);
    }

    /// Called by the signaller with an ice candidate
    pub fn handle_ice(
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
}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCSrc {
    const NAME: &'static str = "RsWebRTCSrc";
    type Type = super::WebRTCSrc;
    type ParentType = gst::Bin;
    type Interfaces = (gst::ChildProxy, gst::URIHandler);
}

impl ObjectImpl for WebRTCSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                gst::ParamSpecArray::new(
                    "video-codecs",
                    "video encoding formats",
                    "Governs what video codecs will be accepted",
                    Some(&glib::ParamSpecString::new(
                        "video-codec",
                        "Video Codec Name",
                        "Video Codec Name",
                        None,
                        glib::ParamFlags::READWRITE,
                    )),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                gst::ParamSpecArray::new(
                    "audio-codecs",
                    "Audio encoding formats",
                    "Governs what audio codecs will be accepted",
                    Some(&glib::ParamSpecString::new(
                        "audio-codec",
                        "Audio Codec Name",
                        "Audio Codec Name",
                        None,
                        glib::ParamFlags::READWRITE,
                    )),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "stun-server",
                    "STUN Server",
                    "The STUN server of the form stun://hostname:port",
                    DEFAULT_STUN_SERVER,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecString::new(
                    "turn-server",
                    "TURN Server",
                    "The TURN server of the form turn(s)://username:password@host:port.",
                    None,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecBoxed::new(
                    "meta",
                    "Meta",
                    "Free form metadata about the producer",
                    gst::Structure::static_type(),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecString::new(
                    "uri",
                    "Uri",
                    "Signaller URI",
                    None,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecObject::new(
                    "signaller",
                    "Signaller",
                    "The Signaller GObject",
                    gst::Object::static_type(),
                    glib::ParamFlags::READWRITE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "video-codecs" => {
                let mut settings = self.settings.lock().unwrap();
                settings.video_codecs = value.get::<gst::Array>().expect("type checked upstream");
            }
            "audio-codecs" => {
                let mut settings = self.settings.lock().unwrap();
                settings.audio_codecs = value.get::<gst::Array>().expect("type checked upstream");
            }
            "stun-server" => {
                self.webrtcbin()
                    .set_property_from_value(pspec.name(), value);
            }
            "turn-server" => {
                self.webrtcbin()
                    .set_property_from_value(pspec.name(), value);
            }
            "meta" => {
                let mut settings = self.settings.lock().unwrap();
                settings.meta = value
                    .get::<Option<gst::Structure>>()
                    .expect("type checked upstream")
            }
            "uri" => {
                let uri = value.get::<&str>().expect("type checked upstream");
                if let Err(err) = obj.set_uri(uri) {
                    gst::error!(CAT, "{err:?}");
                }
            }
            "signaller" => {
                self.set_signaller(value.get::<gst::Object>().expect("type checked upstream"));
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "video-codecs" => {
                let settings = self.settings.lock().unwrap();
                settings.video_codecs.to_value()
            }
            "audio-codecs" => {
                let settings = self.settings.lock().unwrap();
                settings.audio_codecs.to_value()
            }
            "stun-server" => self.webrtcbin().property_value(pspec.name()),
            "turn-server" => self.webrtcbin().property_value(pspec.name()),
            "meta" => {
                let settings = self.settings.lock().unwrap();
                settings.meta.to_value()
            }
            "uri" => self
                .state
                .lock()
                .unwrap()
                .signaller
                .property_value("address"),
            "signaller" => self.state.lock().unwrap().signaller.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);
        let signaller = self.state.lock().unwrap().signaller.clone();

        self.connect_signaller(&signaller);

        obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
        obj.set_element_flags(gst::ElementFlags::SOURCE);
    }
}

impl GstObjectImpl for WebRTCSrc {}

impl ElementImpl for WebRTCSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "WebRTCSrc",
                "Source/Network/WebRTC",
                "WebRTC Src",
                "Thibault Saunier <tsaunier@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("application/x-rtp").build())
                .build();
            let pad_template = gst::PadTemplate::new(
                "src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps,
            )
            .unwrap();

            vec![pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if let gst::StateChange::NullToReady = transition {
            if let Err(err) = self.prepare(element) {
                gst::element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Failed to prepare: {}", err]
                );
                return Err(gst::StateChangeError);
            }
        }

        let mut ret = self.parent_change_state(element, transition);

        match transition {
            gst::StateChange::PausedToReady => {
                if let Err(err) = self.unprepare(element) {
                    gst::element_error!(
                        element,
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
                let mut state = self.state.lock().unwrap();
                state.maybe_start_signaller(element);
            }
            _ => (),
        }

        ret
    }
}

impl BinImpl for WebRTCSrc {}

impl ChildProxyImpl for WebRTCSrc {
    fn child_by_index(&self, _object: &Self::Type, _index: u32) -> Option<glib::Object> {
        None
    }

    fn children_count(&self, _object: &Self::Type) -> u32 {
        0
    }

    fn child_by_name(&self, _object: &Self::Type, name: &str) -> Option<glib::Object> {
        match name {
            "signaller" => Some(self.state.lock().unwrap().signaller.clone().upcast()),
            _ => None,
        }
    }
}

impl URIHandlerImpl for WebRTCSrc {
    const URI_TYPE: gst::URIType = gst::URIType::Src;

    fn protocols() -> &'static [&'static str] {
        &["gstwebrtc", "gstwebrtcs"]
    }

    fn uri(&self, _element: &Self::Type) -> Option<String> {
        todo!("Implement me!")
    }

    fn set_uri(&self, _element: &Self::Type, uri: &str) -> Result<(), glib::Error> {
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
        gst::error!(CAT, "Setting uri: {url_str}");

        self.state
            .lock()
            .unwrap()
            .signaller
            .set_property("address", &url_str);

        Ok(())
    }
}
