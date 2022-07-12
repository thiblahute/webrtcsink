mod imp;
use gst::glib;

use once_cell::sync::Lazy;
// Expose traits and objects from the module itself so it exactly looks like
// generated bindings
pub use imp::WebRTCSignallerRole;
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

mod iface {
    use gst::glib;
    use gst::glib::closure::ToClosureReturnValue;
    use gst::glib::subclass::*;
    use gst::prelude::*;
    use gst::subclass::prelude::*;
    use once_cell::sync::Lazy;

    #[derive(Copy, Clone)]
    pub struct Signallable {
        _parent: glib::gobject_ffi::GTypeInterface,
        pub vstart: fn(&super::Signallable),
        pub vstop: fn(&super::Signallable),
        pub send_sdp: fn(&super::Signallable, String, &gst_webrtc::WebRTCSessionDescription),
        pub add_ice: fn(&super::Signallable, String, &str, Option<u32>, Option<String>),
        pub end_session: fn(&super::Signallable, &str),
    }

    impl Signallable {
        fn request_meta(_iface: &super::Signallable) -> Option<gst::Structure> {
            None
        }
        fn vstart(_iface: &super::Signallable) {}
        fn start(iface: &super::Signallable) {
            iface.vstart();
        }
        fn vstop(_iface: &super::Signallable) {}
        fn stop(iface: &super::Signallable) {
            iface.vstop();
        }
        fn send_sdp(
            _iface: &super::Signallable,
            _session_id: String,
            _sdp: &gst_webrtc::WebRTCSessionDescription,
        ) {
        }
        fn add_ice(
            _iface: &super::Signallable,
            _session_id: String,
            _candidate: &str,
            _sdp_m_line_index: Option<u32>,
            _sdp_mid: Option<String>,
        ) {
        }
        fn end_session(_iface: &super::Signallable, _session_id: &str) {}
    }

    #[glib::object_interface]
    unsafe impl prelude::ObjectInterface for Signallable {
        const NAME: &'static ::std::primitive::str = "Signallable";
        type Prerequisites = (gst::Object,);

        fn interface_init(&mut self) {
            fn vstart_default_trampoline(this: &super::Signallable) {
                Signallable::vstart(this)
            }
            self.vstart = vstart_default_trampoline;
            fn vstop_default_trampoline(this: &super::Signallable) {
                Signallable::vstop(this)
            }
            self.vstop = vstop_default_trampoline;
            fn send_sdp_default_trampoline(
                this: &super::Signallable,
                session_id: String,
                sdp: &gst_webrtc::WebRTCSessionDescription,
            ) {
                Signallable::send_sdp(this, session_id, sdp)
            }
            self.send_sdp = send_sdp_default_trampoline;
            fn add_ice_default_trampoline(
                this: &super::Signallable,
                session_id: String,
                candidate: &str,
                sdp_m_line_index: Option<u32>,
                sdp_mid: Option<String>,
            ) {
                Signallable::add_ice(this, session_id, candidate, sdp_m_line_index, sdp_mid)
            }
            self.add_ice = add_ice_default_trampoline;
            fn end_session_default_trampoline(this: &super::Signallable, session_id: &str) {
                Signallable::end_session(this, session_id)
            }
            self.end_session = end_session_default_trampoline;
        }

