use gst::{glib, prelude::*};

mod imp;
mod signaller;

glib::wrapper! {
    pub struct WebRTCSrc(ObjectSubclass<imp::WebRTCSrc>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst::URIHandler;
}

impl Default for WebRTCSrc {
    fn default() -> Self {
        glib::Object::new(&[]).unwrap()
    }
}

impl WebRTCSrc {
    pub fn with_signaller(signaller: glib::Object) -> Self {
        let ret: WebRTCSrc = glib::Object::new(&[("signaller", &signaller)]).unwrap();

        ret
    }
}

unsafe impl Send for WebRTCSrc {}
unsafe impl Sync for WebRTCSrc {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "webrtcsrc",
        gst::Rank::Primary,
        WebRTCSrc::static_type(),
    )
}
