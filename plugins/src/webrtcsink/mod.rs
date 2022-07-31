use crate::signaller::Signallable;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

mod homegrown_cc;

mod imp;

glib::wrapper! {
    pub struct WebRTCSink(ObjectSubclass<imp::WebRTCSink>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

unsafe impl Send for WebRTCSink {}
unsafe impl Sync for WebRTCSink {}

#[derive(thiserror::Error, Debug)]
pub enum WebRTCSinkError {
    #[error("no consumer with id")]
    NoConsumerWithId(String),
    #[error("consumer refused media")]
    ConsumerRefusedMedia { peer_id: String, media_idx: u32 },
    #[error("consumer did not provide valid payload for media")]
    ConsumerNoValidPayload { peer_id: String, media_idx: u32 },
    #[error("SDP mline index is currently mandatory")]
    MandatorySdpMlineIndex,
    #[error("duplicate consumer id")]
    DuplicateConsumerId(String),
    #[error("error setting up consumer pipeline")]
    ConsumerPipelineError { peer_id: String, details: String },
}

impl Default for WebRTCSink {
    fn default() -> Self {
        glib::Object::new(&[]).unwrap()
    }
}

impl WebRTCSink {
    pub fn with_signaller(signaller: Signallable) -> Self {
        let ret: WebRTCSink = glib::Object::new(&[]).unwrap();

        let ws = imp::WebRTCSink::from_instance(&ret);

        ws.set_signaller(signaller).unwrap();

        ret
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstWebRTCSinkCongestionControl")]
pub enum WebRTCSinkCongestionControl {
    #[enum_value(name = "Disabled: no congestion control is applied", nick = "disabled")]
    Disabled,
    #[enum_value(name = "Homegrown: simple sender-side heuristic", nick = "homegrown")]
    Homegrown,
    #[enum_value(name = "Google Congestion Control algorithm", nick = "gcc")]
    GoogleCongestionControl,
}

#[glib::flags(name = "GstWebRTCSinkMitigationMode")]
enum WebRTCSinkMitigationMode {
    #[flags_value(name = "No mitigation applied", nick = "none")]
    NONE = 0b00000000,
    #[flags_value(name = "Lowered resolution", nick = "downscaled")]
    DOWNSCALED = 0b00000001,
    #[flags_value(name = "Lowered framerate", nick = "downsampled")]
    DOWNSAMPLED = 0b00000010,
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "webrtcsink",
        gst::Rank::None,
        WebRTCSink::static_type(),
    )
}
