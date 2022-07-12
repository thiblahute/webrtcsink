use crate::webrtcsink::{Signallable, WebRTCSink};
use gst::glib;
use gst::subclass::prelude::ObjectSubclassExt;
use std::error::Error;

mod imp;

glib::wrapper! {
    pub struct SinkSignaller(ObjectSubclass<imp::SinkSignaller>);
}

unsafe impl Send for SinkSignaller {}
unsafe impl Sync for SinkSignaller {}

impl Signallable for SinkSignaller {
    fn start(&mut self, element: &WebRTCSink) -> Result<(), Box<dyn Error>> {
        let signaller = imp::SinkSignaller::from_instance(self);
        signaller.start(element);

        Ok(())
    }

    fn handle_sdp(
        &mut self,
        element: &WebRTCSink,
        peer_id: &str,
        sdp: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<(), Box<dyn Error>> {
        let signaller = imp::SinkSignaller::from_instance(self);
        signaller.handle_sdp(element, peer_id, sdp);
        Ok(())
    }

    fn handle_ice(
        &mut self,
        element: &WebRTCSink,
        peer_id: &str,
        candidate: &str,
        sdp_mline_index: Option<u32>,
        sdp_mid: Option<String>,
    ) -> Result<(), Box<dyn Error>> {
        let signaller = imp::SinkSignaller::from_instance(self);
        signaller.handle_ice(element, peer_id, candidate, sdp_mline_index, sdp_mid);
        Ok(())
    }

    fn stop(&mut self, element: &WebRTCSink) {
        let signaller = imp::SinkSignaller::from_instance(self);
        signaller.stop(element);
    }

    fn consumer_removed(&mut self, element: &WebRTCSink, peer_id: &str) {
        let signaller = imp::SinkSignaller::from_instance(self);
        signaller.consumer_removed(element, peer_id);
    }
}

impl Default for SinkSignaller {
    fn default() -> Self {
        glib::Object::new(&[]).unwrap()
    }
}
