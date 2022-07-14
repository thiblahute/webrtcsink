mod imp;

use once_cell::sync::Lazy;
// Expose traits and objects from the module itself so it exactly looks like
// generated bindings
pub use imp::Signaller;
pub mod prelude {
    pub use {super::SignallableExt, super::SignallableImpl};
}

pub static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtcsrc-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC src signaller"),
    )
});

#[gobject::interface(requires(gst::Object))]
mod iface {
    use std::sync::Mutex;

    #[derive(Copy, Clone)]
    pub struct Signallable {
        #[property(get, set)]
        address: Mutex<String>,
        #[property(get, set)]
        cafile: Mutex<Option<String>>,
    }

    impl Signallable {
        #[signal]
        fn session_ended(iface: &super::Signallable, peer_id: &str) {}
        #[signal]
        fn producer_added(iface: &super::Signallable, peer_id: &str, meta: Option<gst::Structure>) {
        }
        #[signal]
        fn producer_removed(
            iface: &super::Signallable,
            peer_id: &str,
            meta: Option<gst::Structure>,
        ) {
        }
        #[signal]
        fn error(iface: &super::Signallable, error: &str) {}
        #[signal(run_first)]
        fn request_meta(_iface: &super::Signallable) -> Option<gst::Structure> {
            None
        }
        #[signal]
        fn handle_ice(
            iface: &super::Signallable,
            peer_id: &str,
            sdp_m_line_index: u32,
            sdp_mid: Option<String>,
            candidate: &str,
        ) {
        }
        #[signal]
        fn sdp_offer(
            _iface: &super::Signallable,
            peer_id: &str,
            _sdp: &gst_webrtc::WebRTCSessionDescription,
        ) {
        }

        #[virt]
        fn start(_iface: &super::Signallable) {}
        #[virt]
        fn stop(_iface: &super::Signallable) {}
        #[virt]
        fn handle_sdp(_iface: &super::Signallable, _sdp: &gst_webrtc::WebRTCSessionDescription) {}
        #[virt]
        fn add_ice(
            _iface: &super::Signallable,
            _candidate: &str,
            _sdp_m_line_index: Option<u32>,
            _sdp_mid: Option<String>,
        ) {
        }
    }
}

unsafe impl Send for Signallable {}
unsafe impl Sync for Signallable {}
