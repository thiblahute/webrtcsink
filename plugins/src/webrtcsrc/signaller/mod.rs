use gst::glib;

mod imp;

glib::wrapper! {
    pub struct SourceSignaller(ObjectSubclass<imp::SourceSignaller>) @extends gst::Object;
}

impl Default for SourceSignaller {
    fn default() -> Self {
        glib::Object::new(&[]).unwrap()
    }
}
