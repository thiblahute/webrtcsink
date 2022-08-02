use gst::{glib, prelude::*};

pub mod signaller;

const RTP_TWCC_URI: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

#[gobject::gst_element(
    class(final, extends(gst::Bin), implements(gst::ChildProxy, gst::URIHandler)),
    factory_name = "webrtcsrc",
    rank = "Primary",
    long_name = "WebRTCSrc",
    classification = "Source/Network/WebRTC",
    description = "WebRTC Src",
    author = "Thibault Saunier <tsaunier@igalia.com>",
    pad_templates(src__u(presence = "Sometimes", caps = "application/x-rtp"),)
)]
mod imp {
    use crate::webrtcsrc::signaller::{prelude::*, Signallable, Signaller};
    use anyhow::{Context, Error};
    use gst::glib;
    use gst::prelude::*;
    use gst::subclass::prelude::*;
    use once_cell::sync::Lazy;
    use std::ops::ControlFlow;
    use std::str::FromStr;
    use std::sync::Mutex;
    use url::Url;

    struct WebRTCSrc {
        #[property(
            get = "_",
            set = "_",
            blurb = "The STUN server of the form stun://hostname:port",
            lax_validation
        )]
        stun_server: Mutex<String>,

        #[property(
            get = "_",
            set = "_",
            object,
            lax_validation,
            blurb = "The Signallable object to use to handle WebRTC Signalling"
        )]
        signaller: Mutex<Signallable>,

        #[property(get, set, boxed)]
        meta: Mutex<Option<gst::Structure>>,

        #[property(get, set, blurb = "Names of usable video codecs")]
        video_codecs: Mutex<gst::Array>,

        #[property(get, set, blurb = "Names of usable audio codecs")]
        audio_codecs: Mutex<gst::Array>,

        state: Mutex<State>,
    }

    impl Default for WebRTCSrc {
        fn default() -> Self {
            let signaller = Signaller::default();

            Self {
                stun_server: Mutex::new("stun://stun.l.google.com:19302".to_string()),
                state: Default::default(),
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
        session_ended: glib::SignalHandlerId,
        request_meta: glib::SignalHandlerId,
        sdp_offer: glib::SignalHandlerId,
        handle_ice: glib::SignalHandlerId,
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
            let caps = pad.current_caps().unwrap();
            gst::info!(CAT, "Pad added with caps: {}", caps.to_string());
            let template = instance.pad_template("src_%u").unwrap();
            let name = format!("src_{}", instance.src_pads().len());
            let src_pad = gst::GhostPad::builder_with_template(&template, Some(&name))
                .proxy_pad_chain_function(move |#[weak(or_panic)] instance, pad, parent, buffer| {
                    let this = instance.imp();
                    let padret = pad.chain_default(parent, buffer);
                    let ret = this.state.lock().unwrap().flow_combiner.update_flow(padret);

                    ret
                })
                .proxy_pad_event_function(move |#[weak(or_panic)] instance, pad, parent, event| {
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
                        if !caps.is_subset(&stream.caps().unwrap()) {
                            return false;
                        }
                        instance
                            .src_pads()
                            .iter()
                            .find(|pad| {
                                let mut already_used = false;

                                pad.sticky_events_foreach(|event| {
                                    if let gst::EventView::StreamStart(stream_start) = event.view()
                                    {
                                        if Some(stream_start.stream_id().into())
                                            == stream.stream_id()
                                        {
                                            already_used = true;
                                            ControlFlow::Break(gst::EventForeachAction::Keep)
                                        } else {
                                            ControlFlow::Continue(gst::EventForeachAction::Keep)
                                        }
                                    } else {
                                        ControlFlow::Continue(gst::EventForeachAction::Keep)
                                    }
                                });

                                already_used == false
                            })
                            .is_some()
                    });

                    let event = stream.map_or_else(
                        || event.clone(),
                        |stream| {
                            let stream_id = stream.stream_id().unwrap();
                            gst::event::StreamStart::builder(stream_id.as_str())
                                .stream(stream)
                                .seqnum(stream_start.seqnum())
                                .group_id(
                                    stream_start
                                        .group_id()
                                        .unwrap_or_else(|| gst::GroupId::next()),
                                )
                                .build()
                        },
                    );

                    pad.event_default(parent, event)
                })
                .build_with_target(pad)
                .unwrap();

            let res = instance.add_pad(&src_pad);

            if res.is_ok() {
                if let Some(collection) = self.state.lock().unwrap().stream_collection.clone() {
                    src_pad.push_event(gst::event::StreamCollection::builder(&collection).build());
                }
            }

            res
        }

        fn prepare(&self) -> Result<(), Error> {
            let webrtcbin = gst::ElementFactory::make("webrtcbin", None)
                .with_context(|| format!("Failed to make element webrtcbin"))
                .unwrap();

            webrtcbin.set_property_from_str("bundle-policy", "max-bundle");
            webrtcbin.set_property_from_str("stun-server", &self.stun_server());
            let instance = self.instance();
            webrtcbin.connect_pad_added(glib::clone!(@weak instance => move |_webrtcbin, pad| {
                if let Err(err) = instance.imp().add_pad(&instance, pad) {
                    gst::error!(CAT, "Could not add pad {}: {err:?}", pad.name());
                }
            }));

            webrtcbin.connect_closure(
                "on-ice-candidate",
                false,
                #[closure]
                move |#[watch] instance,
                      _webrtcbin: gst::Bin,
                      sdp_m_line_index: u32,
                      candidate: String| {
                    let this = Self::from_instance(instance);
                    this.on_ice_candidate(sdp_m_line_index, candidate);
                },
            );

            instance
                .add(&webrtcbin)
                .expect("Could not add `webrtcbin`?");

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

            let _ = self
                .state
                .lock()
                .unwrap()
                .signaller_signals
                .insert(SignallerSignals {
                error: signaler.connect_closure(
                    "error",
                    false,
                    #[closure]
                    move |#[watch] instance, _signaler: glib::Object, error: String| {
                        gst::element_error!(
                            instance,
                            gst::StreamError::Failed,
                            ["Signalling error: {}", error]
                        );
                    },
                ),

                session_ended: signaler.connect_closure(
                    "session-ended",
                    false,
                    #[closure]
                    move |#[watch] instance, _signaller: glib::Object, _peer_id: &str| {
                        gst::debug!(CAT, "Session ended.");
                        gst::element_error!(
                            instance,
                            gst::StreamError::Failed,
                            ["Peer ended session"]
                        );
                    },
                ),

                request_meta: signaler.connect_closure(
                    "request-meta",
                    false,
                    #[closure]
                    move |#[watch] instance, _signaler: glib::Object| -> Option<gst::Structure> {
                        let meta = instance.imp().meta.lock().unwrap().clone();

                        meta
                    },
                ),

                sdp_offer: signaler.connect_closure(
                    "sdp-offer",
                    false,
                    #[closure]
                    move |#[watch] instance,
                          _signaler: glib::Object,
                          _peer_id: &str,
                          offer: &gst_webrtc::WebRTCSessionDescription| {
                        instance.imp().handle_offer(offer);
                    },
                ),

                // sdp_mid is exposed for future proofing, see
                // https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/1174,
                // at the moment sdp_m_line_index must be Some
                handle_ice: signaler.connect_closure(
                    "handle-ice",
                    false,
                    #[closure]
                    move |#[watch] instance,
                          _signaler: glib::Object,
                          peer_id: &str,
                          sdp_m_line_index: u32,
                          _sdp_mid: Option<String>,
                          candidate: &str| {
                        instance
                            .imp()
                            .handle_ice(peer_id, Some(sdp_m_line_index), None, candidate);
                    },
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
                                            match value
                                                .split(" ")
                                                .into_iter()
                                                .collect::<Vec<&str>>()[..]
                                            {
                                                [k, v] => {
                                                    if !v.contains(super::RTP_TWCC_URI) {
                                                        continue;
                                                    }

                                                    if let Ok(twcc_idx) = k.parse::<u32>() {
                                                        tmpcaps
                                                            .get_mut()
                                                            .unwrap()
                                                            .iter_mut()
                                                            .for_each(|s| {
                                                                s.set(
                                                                    "rtcp-fb-transport-cc",
                                                                    &true,
                                                                );
                                                                s.set(
                                                                    &format!("extmap-{}", twcc_idx),
                                                                    super::RTP_TWCC_URI,
                                                                );
                                                            });

                                                        break;
                                                    }
                                                }
                                                _ => (),
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

            let sdp = answer.sdp();
            gst::log!(CAT, "Answer: {}", sdp.to_string());

            let collection = gst::StreamCollection::builder(None)
                .streams(
                    &sdp.medias()
                        .map(|media| {
                            let caps = media.formats().find_map(|format| {
                                format.parse::<i32>().map_or(None, |pt| {
                                    media.caps_from_media(pt).map_or(None, |mut caps| {
                                        // apt: associated payload type. The value of this parameter is the
                                        // payload type of the associated original stream.
                                        if caps.structure(0).unwrap().has_field("apt") {
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

                            gst::Stream::new(
                                None,
                                caps.as_ref(),
                                match media.media() {
                                    Some("video") => gst::StreamType::VIDEO,
                                    Some("audio") => gst::StreamType::AUDIO,
                                    _ => gst::StreamType::UNKNOWN,
                                },
                                gst::StreamFlags::empty(),
                            )
                        })
                        .collect::<Vec<gst::Stream>>(),
                )
                .build();

            if let Err(err) = self
                .instance()
                .post_message(gst::message::StreamCollection::new(&collection))
            {
                gst::error!(CAT, "Could not post stream collection: {:?}", err);
            }
            self.state.lock().unwrap().stream_collection = Some(collection);

            let signaller = self.signaller();
            signaller.send_sdp(None, &answer);
        }

        fn on_ice_candidate(&self, sdp_m_line_index: u32, candidate: String) {
            let signaller = self.signaller();
            signaller.add_ice(None, &candidate, Some(sdp_m_line_index), None::<String>);
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
                instance.signaller().vstart();

                gst::info!(CAT, "Started signaller");
                state.signaller_state = SignallerState::Started;
            }
        }

        fn maybe_stop_signaller(&self) {
            let mut state = self.state.lock().unwrap();
            if state.signaller_state == SignallerState::Started {
                self.instance().signaller().vstop();
                state.signaller_state = SignallerState::Stopped;
                gst::info!(CAT, "Stopped signaller");
            }
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
        signaller_signals: Option<SignallerSignals>,
        stream_collection: Option<gst::StreamCollection>,
    }

    impl Default for State {
        fn default() -> Self {
            Self {
                signaller_state: SignallerState::Stopped,
                webrtcbin: None,
                flow_combiner: Default::default(),
                signaller_signals: Default::default(),
                stream_collection: Default::default(),
            }
        }
    }
}
