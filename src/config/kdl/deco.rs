// decoration { rounding, dim-inactive, shadow {} } - the render candy

use super::{children, Cx};
use crate::config::*;

pub(super) fn parse(node: &KdlNode, cfg: &mut Config, cx: &mut Cx) {
    let d = &mut cfg.decoration;
    for c in children(node) {
        match c.name().value() {
            "rounding" => {
                if let Some(v) = cx.int(c) {
                    match int_in(v, "rounding", 0, 200) {
                        Ok(v) => d.rounding = v as i32,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "rounding-power" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "rounding-power", 1.0, 8.0) {
                        Ok(v) => d.rounding_power = v,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "dim-inactive" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "dim-inactive", 0.0, 1.0) {
                        Ok(v) => d.dim_inactive = v,
                        Err(e) => cx.leaf(c, e),
                    }
                }
            }
            "shadow" => {
                let mut s = ShadowCfg::default();
                if shadow(c, &mut s, cx) {
                    d.shadow = Some(s);
                }
            }
            // parses when the blur pass exists
            "blur" => cx.at(c, "blur: not implemented yet"),
            other => cx.at(c, &format!("unknown decoration key \"{other}\"")),
        }
    }
}

fn shadow(node: &KdlNode, out: &mut ShadowCfg, cx: &mut Cx) -> bool {
    let mut ok = true;
    for c in children(node) {
        match c.name().value() {
            "size" => {
                if let Some(v) = cx.int(c) {
                    match int_in(v, "size", 1, 200) {
                        Ok(v) => out.size = v as i32,
                        Err(e) => {
                            cx.leaf(c, e);
                            ok = false;
                        }
                    }
                }
            }
            "color" => {
                if let Some(s) = cx.str_(c) {
                    match color(&s) {
                        Ok(v) => out.color = v,
                        Err(e) => {
                            cx.leaf(c, e);
                            ok = false;
                        }
                    }
                }
            }
            "offset" => {
                let mut it = c.entries().iter().filter_map(|e| e.value().as_integer());
                match (it.next(), it.next()) {
                    (Some(x), Some(y)) if x.abs() <= 500 && y.abs() <= 500 => {
                        out.offset = (x as i32, y as i32);
                    }
                    _ => {
                        cx.at(c, "offset is two integers within -500..500");
                        ok = false;
                    }
                }
            }
            "power" => {
                if let Some(v) = cx.float(c) {
                    match f64_in(v, "power", 0.5, 8.0) {
                        Ok(v) => out.power = v,
                        Err(e) => {
                            cx.leaf(c, e);
                            ok = false;
                        }
                    }
                }
            }
            other => {
                cx.at(c, &format!("unknown shadow key \"{other}\""));
                ok = false;
            }
        }
    }
    ok
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
        crate::config::kdl::parse_bare(src).err().unwrap_or_default()
    }

    #[test]
    fn decoration_parses() {
        let c = parse_ok(
            "decoration {\n rounding 10\n rounding-power 2.5\n dim-inactive 0.15\n shadow { size 24; color \"#00000099\"; offset 0 4; power 3.0 }\n }",
        );
        assert_eq!(c.decoration.rounding, 10);
        assert_eq!(c.decoration.rounding_power, 2.5);
        assert!((c.decoration.dim_inactive - 0.15).abs() < 1e-9);
        let s = c.decoration.shadow.as_ref().unwrap();
        assert_eq!(s.size, 24);
        assert_eq!(s.offset, (0, 4));
        assert_eq!(s.power, 3.0);
        assert!((s.color[3] - 0.6).abs() < 0.01);
    }

    #[test]
    fn decoration_rejects_bad_input() {
        for (src, needle) in [
            ("decoration { rounding 999 }", "rounding"),
            ("decoration { dim-inactive 2 }", "dim-inactive"),
            ("decoration { shadow { offset 0 } }", "offset"),
            ("decoration { blur { passes 3 } }", "blur"),
            ("decoration { bogus 1 }", "unknown"),
        ] {
            let errs = parse_errs(src);
            assert!(errs.iter().any(|e| e.contains(needle)), "{src}: {errs:?}");
        }
    }

    #[test]
    fn decoration_rule_overrides() {
        let c = parse_ok(
            "window-rule {\n match app-id=\"term\"\n rounding 0\n shadow #false\n dim #false\n }",
        );
        let fx = rule_effects(&c, "term", "", false, false);
        assert_eq!(fx.rounding, Some(0));
        assert_eq!(fx.shadow, Some(false));
        assert_eq!(fx.dim, Some(false));
    }
}
