// animations { off, slowdown, curve defs, per-kind spring/ease/style }

use super::{children, Cx};
use crate::config::*;

// which styles a kind accepts
#[derive(Copy, Clone, PartialEq)]
enum Family {
    Win,        // popin, fade, slide
    Ws,         // slide, slidevert, fade, slidefade, slidefadevert
    MotionOnly, // no style at all
}

pub(super) fn parse(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let a = &mut cfg.animations;
    for c in children(node) {
        match c.name().value() {
            "off" => {
                if let Some(b) = cx.flag(c) {
                    a.off = b;
                }
            }
            "slowdown" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "slowdown", 0.1, 10.0) {
                        Ok(v) => a.slowdown = v,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "curve" => curve_def(c, a, cx),
            "spring" | "ease" => {
                if let Some(m) = motion(c, cx) {
                    a.default_motion = m;
                }
            }
            "window-open" => kind(c, cx, Family::Win, |a| &mut a.window_open, a),
            "window-close" => kind(c, cx, Family::Win, |a| &mut a.window_close, a),
            "window-move" => kind(c, cx, Family::MotionOnly, |a| &mut a.window_move, a),
            "window-resize" => kind(c, cx, Family::MotionOnly, |a| &mut a.window_resize, a),
            "workspace-switch" => kind(c, cx, Family::Ws, |a| &mut a.workspace_switch, a),
            "view-movement" => kind(c, cx, Family::MotionOnly, |a| &mut a.view_movement, a),
            "layer-open" => kind(c, cx, Family::Win, |a| &mut a.layer_open, a),
            "layer-close" => kind(c, cx, Family::Win, |a| &mut a.layer_close, a),
            "border-color" => kind(c, cx, Family::MotionOnly, |a| &mut a.border_color, a),
            other => cx.at(c, &format!("unknown animations key \"{other}\"")),
        }
    }
    // named curves must exist by the end of the section
    let known: Vec<String> = a.curves.iter().map(|(n, _)| n.clone()).collect();
    let mut check = |m: &Option<Motion>, cx: &mut Cx| {
        if let Some(Motion::Ease { curve: CurveRef::Named(n), .. }) = m {
            if !known.contains(n) {
                cx.at(node, &format!("unknown curve \"{n}\""));
            }
        }
    };
    if let Motion::Ease { curve: CurveRef::Named(n), .. } = &a.default_motion {
        if !known.contains(n) {
            cx.at(node, &format!("unknown curve \"{n}\""));
        }
    }
    for kc in [
        &a.window_open,
        &a.window_close,
        &a.window_move,
        &a.window_resize,
        &a.workspace_switch,
        &a.view_movement,
        &a.layer_open,
        &a.layer_close,
        &a.border_color,
    ] {
        check(&kc.motion, cx);
    }
}

fn prop_f64(node: &KdlNode, name: &str) -> Option<f64> {
    node.get(name)
        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
}

fn prop_str<'a>(node: &'a KdlNode, name: &str) -> Option<&'a str> {
    node.get(name).and_then(|v| v.as_string())
}