        fn properties() -> &'static [glib::ParamSpec] {
            use glib::ParamSpecBuilderExt;
            static PROPS: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
                vec![
                    glib::ParamSpecString::builder("address")
                        .flags(glib::ParamFlags::READWRITE)
                        .build(),
                    glib::ParamSpecString::builder("cafile")
                        .flags(glib::ParamFlags::READWRITE)
                        .build(),
                ]
            });
            PROPS.as_ref()
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: Lazy<Vec<Signal>> = Lazy::new(|| {
                vec![
                    {
                        let builder = Signal::builder("session-ended");
                        let builder = builder.param_types([str::static_type()]);
                        builder.build()
                    },
                    {
                        Signal::builder("producer-added")
                            .param_types([
                                str::static_type(),
                                <Option<gst::Structure>>::static_type(),
                            ])
                            .build()
                    },
                    {
                        Signal::builder("producer-removed")
                            .param_types([
                                str::static_type(),
                                <Option<gst::Structure>>::static_type(),
                            ])
                            .build()
                    },
                    {
                        Signal::builder("session-started")
                            .param_types([str::static_type(), str::static_type()])
                            .build()
                    },
                    {
                        Signal::builder("session-requested")
                            .param_types([str::static_type()])
                            .build()
                    },
                    {
                        Signal::builder("error")
                            .param_types([str::static_type()])
                            .build()
                    },
                    {
                        Signal::builder("request-meta")
                            .return_type::<Option<gst::Structure>>()
                            .flags(glib::SignalFlags::RUN_FIRST)
                            .class_handler(|_token, args| {
                                let arg0 = args[0usize]
                                    .get::<&super::Signallable>()
                                    .unwrap_or_else(|e| {
                                        panic!("Wrong type for argument {}: {:?}", 0usize, e)
                                    });
                                Signallable::request_meta(arg0).to_closure_return_value()
                            })
                            .build()
                    },
                    {
                        Signal::builder("handle-ice")
                            .param_types([
                                str::static_type(),
                                u32::static_type(),
                                <Option<String>>::static_type(),
                                str::static_type(),
                            ])
                            .build()
                    },
                    {
                        Signal::builder("sdp-offer")
                            .param_types([
                                str::static_type(),
                                gst_webrtc::WebRTCSessionDescription::static_type(),
                            ])
                            .build()
                    },
                    {
                        Signal::builder("sdp-answer")
                            .param_types([
                                str::static_type(),
                                gst_webrtc::WebRTCSessionDescription::static_type(),
                            ])
                            .build()
                    },
                    {
                        Signal::builder("start")
                            .flags(glib::SignalFlags::ACTION)
                            .class_handler(|_token, args| {
                                let arg0 = args[0usize]
                                    .get::<&super::Signallable>()
                                    .unwrap_or_else(|e| {
                                        panic!("Wrong type for argument {}: {:?}", 0usize, e)
                                    });
                                Signallable::start(arg0);

                                None
                            })
                            .build()
                    },
                    {
                        Signal::builder("stop")
                            .flags(glib::SignalFlags::ACTION)
                            .class_handler(|_tokens, args| {
                                let arg0 = args[0usize]
                                    .get::<&super::Signallable>()
                                    .unwrap_or_else(|e| {
                                        panic!("Wrong type for argument {}: {:?}", 0usize, e)
                                    });
                                Signallable::stop(arg0);

                                None
                            })
                            .build()
                    },
                ]
            });
            SIGNALS.as_ref()
        }
    }

    unsafe impl<Obj: SignallableImpl> types::IsImplementable<Obj> for super::Signallable
    where
        <Obj as types::ObjectSubclass>::Type: glib::IsA<glib::Object>,
    {
        fn interface_init(iface: &mut glib::Interface<Self>) {
            let iface = ::std::convert::AsMut::as_mut(iface);

            fn vstart_trampoline<Obj: types::ObjectSubclass + SignallableImpl>(
                this: &super::Signallable,
            ) {
                let this = this
                    .dynamic_cast_ref::<<Obj as types::ObjectSubclass>::Type>()
                    .unwrap();
                SignallableImpl::vstart(this.imp(), this)
            }
            iface.vstart = vstart_trampoline::<Obj>;

            fn vstop_trampoline<Obj: types::ObjectSubclass + SignallableImpl>(
                this: &super::Signallable,
            ) {
                let this = this
                    .dynamic_cast_ref::<<Obj as types::ObjectSubclass>::Type>()
                    .unwrap();
                SignallableImpl::vstop(this.imp(), this)
            }
            iface.vstop = vstop_trampoline::<Obj>;

            fn send_sdp_trampoline<Obj: types::ObjectSubclass + SignallableImpl>(
                this: &super::Signallable,
                session_id: String,
                sdp: &gst_webrtc::WebRTCSessionDescription,
            ) {
                let this = this
                    .dynamic_cast_ref::<<Obj as types::ObjectSubclass>::Type>()
                    .unwrap();
                SignallableImpl::send_sdp(this.imp(), this, session_id, sdp)
            }
            iface.send_sdp = send_sdp_trampoline::<Obj>;

            fn add_ice_trampoline<Obj: types::ObjectSubclass + SignallableImpl>(
                this: &super::Signallable,
                session_id: String,
                candidate: &str,
                sdp_m_line_index: Option<u32>,
                sdp_mid: Option<String>,
            ) {
                let this = this
                    .dynamic_cast_ref::<<Obj as types::ObjectSubclass>::Type>()
                    .unwrap();
                SignallableImpl::add_ice(
                    this.imp(),
                    this,
                    session_id,
                    candidate,
                    sdp_m_line_index,
                    sdp_mid,
                )
            }
            iface.add_ice = add_ice_trampoline::<Obj>;

            fn end_session_trampoline<Obj: types::ObjectSubclass + SignallableImpl>(
                this: &super::Signallable,
                session_id: &str,
            ) {
                let this = this
                    .dynamic_cast_ref::<<Obj as types::ObjectSubclass>::Type>()
                    .unwrap();
                SignallableImpl::end_session(this.imp(), this, session_id)
            }
            iface.end_session = end_session_trampoline::<Obj>;
        }
    }

    pub trait SignallableImpl: object::ObjectImpl + 'static {
        fn vstart(&self, this: &<Self as types::ObjectSubclass>::Type) {
            #![inline]
            SignallableImplExt::parent_vstart(self, this)
        }
        fn vstop(&self, this: &<Self as types::ObjectSubclass>::Type) {
            #![inline]
            SignallableImplExt::parent_vstop(self, this)
        }
        fn send_sdp(
            &self,
            this: &<Self as types::ObjectSubclass>::Type,
            session_id: String,
            sdp: &gst_webrtc::WebRTCSessionDescription,
        ) {
            #![inline]
            SignallableImplExt::parent_send_sdp(self, this, session_id, sdp)
        }
        fn add_ice(
            &self,
            this: &<Self as types::ObjectSubclass>::Type,
            session_id: String,
            candidate: &str,
            sdp_m_line_index: Option<u32>,
            sdp_mid: Option<String>,
        ) {
            #![inline]
            SignallableImplExt::parent_add_ice(
                self,
                this,
                session_id,
                candidate,
                sdp_m_line_index,
                sdp_mid,
            )
        }
        fn end_session(&self, this: &<Self as types::ObjectSubclass>::Type, session_id: &str) {
            #![inline]
            SignallableImplExt::parent_end_session(self, this, session_id)
        }
    }

    pub trait SignallableImplExt: types::ObjectSubclass {
        fn parent_vstart(&self, obj: &<Self as types::ObjectSubclass>::Type);
        fn parent_vstop(&self, obj: &<Self as types::ObjectSubclass>::Type);
        fn parent_send_sdp(
            &self,
            obj: &<Self as types::ObjectSubclass>::Type,
            session_id: String,
            sdp: &gst_webrtc::WebRTCSessionDescription,
        );
        fn parent_add_ice(
            &self,
            obj: &<Self as types::ObjectSubclass>::Type,
            session_id: String,
            candidate: &str,
            sdp_m_line_index: Option<u32>,
            sdp_mid: Option<String>,
        );
        fn parent_end_session(&self, obj: &<Self as types::ObjectSubclass>::Type, session_id: &str);
    }

    type ClassType = *mut <super::Signallable as glib::object::ObjectType>::GlibClassType;
    impl<Obj: SignallableImpl> SignallableImplExt for Obj {
        fn parent_vstart(&self, this: &<Self as types::ObjectSubclass>::Type) {
            #![inline]
            let this = unsafe { this.unsafe_cast_ref::<super::Signallable>() };
            let vtable = unsafe {
                &*(Self::type_data()
                    .as_ref()
                    .parent_interface::<super::Signallable>() as ClassType)
            };
            (vtable.vstart)(this)
        }
        fn parent_vstop(&self, this: &<Self as types::ObjectSubclass>::Type) {
            #![inline]
            let this = unsafe { this.unsafe_cast_ref::<super::Signallable>() };
            let vtable = unsafe {
                &*(Self::type_data()
                    .as_ref()
                    .parent_interface::<super::Signallable>() as ClassType)
            };
            (vtable.vstop)(this)
        }
        fn parent_send_sdp(
            &self,
            this: &<Self as types::ObjectSubclass>::Type,
            session_id: String,
            sdp: &gst_webrtc::WebRTCSessionDescription,
        ) {
            #![inline]
            let this = unsafe { this.unsafe_cast_ref::<super::Signallable>() };
            let vtable = unsafe {
                &*(Self::type_data()
                    .as_ref()
                    .parent_interface::<super::Signallable>() as ClassType)
            };
            (vtable.send_sdp)(this, session_id, sdp)
        }
        fn parent_add_ice(
            &self,
            this: &<Self as types::ObjectSubclass>::Type,
            session_id: String,
            candidate: &str,
            sdp_m_line_index: Option<u32>,
            sdp_mid: Option<String>,
        ) {
            #![inline]
            let this = unsafe { this.unsafe_cast_ref::<super::Signallable>() };
            let vtable = unsafe {
                &*(Self::type_data()
                    .as_ref()
                    .parent_interface::<super::Signallable>() as ClassType)
            };
            (vtable.add_ice)(this, session_id, candidate, sdp_m_line_index, sdp_mid)
        }
        fn parent_end_session(
            &self,
            this: &<Self as types::ObjectSubclass>::Type,
            session_id: &str,
        ) {
            #![inline]
            let this = unsafe { this.unsafe_cast_ref::<super::Signallable>() };
            let vtable = unsafe {
                &*(Self::type_data()
                    .as_ref()
                    .parent_interface::<super::Signallable>() as ClassType)
            };
            (vtable.end_session)(this, session_id)
        }
    }

    pub trait SignallableExt: 'static {
        fn vstart(&self);
        fn vstop(&self);
        fn send_sdp(&self, session_id: String, sdp: &gst_webrtc::WebRTCSessionDescription);
        fn add_ice(
            &self,
            session_id: String,
            candidate: &str,
            sdp_m_line_index: Option<u32>,
            sdp_mid: Option<String>,
        );
        fn end_session(&self, session_id: &str);
    }

    impl<Obj: glib::IsA<super::Signallable>> SignallableExt for Obj {
        fn vstart(&self) {
            #![inline]
            let obj = self.upcast_ref::<super::Signallable>();
            let vtable = obj.interface::<super::Signallable>().unwrap();
            let vtable = vtable.as_ref();
            (vtable.vstart)(obj)
        }

        fn vstop(&self) {
            #![inline]
            let obj = self.upcast_ref::<super::Signallable>();
            let vtable = obj.interface::<super::Signallable>().unwrap();
            let vtable = vtable.as_ref();
            (vtable.vstop)(obj)
        }

        fn send_sdp(&self, session_id: String, sdp: &gst_webrtc::WebRTCSessionDescription) {
            #![inline]
            let obj = self.upcast_ref::<super::Signallable>();
            let vtable = obj.interface::<super::Signallable>().unwrap();
            let vtable = vtable.as_ref();
            (vtable.send_sdp)(obj, session_id, sdp)
        }

        fn add_ice(
            &self,
            session_id: String,
            candidate: &str,
            sdp_m_line_index: Option<u32>,
            sdp_mid: Option<String>,
        ) {
            #![inline]
            let obj = self.upcast_ref::<super::Signallable>();
            let vtable = obj.interface::<super::Signallable>().unwrap();
            let vtable = vtable.as_ref();
            (vtable.add_ice)(obj, session_id, candidate, sdp_m_line_index, sdp_mid)
        }

        fn end_session(&self, session_id: &str) {
            #![inline]
            let obj = self.upcast_ref::<super::Signallable>();
            let vtable = obj.interface::<super::Signallable>().unwrap();
            let vtable = vtable.as_ref();
            (vtable.end_session)(obj, session_id)
        }
    }
}

glib::wrapper! {
    pub struct Signallable(ObjectInterface<iface::Signallable>) @requires gst::Object ;
}

glib::wrapper! {
    pub struct Signaller(ObjectSubclass <imp::Signaller>) @extends gst::Object, @implements Signallable;
}

impl Signaller {
    pub fn default() -> Self {
        glib::Object::new(&[]).unwrap()
    }

    pub fn new(mode: WebRTCSignallerRole) -> Self {
        glib::Object::new(&[("mode", &mode)]).unwrap()
    }
}

pub use iface::SignallableExt;
pub use iface::SignallableImpl;
pub use iface::SignallableImplExt;

unsafe impl Send for Signallable {}
unsafe impl Sync for Signallable {}
