use gst::{glib, prelude::*};

mod signaller;

#[gobject::class(
    final,
    extends(gst::Bin, gst::Element, gst::Object),
    implements(gst::ChildProxy, gst::URIHandler)
)]
mod imp {
    use crate::webrtcsrc::signaller::{prelude::*, Signallable, Signaller};
    use anyhow::{Context, Error};
    use gst::glib;
    use gst::prelude::*;
    use gst::subclass::prelude::*;
    use once_cell::sync::Lazy;
    use std::str::FromStr;
    use std::sync::Mutex;
    use url::Url;

    static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
        gst::DebugCategory::new(
            "webrtcsrc",
            gst::DebugColorFlags::empty(),
            Some("WebRTC sink"),
        )
    });

    struct WebRTCSrc {
        #[property(get = "_", set = "_", lax_validation)]
        stun_server: Mutex<String>,
        #[property(get = "_", set = "_", object, lax_validation)]
        signaller: Mutex<Signallable>,
        #[property(get, set, boxed)]
        meta: Mutex<Option<gst::Structure>>,

        state: Mutex<State>,
        settings: Mutex<Settings>,
    }

    impl Default for WebRTCSrc {
        fn default() -> Self {
            let signaller = Signaller::default();

            Self {
                stun_server: Default::default(),
                state: Default::default(),
                settings: Default::default(),
                signaller: Mutex::new(signaller.upcast()),
                meta: Default::default(),
            }
        }
    }

    impl WebRTCSrc {
        // --------------------------
        // Properties implementations

        #[public]
        fn stun_server(&self) -> String {
            self.stun_server.lock().unwrap().clone()
        }

        #[public]
        fn set_stun_server(&self, stun_server: String) {
            self.webrtcbin().set_property("stun-server", &stun_server);

            *self.stun_server.lock().unwrap() = stun_server;
        }

        #[public]
        fn signaller(&self) -> Signallable {
            self.signaller.lock().unwrap().clone()
        }

        /// When using a custom signaller
        #[public]
        fn set_signaller(&self, signaller: Signallable) {
            let sigobj = signaller.clone();
            self.connect_signaller(&sigobj);

            *self.signaller.lock().unwrap() = signaller;
        }

        fn properties() -> Vec<glib::ParamSpec> {
            // GStreamer ParamSpecs are not supported by `gobject`
            // implement those properties manually
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
            ]
        }

        fn set_property(
            &self,
            _obj: &super::WebRTCSrc,
            _id: usize,
            value: &glib::Value,
            pspec: &glib::ParamSpec,
        ) {
            match pspec.name() {
                "video-codecs" => {
                    let mut settings = self.settings.lock().unwrap();
                    settings.video_codecs =
                        value.get::<gst::Array>().expect("type checked upstream");
                }
                "audio-codecs" => {
                    let mut settings = self.settings.lock().unwrap();
                    settings.audio_codecs =
                        value.get::<gst::Array>().expect("type checked upstream");
                }
                _ => unimplemented!(),
            }
        }

        fn property(
            &self,
            _obj: &super::WebRTCSrc,
            _id: usize,
            pspec: &glib::ParamSpec,
        ) -> glib::Value {
            match pspec.name() {
                "video-codecs" => {
                    let settings = self.settings.lock().unwrap();
                    settings.video_codecs.to_value()
                }
                "audio-codecs" => {
                    let settings = self.settings.lock().unwrap();
                    settings.audio_codecs.to_value()
                }
                _ => unimplemented!(),
            }
        }

        // --------------------------
        // GObject overrides
        fn constructed(&self, obj: &super::WebRTCSrc) {
            self.parent_constructed(obj);
            let signaller = self.signaller.lock().unwrap().clone();

            self.connect_signaller(&signaller);

            obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
            obj.set_element_flags(gst::ElementFlags::SOURCE);
        }

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

        #[gobject::clone_block]
        fn add_pad(
            &self,
            instance: &super::WebRTCSrc,
            pad: &gst::Pad,
        ) -> Result<(), glib::error::BoolError> {
            gst::info!(
                CAT,
                "Pad added with caps: {}",
                pad.current_caps().unwrap().to_string()
            );
            let template = instance.pad_template("src_%u").unwrap();
            let src_pad = gst::GhostPad::builder_with_template(&template, None)
                .proxy_pad_chain_function(
                    move |#[weak(or_panic)] instance, pad, parent, buffer| {
                        let this = instance.imp();
                        let padret = pad.chain_default(parent, buffer);
                        let ret = this.state.lock().unwrap().flow_combiner.update_flow(padret);

                        ret
                    },
                )
                .build_with_target(pad)
                .unwrap();

            instance.add_pad(&src_pad)
        }

        fn prepare(&self) -> Result<(), Error> {
            let webrtcbin = gst::ElementFactory::make("webrtcbin", None)
                .with_context(|| format!("Failed to make element webrtcbin"))
                .unwrap();

            webrtcbin.set_property_from_str("bundle-policy", "max-bundle");
            let instance = self.instance();
            webrtcbin.connect_pad_added(glib::clone!(@weak instance => move |_webrtcbin, pad| {
                if let Err(err) = instance.imp().add_pad(&instance, pad) {
                    gst::error!(CAT, "Could not add pad {}: {err:?}", pad.name());
                }
            }));

            webrtcbin.connect_closure(
                "on-ice-candidate",
                false,
                #[closure] move |#[watch] instance, _webrtcbin: gst::Bin, sdp_m_line_index: u32, candidate: String| {
                    let this = Self::from_instance(instance);
                    this.on_ice_candidate(
                        sdp_m_line_index,
                        candidate,
                    );
                },
            );

            instance.add(&webrtcbin).expect("Could not add `webrtcbin`?");

            self.state.lock().unwrap().webrtcbin.replace(webrtcbin);

            Ok(())
        }

        /// Unprepare by stopping consumers, then the signaller object.
        /// Might abort codec discovery
        fn unprepare(&self, instance: &super::WebRTCSrc) -> Result<(), Error> {
            gst::info!(CAT, obj: instance, "unpreparing");

            self.maybe_stop_signaller();

            Ok(())
        }

        fn connect_signaller(&self, signaler: &Signallable) {
            let instance = self.instance();

            // FIXME Port to signaler.connect_error once https://github.com/jf2048/gobject/pull/30/ is merged
            signaler.connect_closure("error", false,
                #[closure] move |#[watch] instance, _signaler: glib::Object, error: String| {
                    gst::element_error!(
                        instance,
                        gst::StreamError::Failed,
                        ["Signalling error: {}", error]
                    );
                }
            );

            signaler.connect_closure(
                "session-ended",
                false,
                #[closure] move |#[watch] instance, _signaller: glib::Object, _peer_id: &str| {
                    gst::debug!(CAT, "Session ended.");
                    gst::element_error!(
                        instance,
                        gst::StreamError::Failed,
                        ["Peer ended session"]
                    );
                },
            );

            // FIXME: signaler.connect_request_meta(
            signaler.connect_closure(
                "request-meta",
                false,
                #[closure] move |#[watch] instance, _signaler: glib::Object| -> Option<gst::Structure> {
                    let meta = instance.imp().meta.lock().unwrap().clone();

                    meta
                },
            );

            // FIXME signaler.connect_sdp_offer(
            signaler.connect_closure("sdp-offer", false,
                #[closure] move |#[watch] instance, _signaler: glib::Object, _peer_id: &str, offer: &gst_webrtc::WebRTCSessionDescription| {
                    instance.imp().handle_offer(offer);
                }
            );

            // sdp_mid is exposed for future proofing, see
            // https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/1174,
            // at the moment sdp_m_line_index must be Some
            signaler.connect_closure("handle-ice", false,
                #[closure] move |#[watch] instance, _signaler: glib::Object, peer_id: &str, sdp_m_line_index: u32, _sdp_mid: Option<String>, candidate: &str, | {
                    instance.imp().handle_ice( peer_id, Some(sdp_m_line_index), None, candidate);
                }
            );
        }

        fn handle_offer(&self, offer: &gst_webrtc::WebRTCSessionDescription) {
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
            webrtcbin
                .emit_by_name::<()>("set-remote-description", &[&offer, &None::<gst::Promise>]);

            let instance = self.instance();
            let promise =
                gst::Promise::with_change_func(glib::clone!(@weak instance => move |reply| {
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

            gst::log!(CAT, "Answer: {}", answer.sdp().to_string());

            let signaller = self.signaller();
            signaller.handle_sdp(&answer);
        }

        fn on_ice_candidate(&self, sdp_m_line_index: u32, candidate: String) {
            let signaller = self.signaller();
            signaller.add_ice(&candidate, Some(sdp_m_line_index), None::<String>);
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
                instance.signaller().start();

                gst::info!(CAT, "Started signaller");
                state.signaller_state = SignallerState::Started;
            }
        }

        fn maybe_stop_signaller(&self) {
            let mut state = self.state.lock().unwrap();
            if state.signaller_state == SignallerState::Started {
                self.instance().signaller().stop();
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
            instance: &Self::Type,
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

    impl BinImpl for WebRTCSrc {}
    impl GstObjectImpl for WebRTCSrc {}

    // --------------------------
    // Interfaces implementation
    impl ChildProxyImpl for WebRTCSrc {
        fn child_by_index(&self, _object: &Self::Type, _index: u32) -> Option<glib::Object> {
            None
        }

        fn children_count(&self, _object: &Self::Type) -> u32 {
            0
        }

        fn child_by_name(&self, object: &Self::Type, name: &str) -> Option<glib::Object> {
            match name {
                "signaller" => {
                    gst::error!(CAT, "Getting signaller");
                    Some(object.signaller().upcast())
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

        fn uri(&self, instance: &Self::Type) -> Option<String> {
            instance.signaller().property::<Option<String>>("address")
        }

        fn set_uri(&self, instance: &Self::Type, uri: &str) -> Result<(), glib::Error> {
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

            instance.signaller().set_property("address", &url_str);

            Ok(())
        }
    }

    #[derive(PartialEq)]
    enum SignallerState {
        Started,
        Stopped,
    }

    struct State {
        signaller_state: SignallerState,
        webrtcbin: Option<gst::Element>,
        flow_combiner: gst_base::UniqueFlowCombiner,
    }

    impl Default for State {
        fn default() -> Self {
            Self {
                signaller_state: SignallerState::Stopped,
                webrtcbin: None,
                flow_combiner: Default::default(),
            }
        }
    }

    struct Settings {
        video_codecs: gst::Array,
        audio_codecs: gst::Array,
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
            }
        }
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "webrtcsrc",
        gst::Rank::Primary,
        WebRTCSrc::static_type(),
    )
}