/// `spring damping-ratio=.. stiffness=.. epsilon=..` or
/// `ease duration-ms=.. curve=".."` - properties, not arguments
fn motion(node: &KdlNode, cx: &mut Cx) -> Option<Motion> {
    match node.name().value() {
        "spring" => {
            let (Some(d), Some(s), Some(e)) = (
                prop_f64(node, "damping-ratio"),
                prop_f64(node, "stiffness"),
                prop_f64(node, "epsilon"),
            ) else {
                cx.at(node, "spring needs damping-ratio= stiffness= epsilon=");
                return None;
            };
            let d = match f64_in(d, "damping-ratio", 0.1, 10.0) {
                Ok(v) => v,
                Err(e) => return err(cx, node, e),
            };
            let s = match f64_in(s, "stiffness", 1.0, 100_000.0) {
                Ok(v) => v,
                Err(e) => return err(cx, node, e),
            };
            let e = match f64_in(e, "epsilon", 0.00001, 0.1) {
                Ok(v) => v,
                Err(e) => return err(cx, node, e),
            };
            Some(Motion::Spring { damping: d, stiffness: s, epsilon: e })
        }
        "ease" => {
            let Some(ms) = prop_f64(node, "duration-ms") else {
                cx.at(node, "ease needs duration-ms=");
                return None;
            };
            let ms = match int_in(ms as i64, "duration-ms", 0, 10_000) {
                Ok(v) => v as u32,
                Err(e) => return err(cx, node, e),
            };
            let curve = match prop_str(node, "curve") {
                None => CurveRef::Cubic,
                Some("linear") => CurveRef::Linear,
                Some("ease-out-quad") => CurveRef::Quad,
                Some("ease-out-cubic") => CurveRef::Cubic,
                Some("ease-out-expo") => CurveRef::Expo,
                Some(name) => CurveRef::Named(name.to_string()),
            };
            Some(Motion::Ease { ms, curve })
        }
        _ => unreachable!(),
    }
}

fn err(cx: &mut Cx, node: &KdlNode, e: String) -> Option<Motion> {
    cx.leaf(node, e);
    None
}

/// `curve "name" x1 y1 x2 y2` - a named cubic bezier
fn curve_def(node: &KdlNode, a: &mut AnimsCfg, cx: &mut Cx) {
    let args: Vec<_> = node.entries().iter().filter(|e| e.name().is_none()).collect();
    let name = args.first().and_then(|e| e.value().as_string());
    let nums: Vec<f64> = args
        .iter()
        .skip(1)
        .filter_map(|e| e.value().as_float().or_else(|| e.value().as_integer().map(|i| i as f64)))
        .collect();
    let (Some(name), [x1, y1, x2, y2]) = (name, nums.as_slice()) else {
        cx.at(node, "curve is: curve \"name\" x1 y1 x2 y2");
        return;
    };
    if a.curves.iter().any(|(n, _)| n == name) {
        cx.at(node, &format!("duplicate curve \"{name}\""));
        return;
    }
    a.curves
        .push((name.to_string(), crate::anim::CubicBezier::new(*x1, *y1, *x2, *y2)));
}

fn kind(
    node: &KdlNode,
    cx: &mut Cx,
    family: Family,
    get: impl Fn(&mut AnimsCfg) -> &mut KindCfg,
    a: &mut AnimsCfg,
) {
    let mut out = get(a).clone();
    let mut has_motion = false;
    for c in children(node) {
        match c.name().value() {
            "off" => {
                if let Some(b) = cx.flag(c) {
                    out.off = b;
                }
            }
            "spring" | "ease" => {
                if has_motion {
                    cx.at(c, "spring and ease are mutually exclusive");
                    continue;
                }
                if let Some(m) = motion(c, cx) {
                    out.motion = Some(m);
                    has_motion = true;
                }
            }
            "style" => {
                if family == Family::MotionOnly {
                    cx.at(c, &format!("{} takes no style", node.name().value()));
                    continue;
                }
                if let Some(s) = style(c, cx, family) {
                    out.style = s;
                }
            }
            other => cx.at(c, &format!("unknown animation key \"{other}\"")),
        }
    }
    *get(a) = out;
}

