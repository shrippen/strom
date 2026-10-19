//! A transition held at a position (a T-bar): the pad values a
//! transition shows at `position` (0 = old source, 1 = new one), set
//! statically instead of animated. Pure, no GStreamer.

use super::TransitionType;

/// What the outgoing and incoming pads show at one position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TbarFrame {
    pub from_alpha: f64,
    pub to_alpha: f64,
    /// Offset of the outgoing pad from its resting place, in pixels.
    pub from_offset: (i32, i32),
    /// Offset of the incoming pad from its resting place, in pixels.
    pub to_offset: (i32, i32),
}

/// Cosine ease-in-out, as the animated dip to black uses.
fn ease_in_out(t: f64) -> f64 {
    (1.0 - (t * std::f64::consts::PI).cos()) / 2.0
}

/// The frame of `transition` at `position` on a `width`×`height` canvas.
/// Shader transitions (wipes, master FX) and a cut show as a fade: a wipe
/// needs the FX engine's uniforms, a cut has no in-between.
pub fn tbar_frame(transition: TransitionType, position: f64, width: i32, height: i32) -> TbarFrame {
    let p = position.clamp(0.0, 1.0);
    let still = TbarFrame {
        from_alpha: 1.0,
        to_alpha: p,
        from_offset: (0, 0),
        to_offset: (0, 0),
    };
    // Direction the incoming picture moves in, as the animated ones.
    let (dx, dy, push) = match transition {
        TransitionType::SlideLeft => (-1, 0, false),
        TransitionType::SlideRight => (1, 0, false),
        TransitionType::SlideUp => (0, -1, false),
        TransitionType::SlideDown => (0, 1, false),
        TransitionType::PushLeft => (-1, 0, true),
        TransitionType::PushRight => (1, 0, true),
        TransitionType::PushUp => (0, -1, true),
        TransitionType::PushDown => (0, 1, true),
        TransitionType::DipToBlack => {
            return if p < 0.5 {
                TbarFrame {
                    from_alpha: 1.0 - ease_in_out(2.0 * p),
                    to_alpha: 0.0,
                    ..still
                }
            } else {
                TbarFrame {
                    from_alpha: 0.0,
                    to_alpha: ease_in_out(2.0 * p - 1.0),
                    ..still
                }
            };
        }
        TransitionType::Cut
        | TransitionType::Fade
        | TransitionType::Wipe(_)
        | TransitionType::MasterFx(_) => return still,
    };
    let (w, h) = (f64::from(width), f64::from(height));
    let to_offset = (
        (-f64::from(dx) * w * (1.0 - p)).round() as i32,
        (-f64::from(dy) * h * (1.0 - p)).round() as i32,
    );
    let from_offset = if push {
        (
            (f64::from(dx) * w * p).round() as i32,
            (f64::from(dy) * h * p).round() as i32,
        )
    } else {
        (0, 0)
    };
    TbarFrame {
        from_alpha: 1.0,
        to_alpha: 1.0,
        from_offset,
        to_offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fade_shows_the_new_source_at_the_bar_position() {
        let f = tbar_frame(TransitionType::Fade, 0.25, 1920, 1080);
        assert_eq!((f.from_alpha, f.to_alpha), (1.0, 0.25));
        assert_eq!((f.from_offset, f.to_offset), ((0, 0), (0, 0)));
    }

    #[test]
    fn a_slide_brings_the_new_source_in_from_its_side() {
        // Slide left: the new picture comes from the right.
        let f = tbar_frame(TransitionType::SlideLeft, 0.25, 1920, 1080);
        assert_eq!(f.to_offset, (1440, 0));
        assert_eq!(f.from_offset, (0, 0), "the old one stays");
        assert_eq!(f.to_alpha, 1.0);
        let f = tbar_frame(TransitionType::SlideDown, 0.5, 1920, 1080);
        assert_eq!(f.to_offset, (0, -540));
        let end = tbar_frame(TransitionType::SlideLeft, 1.0, 1920, 1080);
        assert_eq!(end.to_offset, (0, 0));
    }

    #[test]
    fn a_push_moves_both_sources() {
        let f = tbar_frame(TransitionType::PushRight, 0.5, 1920, 1080);
        assert_eq!(f.from_offset, (960, 0));
        assert_eq!(f.to_offset, (-960, 0));
    }

    #[test]
    fn a_dip_goes_through_black_at_half() {
        let half = tbar_frame(TransitionType::DipToBlack, 0.5, 1920, 1080);
        assert_eq!((half.from_alpha, half.to_alpha), (0.0, 0.0));
        let quarter = tbar_frame(TransitionType::DipToBlack, 0.25, 1920, 1080);
        assert!((quarter.from_alpha - 0.5).abs() < 1e-9 && quarter.to_alpha == 0.0);
        let end = tbar_frame(TransitionType::DipToBlack, 1.0, 1920, 1080);
        assert_eq!((end.from_alpha, end.to_alpha), (0.0, 1.0));
    }
}
