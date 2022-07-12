use gst::glib;

pub mod gcc;
mod signaller;
pub mod webrtcsink;
pub mod webrtcsrc;
pub mod utils;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    webrtcsink::register(plugin)?;
    webrtcsrc::register(plugin)?;
    gcc::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    webrtcsink,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
