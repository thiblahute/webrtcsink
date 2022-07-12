use gst::{glib, prelude::StaticType};

mod imp;
pub mod signaller;

glib::wrapper! {
    pub struct WebRTCSrc(ObjectSubclass<imp::WebRTCSrc>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::URIHandler, gst::ChildProxy;
}

pub fn register(plugin: Option<&gst::Plugin>) -> Result<(), glib::BoolError> {
    gst::Element::register(
        plugin,
        "webrtcsrc",
        gst::Rank::Primary,
        WebRTCSrc::static_type(),
    )
}
