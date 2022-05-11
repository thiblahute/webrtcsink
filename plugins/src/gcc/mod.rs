use gst::glib;
use gst::prelude::*;
mod imp;

glib::wrapper! {
    pub struct BandwidthEstimator(ObjectSubclass<imp::BandwidthEstimator>) @extends gst::Element, gst::Object;
}

unsafe impl Send for BandwidthEstimator {}
unsafe impl Sync for BandwidthEstimator {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtpgccbwe",
        gst::Rank::None,
        BandwidthEstimator::static_type(),
    )
}