fn style(node: &KdlNode, cx: &mut Cx, family: Family) -> Option<Style> {
    let name = node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())?;
    let perc = || -> f64 {
        prop_f64(node, "perc")
            .map(|p| (p / 100.0).clamp(0.0, 1.0))
            .unwrap_or(0.8)
    };
    let dir = match prop_str(node, "dir") {
        None => None,
        Some("top") => Some(Dir::Up),
        Some("bottom") => Some(Dir::Down),
        Some("left") => Some(Dir::Left),
        Some("right") => Some(Dir::Right),
        Some(other) => {
            cx.at(node, &format!("dir \"{other}\" is top, bottom, left or right"));
            return None;
        }
    };
    let s = match (family, name) {
        (Family::Win, "popin") => Style::Popin { perc: perc() },
        (Family::Win, "fade") => Style::Fade,
        (Family::Win, "slide") => Style::Slide { dir },
        (Family::Ws, "slide") => Style::Slide { dir: None },
        (Family::Ws, "slidevert") => Style::SlideVert,
        (Family::Ws, "fade") => Style::Fade,
        (Family::Ws, "slidefade") => Style::SlideFade { perc: perc() },
        (Family::Ws, "slidefadevert") => Style::SlideFadeVert { perc: perc() },
        _ => {
            cx.at(node, &format!("style \"{name}\" does not fit this animation"));
            return None;
        }
    };
    Some(s)
}

#[cfg(test)]
mod tests {
    use crate::config::*;

    fn parse_ok(src: &str) -> Config {
        match crate::config::kdl::parse_bare(src) {
            Ok(c) => c,
            Err(e) => panic!("expected clean parse: {e:?}"),
        }
    }

    fn parse_errs(src: &str) -> Vec<String> {
        match crate::config::kdl::parse_bare(src) {
            Ok(_) => vec![],
            Err(e) => e,
        }
    }

    #[test]
    fn anims_defaults_when_absent() {
        let c = parse_ok("layout { mode \"dwindle\" }");
        assert!(!c.animations.off);
        assert_eq!(c.animations.slowdown, 1.0);
        assert!(matches!(c.animations.window_open.style, Style::Popin { .. }));
    }

    #[test]
    fn anims_both_flavors_parse() {
        let c = parse_ok(
            "animations {\n\
             slowdown 2.0\n\
             curve \"shoot\" 0.05 0.9 0.1 1.05\n\
             spring damping-ratio=1.0 stiffness=600 epsilon=0.001\n\
             window-open { ease duration-ms=200 curve=\"shoot\"; style \"slide\" dir=\"top\" }\n\
             workspace-switch { style \"slidefadevert\" perc=30 }\n\
             window-move { off }\n\
             }",
        );
        assert_eq!(c.animations.slowdown, 2.0);
        assert_eq!(c.animations.curves.len(), 1);
        assert!(
            matches!(c.animations.default_motion, Motion::Spring { stiffness, .. } if stiffness == 600.0)
        );
        assert!(matches!(
            c.animations.window_open.motion,
            Some(Motion::Ease { ms: 200, curve: CurveRef::Named(ref n) }) if n == "shoot"
        ));
        assert!(matches!(c.animations.window_open.style, Style::Slide { dir: Some(_) }));
        assert!(matches!(
            c.animations.workspace_switch.style,
            Style::SlideFadeVert { perc } if (perc - 0.3).abs() < 1e-9
        ));
        assert!(c.animations.window_move.off);
        // inheritance: window-move has no own motion, and off wins
        assert!(c.animations.window_move.motion.is_none());
        assert!(c.animations.motion(AnimKind::WindowMove).is_none());
        assert!(c.animations.motion(AnimKind::ViewMovement).is_some());
    }

    #[test]
    fn anims_rejects_bad_input() {
        for (src, needle) in [
            (
                "animations { window-open { spring damping-ratio=1.0 stiffness=800 epsilon=0.001; ease duration-ms=100 curve=\"linear\" } }",
                "exclusive",
            ),
            ("animations { window-open { ease duration-ms=100 curve=\"nope\" } }", "curve"),
            ("animations { window-open { style \"slidefadevert\" } }", "style"),
            ("animations { window-move { style \"popin\" } }", "style"),
            ("animations { spring damping-ratio=99 stiffness=800 epsilon=0.001 }", "damping"),
            ("animations { bogus 1 }", "unknown"),
        ] {
            let errs = parse_errs(src);
            assert!(
                errs.iter().any(|e| e.to_lowercase().contains(needle)),
                "{src} should error mentioning {needle}, got {errs:?}"
            );
        }
    }
}
