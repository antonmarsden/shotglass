//! Animated values for uniforms: constants or keyframes, evaluated at shot-local time.

use anyhow::{Result, anyhow, bail};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ease {
    Linear,
    Smooth,
    Smoother,
    In,
    Out,
    InOut,
    Step,
}

impl Ease {
    pub fn parse(s: &str) -> Result<Ease> {
        Ok(match s {
            "linear" => Ease::Linear,
            "smooth" => Ease::Smooth,
            "smoother" => Ease::Smoother,
            "in" => Ease::In,
            "out" => Ease::Out,
            "inout" | "in_out" | "inOut" => Ease::InOut,
            "step" => Ease::Step,
            _ => bail!("unknown ease \"{s}\" (use linear, smooth, smoother, in, out, inout, step)"),
        })
    }

    /// Matches `sg_ease` in generated GLSL.
    pub fn code(self) -> i32 {
        match self {
            Ease::Linear => 0,
            Ease::Smooth => 1,
            Ease::Smoother => 2,
            Ease::In => 3,
            Ease::Out => 4,
            Ease::InOut => 5,
            Ease::Step => 6,
        }
    }

    pub fn apply(self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Ease::Linear => x,
            Ease::Smooth => x * x * (3.0 - 2.0 * x),
            Ease::Smoother => x * x * x * (x * (6.0 * x - 15.0) + 10.0),
            Ease::In => x * x,
            Ease::Out => 1.0 - (1.0 - x) * (1.0 - x),
            Ease::InOut => {
                if x < 0.5 {
                    2.0 * x * x
                } else {
                    1.0 - 2.0 * (1.0 - x) * (1.0 - x)
                }
            }
            Ease::Step => {
                if x < 1.0 {
                    0.0
                } else {
                    1.0
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    pub t: f32,
    pub v: Vec<f32>,
    /// Ease used on the way *into* this key.
    pub ease: Ease,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Anim {
    Const(Vec<f32>),
    Keys(Vec<Key>),
}

impl Anim {
    /// Number of components (1 = float, 2..4 = vecN).
    pub fn dim(&self) -> usize {
        match self {
            Anim::Const(v) => v.len(),
            Anim::Keys(k) => k[0].v.len(),
        }
    }

    pub fn glsl_type(&self) -> &'static str {
        match self.dim() {
            1 => "float",
            2 => "vec2",
            3 => "vec3",
            _ => "vec4",
        }
    }

    pub fn eval(&self, t: f32) -> [f32; 4] {
        let mut out = [0.0; 4];
        match self {
            Anim::Const(v) => out[..v.len()].copy_from_slice(v),
            Anim::Keys(ks) => {
                let n = ks[0].v.len();
                if t <= ks[0].t {
                    out[..n].copy_from_slice(&ks[0].v);
                } else if let Some(i) = ks.iter().position(|k| t <= k.t) {
                    let (a, b) = (&ks[i - 1], &ks[i]);
                    let span = b.t - a.t;
                    let k = if span > 0.0 {
                        b.ease.apply((t - a.t) / span)
                    } else {
                        1.0
                    };
                    for (o, (x, y)) in out.iter_mut().zip(a.v.iter().zip(&b.v)) {
                        *o = x + (y - x) * k;
                    }
                } else {
                    out[..n].copy_from_slice(&ks[ks.len() - 1].v);
                }
            }
        }
        out
    }

    /// A GLSL function `T name(float t)` that evaluates this value, for sound shaders
    /// (which need the value at every sample, so it can't be a uniform).
    pub fn glsl_fn(&self, name: &str) -> String {
        let ty = self.glsl_type();
        let lit = |v: &[f32]| match v.len() {
            1 => glsl_f(v[0]),
            n => format!(
                "vec{n}({})",
                v.iter().map(|x| glsl_f(*x)).collect::<Vec<_>>().join(", ")
            ),
        };
        let mut s = format!("{ty} {name}(float t) {{\n");
        match self {
            Anim::Const(v) => s += &format!("  return {};\n", lit(v)),
            Anim::Keys(ks) => {
                s += &format!(
                    "  if (t <= {}) return {};\n",
                    glsl_f(ks[0].t),
                    lit(&ks[0].v)
                );
                for w in ks.windows(2) {
                    let (a, b) = (&w[0], &w[1]);
                    let span = (b.t - a.t).max(1e-6);
                    s += &format!(
                        "  if (t <= {}) return mix({}, {}, sg_ease({}, (t - {}) / {}));\n",
                        glsl_f(b.t),
                        lit(&a.v),
                        lit(&b.v),
                        b.ease.code(),
                        glsl_f(a.t),
                        glsl_f(span)
                    );
                }
                s += &format!("  return {};\n", lit(&ks[ks.len() - 1].v));
            }
        }
        s + "}\n"
    }

    /// Parse a TOML value: `0.5`, `[0.1, 0.2]`, or `{ keys = [[0, 0.7], [8, 0.2, "out"]], ease = "smooth" }`.
    pub fn from_toml(v: &toml::Value) -> Result<Anim> {
        match v {
            toml::Value::Table(t) => {
                let default_ease = match t.get("ease") {
                    Some(e) => {
                        Ease::parse(e.as_str().ok_or_else(|| anyhow!("ease must be a string"))?)?
                    }
                    None => Ease::Smooth,
                };
                for k in t.keys() {
                    if k != "keys" && k != "ease" {
                        bail!("unknown field \"{k}\" in keyframed uniform (expected keys, ease)");
                    }
                }
                let keys = t.get("keys").and_then(|k| k.as_array()).ok_or_else(|| {
                    anyhow!("keyframed uniform needs keys = [[time, value], ...]")
                })?;
                let mut out = Vec::new();
                for key in keys {
                    let a = key.as_array().ok_or_else(|| {
                        anyhow!("each key must be [time, value] or [time, value, \"ease\"]")
                    })?;
                    if a.len() < 2 || a.len() > 3 {
                        bail!("each key must be [time, value] or [time, value, \"ease\"]");
                    }
                    let t = num(&a[0])?;
                    let v = vector(&a[1])?;
                    let ease = match a.get(2) {
                        Some(e) => Ease::parse(
                            e.as_str()
                                .ok_or_else(|| anyhow!("key ease must be a string"))?,
                        )?,
                        None => default_ease,
                    };
                    out.push(Key { t, v, ease });
                }
                if out.is_empty() {
                    bail!("keys must not be empty");
                }
                let n = out[0].v.len();
                if out.iter().any(|k| k.v.len() != n) {
                    bail!("all keyframe values must have the same number of components");
                }
                out.sort_by(|a, b| a.t.total_cmp(&b.t));
                Ok(Anim::Keys(out))
            }
            other => Ok(Anim::Const(vector(other)?)),
        }
    }
}

/// GLSL source for `sg_ease`, matching [`Ease::apply`].
pub const GLSL_EASE: &str = "float sg_ease(int e, float x) {
  x = clamp(x, 0.0, 1.0);
  if (e == 0) return x;
  if (e == 1) return x * x * (3.0 - 2.0 * x);
  if (e == 2) return x * x * x * (x * (6.0 * x - 15.0) + 10.0);
  if (e == 3) return x * x;
  if (e == 4) return 1.0 - (1.0 - x) * (1.0 - x);
  if (e == 5) return x < 0.5 ? 2.0 * x * x : 1.0 - 2.0 * (1.0 - x) * (1.0 - x);
  return x < 1.0 ? 0.0 : 1.0;
}
";

fn glsl_f(x: f32) -> String {
    crate::movie::glsl_float(x)
}

fn num(v: &toml::Value) -> Result<f32> {
    match v {
        toml::Value::Float(f) => Ok(*f as f32),
        toml::Value::Integer(i) => Ok(*i as f32),
        toml::Value::Boolean(b) => Ok(if *b { 1.0 } else { 0.0 }),
        _ => bail!("expected a number, got {v}"),
    }
}

/// A number or an array of 2..4 numbers.
pub fn vector(v: &toml::Value) -> Result<Vec<f32>> {
    match v {
        toml::Value::Array(a) => {
            if !(2..=4).contains(&a.len()) {
                bail!("vectors need 2 to 4 components, got {}", a.len());
            }
            a.iter().map(num).collect()
        }
        other => Ok(vec![num(other)?]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Anim {
        let v: toml::Table = toml::from_str(&format!("x = {s}")).unwrap();
        Anim::from_toml(&v["x"]).unwrap()
    }

    #[test]
    fn constants() {
        assert_eq!(parse("2").eval(5.0)[0], 2.0);
        let v = parse("[1, 2, 3]");
        assert_eq!(v.dim(), 3);
        assert_eq!(v.glsl_type(), "vec3");
        assert_eq!(&v.eval(0.0)[..3], &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn keyframes_hold_and_interpolate() {
        let a = parse(r#"{ keys = [[0, 0], [2, 10, "linear"]] }"#);
        assert_eq!(a.eval(-1.0)[0], 0.0);
        assert_eq!(a.eval(1.0)[0], 5.0);
        assert_eq!(a.eval(9.0)[0], 10.0);
        let s = parse(r#"{ keys = [[0, 0], [2, 10]], ease = "smooth" }"#);
        assert!((s.eval(1.0)[0] - 5.0).abs() < 1e-5);
        assert!(s.eval(0.5)[0] < 2.5);
    }

    #[test]
    fn glsl_functions() {
        let a = parse(r#"{ keys = [[0, 0], [2, 10, "linear"]] }"#);
        let g = a.glsl_fn("f");
        assert!(g.starts_with("float f(float t) {"), "{g}");
        assert!(g.contains("sg_ease(0, (t - 0.0) / 2.0)"), "{g}");
        assert!(
            parse("[1, 2]")
                .glsl_fn("v")
                .contains("return vec2(1.0, 2.0);")
        );
    }

    #[test]
    fn rejects_mismatched_dims() {
        let v: toml::Table = toml::from_str("x = { keys = [[0, 1], [1, [1, 2]]] }").unwrap();
        assert!(Anim::from_toml(&v["x"]).is_err());
    }
}
