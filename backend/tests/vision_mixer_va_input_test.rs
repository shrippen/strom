//! Regression test: VA surfaces must reach the GPU Vision Mixer.
//!
//! Every video input of the GPU Vision Mixer starts with `glupload`, which does
//! not take `memory:VAMemory`. The Media Player re-pushes what its decoder
//! produced from an `appsrc`; behind an AMD or Intel VA decoder that can be VA
//! surfaces and nothing else. The input front splices `vapostproc`, which
//! hands them on as DMABuf, in front of `glupload` when the producer offers VA
//! memory only.
//!
//! These tests drive the real Media Player block into the real Vision Mixer
//! block (see `vision_mixer_cuda`). `vapostproc` needs a VA device, so a
//! stand-in registered under that name takes its place; real VA surfaces need
//! AMD or Intel hardware (HARDWARE-TODO H.35 in lmix).

pub mod common;
pub mod vision_mixer_cuda;

use gstreamer::prelude::*;
use strom::gst::video_adapt::VA_POSTPROC_FACTORY;
use vision_mixer_cuda::*;

const VA_CAPS: &str = "video/x-raw(memory:VAMemory),format=NV12,width=320,height=240,\
     framerate=25/1,pixel-aspect-ratio=1/1,interlace-mode=progressive";

/// A stand-in for `vapostproc` with the same memory types: VA or system
/// memory in, DMABuf or system memory out. The tests only need the element to
/// exist under that name and to accept VA caps on its sink pad.
mod stand_in {
    use gstreamer as gst;
    use gstreamer::glib;
    use gstreamer::prelude::*;
    use gstreamer::subclass::prelude::*;

    mod imp {
        use super::*;
        use std::sync::LazyLock;

        #[derive(Default)]
        pub struct VaPostProcStandIn;

        #[glib::object_subclass]
        impl ObjectSubclass for VaPostProcStandIn {
            const NAME: &'static str = "StromTestVaPostProcStandIn";
            type Type = super::VaPostProcStandIn;
            type ParentType = gst::Element;
        }

        impl ObjectImpl for VaPostProcStandIn {
            fn constructed(&self) {
                self.parent_constructed();
                let obj = self.obj();
                for name in ["sink", "src"] {
                    let templ = obj.element_class().pad_template(name).unwrap();
                    obj.add_pad(&gst::Pad::from_template(&templ)).unwrap();
                }
            }
        }

        impl GstObjectImpl for VaPostProcStandIn {}

        impl ElementImpl for VaPostProcStandIn {
            fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "vapostproc stand-in",
                        "Filter/Video",
                        "Test stand-in for the va vapostproc element",
                        "Strom tests",
                    )
                });
                Some(&META)
            }

            fn pad_templates() -> &'static [gst::PadTemplate] {
                static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                    let sink: gst::Caps =
                        "video/x-raw(memory:VAMemory); video/x-raw".parse().unwrap();
                    let src: gst::Caps = "video/x-raw(memory:DMABuf); video/x-raw".parse().unwrap();
                    vec![
                        gst::PadTemplate::new(
                            "sink",
                            gst::PadDirection::Sink,
                            gst::PadPresence::Always,
                            &sink,
                        )
                        .unwrap(),
                        gst::PadTemplate::new(
                            "src",
                            gst::PadDirection::Src,
                            gst::PadPresence::Always,
                            &src,
                        )
                        .unwrap(),
                    ]
                });
                TEMPLATES.as_ref()
            }
        }
    }

    glib::wrapper! {
        pub struct VaPostProcStandIn(ObjectSubclass<imp::VaPostProcStandIn>)
            @extends gst::Element, gst::Object;
    }

    /// Make [`super::VA_POSTPROC_FACTORY`] available for the rest of the process.
    /// On a host with VA-API the real element is left in place and used.
    ///
    /// Registered once and never removed: `Registry::remove_feature` frees the
    /// factory while the element class keeps a plain pointer to it, so a later
    /// lookup or registration under the same name reads freed memory.
    pub fn register() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            gst::init().expect("GStreamer initialises");
            if gst::ElementFactory::find(super::VA_POSTPROC_FACTORY).is_some() {
                return;
            }
            gst::Element::register(
                None,
                super::VA_POSTPROC_FACTORY,
                gst::Rank::NONE,
                VaPostProcStandIn::static_type(),
            )
            .expect("register the vapostproc stand-in");
        });
    }
}

/// VA memory from the Media Player into a mixer input (video and DSK) gets
/// `vapostproc` between the input queue and `glupload`, and negotiates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn va_memory_from_the_media_player_gets_vapostproc_before_glupload() {
    common::require_elements(GL_ELEMENTS);
    stand_in::register();

    for input in ["video_in_0", "dsk_in_0"] {
        let suffix = if input == "dsk_in_0" { "dsk_0" } else { "0" };
        let queue = format!("{}:queue_{}", MIXER, suffix);
        let glupload = format!("{}:glupload_{}", MIXER, suffix);

        let (manager, _registry) = build_manager(&player_into_mixer("va_into_vm", input));
        let pipeline = manager.pipeline();
        let outcome = push_from_player(pipeline, VA_CAPS);

        let adapter = feeder_of(pipeline, &glupload).unwrap_or_else(|| {
            panic!("{}: {} lost its feeder", input, glupload);
        });
        assert_eq!(
            factory_name(&adapter),
            VA_POSTPROC_FACTORY,
            "{}: VA memory reached {} without a {} in front of it",
            input,
            glupload,
            VA_POSTPROC_FACTORY
        );
        assert_eq!(
            feeder_of(pipeline, &adapter.name()).map(|e| e.name().to_string()),
            Some(queue.clone()),
            "{}: the inserted {} is not fed by the input queue",
            input,
            VA_POSTPROC_FACTORY
        );
        assert!(
            outcome.errors.is_empty(),
            "{}: the flow failed: {:?}",
            input,
            outcome.errors
        );
        assert!(
            outcome
                .negotiated
                .as_ref()
                .and_then(|c| c.features(0))
                .is_some_and(|f| f.contains("memory:VAMemory")),
            "{}: the Media Player's output did not negotiate VA memory into the mixer \
             (got {:?})",
            input,
            outcome.negotiated
        );
    }
}
