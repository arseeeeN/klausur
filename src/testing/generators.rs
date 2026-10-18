//! Deterministic randomness and value generators (`gen::…`).

use rhai::{Array, Dynamic, EvalAltResult, FnPtr, ImmutableString, Map, Module, NativeCallContext};
use std::cell::RefCell;
use std::ops::{Range, RangeInclusive};
use std::rc::Rc;

/// SplitMix64: small, fast and stable across versions, so seeds keep reproducing runs.
#[derive(Clone)]
pub struct Rng(Rc<RefCell<u64>>);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(Rc::new(RefCell::new(seed)))
    }

    pub fn next_u64(&self) -> u64 {
        let mut s = self.0.borrow_mut();
        *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `lo..=hi`.
    pub fn int(&self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = (hi as i128 - lo as i128 + 1) as u128;
        (lo as i128 + (self.next_u64() as u128 % span) as i128) as i64
    }

    pub fn float(&self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn index(&self, len: usize) -> usize {
        self.int(0, len as i64 - 1) as usize
    }
}

/// Derive a seed from a base seed and some identifying parts.
pub fn derive_seed(base: u64, parts: &[&[u8]]) -> u64 {
    // FNV-1a over the parts, mixed with the base seed.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ base;
    for part in parts {
        for b in *part {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    Rng::new(h).next_u64()
}

const ALNUM: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

#[derive(Clone)]
pub enum Gen {
    Int(i64, i64),
    Float(f64, f64),
    Bool,
    Str {
        min: usize,
        max: usize,
        chars: Rc<Vec<char>>,
    },
    Email,
    Uuid,
    OneOf(Array),
    Array(Box<Gen>, usize, usize),
    Map(Vec<(String, Dynamic)>),
    Custom(FnPtr),
}

pub type CallFn<'a> = dyn FnMut(&FnPtr, Rng) -> Result<Dynamic, Box<EvalAltResult>> + 'a;

impl Gen {
    pub fn sample(&self, rng: &Rng, call: &mut CallFn) -> Result<Dynamic, Box<EvalAltResult>> {
        Ok(match self {
            Gen::Int(lo, hi) => rng.int(*lo, *hi).into(),
            Gen::Float(lo, hi) => (lo + rng.float() * (hi - lo)).into(),
            Gen::Bool => (rng.next_u64() & 1 == 1).into(),
            Gen::Str { min, max, chars } => {
                let len = rng.int(*min as i64, *max as i64) as usize;
                (0..len)
                    .map(|_| chars[rng.index(chars.len())])
                    .collect::<String>()
                    .into()
            }
            Gen::Email => {
                let user = random_string(rng, 1, 12);
                let domain = random_string(rng, 1, 10).to_lowercase();
                format!("{user}@{domain}.test").into()
            }
            Gen::Uuid => uuid(rng).into(),
            Gen::OneOf(items) if items.is_empty() => Dynamic::UNIT,
            Gen::OneOf(items) => sample_value(&items[rng.index(items.len())], rng, call)?,
            Gen::Array(g, min, max) => {
                let len = rng.int(*min as i64, *max as i64);
                let mut out = Array::new();
                for _ in 0..len {
                    out.push(g.sample(rng, call)?);
                }
                out.into()
            }
            Gen::Map(fields) => {
                let mut out = Map::new();
                for (k, v) in fields {
                    out.insert(k.as_str().into(), sample_value(v, rng, call)?);
                }
                out.into()
            }
            Gen::Custom(f) => call(f, rng.clone())?,
        })
    }
}

/// Sample a generator, or return a plain value unchanged.
pub fn sample_value(
    v: &Dynamic,
    rng: &Rng,
    call: &mut CallFn,
) -> Result<Dynamic, Box<EvalAltResult>> {
    match v.read_lock::<Gen>() {
        Some(g) => g.clone().sample(rng, call),
        None => Ok(v.clone()),
    }
}

fn random_string(rng: &Rng, min: usize, max: usize) -> String {
    let chars: Vec<char> = ALNUM.chars().collect();
    let len = rng.int(min as i64, max as i64) as usize;
    (0..len).map(|_| chars[rng.index(chars.len())]).collect()
}

pub fn uuid(rng: &Rng) -> String {
    let a = rng.next_u64();
    let b = rng.next_u64();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_be_bytes());
    bytes[8..].copy_from_slice(&b.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn len_range(r: Range<i64>) -> (usize, usize) {
    (
        r.start.max(0) as usize,
        (r.end - 1).max(r.start).max(0) as usize,
    )
}

fn len_range_incl(r: RangeInclusive<i64>) -> (usize, usize) {
    (
        *r.start().max(&0) as usize,
        *r.end().max(r.start()).max(&0) as usize,
    )
}

fn string_gen(min: usize, max: usize, chars: &str) -> Gen {
    let chars: Vec<char> = if chars.is_empty() {
        ALNUM.chars().collect()
    } else {
        chars.chars().collect()
    };
    Gen::Str {
        min,
        max,
        chars: Rc::new(chars),
    }
}

/// The `gen` module.
pub fn module(rng_of: impl Fn() -> Rng + 'static) -> Module {
    let mut m = Module::new();
    m.set_native_fn("int", |r: Range<i64>| Ok(Gen::Int(r.start, r.end - 1)));
    m.set_native_fn("int", |r: RangeInclusive<i64>| {
        Ok(Gen::Int(*r.start(), *r.end()))
    });
    m.set_native_fn("int", || Ok(Gen::Int(-1_000_000, 1_000_000)));
    m.set_native_fn("float", |lo: f64, hi: f64| Ok(Gen::Float(lo, hi)));
    m.set_native_fn("float", || Ok(Gen::Float(0.0, 1.0)));
    m.set_native_fn("bool", || Ok(Gen::Bool));
    m.set_native_fn("string", |r: Range<i64>| {
        let (a, b) = len_range(r);
        Ok(string_gen(a, b, ""))
    });
    m.set_native_fn("string", |r: RangeInclusive<i64>| {
        let (a, b) = len_range_incl(r);
        Ok(string_gen(a, b, ""))
    });
    m.set_native_fn("string", |r: Range<i64>, chars: ImmutableString| {
        let (a, b) = len_range(r);
        Ok(string_gen(a, b, &chars))
    });
    m.set_native_fn(
        "string",
        |r: RangeInclusive<i64>, chars: ImmutableString| {
            let (a, b) = len_range_incl(r);
            Ok(string_gen(a, b, &chars))
        },
    );
    m.set_native_fn("string", || Ok(string_gen(0, 32, "")));
    m.set_native_fn("email", || Ok(Gen::Email));
    m.set_native_fn("uuid", || Ok(Gen::Uuid));
    m.set_native_fn("one_of", |items: Array| Ok(Gen::OneOf(items)));
    m.set_native_fn("array", |g: Gen, r: Range<i64>| {
        let (a, b) = len_range(r);
        Ok(Gen::Array(Box::new(g), a, b))
    });
    m.set_native_fn("array", |g: Gen, r: RangeInclusive<i64>| {
        let (a, b) = len_range_incl(r);
        Ok(Gen::Array(Box::new(g), a, b))
    });
    m.set_native_fn("map", |fields: Map| {
        Ok(Gen::Map(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        ))
    });
    m.set_native_fn("custom", |f: FnPtr| Ok(Gen::Custom(f)));
    m.set_native_fn("rng", move || Ok(rng_of()));
    m
}

pub fn register(engine: &mut rhai::Engine, rng_of: Rc<dyn Fn() -> Rng>) {
    engine.register_type_with_name::<Gen>("Generator");
    engine.register_type_with_name::<Rng>("Rng");
    let current = rng_of.clone();
    engine.register_fn("sample", move |ctx: NativeCallContext, g: &mut Gen| {
        g.sample(&current(), &mut |f, r| f.call_within_context(&ctx, (r,)))
    });
    engine.register_fn("int", |r: &mut Rng, lo: i64, hi: i64| r.int(lo, hi));
    engine.register_fn("float", |r: &mut Rng| r.float());
    engine.register_fn("bool", |r: &mut Rng| r.next_u64() & 1 == 1);
    engine.register_fn("pick", |r: &mut Rng, items: Array| {
        if items.is_empty() {
            Dynamic::UNIT
        } else {
            items[r.index(items.len())].clone()
        }
    });
    engine.register_fn("string", |r: &mut Rng, len: i64| {
        random_string(r, len.max(0) as usize, len.max(0) as usize)
    });
    engine.register_fn("sample", |ctx: NativeCallContext, r: &mut Rng, g: Gen| {
        g.sample(r, &mut |f, rng| f.call_within_context(&ctx, (rng,)))
    });
    engine.register_static_module("gen", module(move || rng_of()).into());
    engine.register_fn("to_string", |g: &mut Gen| describe(g));
}

fn describe(g: &Gen) -> String {
    match g {
        Gen::Int(a, b) => format!("gen::int({a}..={b})"),
        Gen::Float(a, b) => format!("gen::float({a}, {b})"),
        Gen::Bool => "gen::bool()".into(),
        Gen::Str { min, max, .. } => format!("gen::string({min}..={max})"),
        Gen::Email => "gen::email()".into(),
        Gen::Uuid => "gen::uuid()".into(),
        Gen::OneOf(items) => format!("gen::one_of({} items)", items.len()),
        Gen::Array(_, a, b) => format!("gen::array(…, {a}..={b})"),
        Gen::Map(_) => "gen::map(…)".into(),
        Gen::Custom(_) => "gen::custom(…)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic() {
        let a = Rng::new(42);
        let b = Rng::new(42);
        for _ in 0..10 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        for _ in 0..1000 {
            let v = a.int(-3, 3);
            assert!((-3..=3).contains(&v));
        }
    }

    #[test]
    fn uuids_look_like_v4() {
        let u = uuid(&Rng::new(1));
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
    }
}
