use noprop::TestCaseContext;

pub const DEFAULT_CASES: usize = 1024;
const DEFAULT_SEED: u64 = 0x5445_4D4F_5445_0001;

pub fn seed(salt: u64) -> u64 {
    let base = match std::env::var("TEMOTE_PBT_SEED") {
        Ok(raw) => {
            if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
                u64::from_str_radix(hex, 16).expect("TEMOTE_PBT_SEED must be a u64")
            } else {
                raw.parse().expect("TEMOTE_PBT_SEED must be a u64")
            }
        }
        Err(_) => DEFAULT_SEED,
    };
    base ^ salt
}

pub fn run(
    salt: u64,
    cases: usize,
    property: impl Fn(&mut TestCaseContext) -> noprop::TestResult,
) -> noprop::TestResult {
    noprop::Runner::new(seed(salt)).run(cases, property)?;
    Ok(())
}

#[allow(dead_code)]
pub fn ascii_string(ctx: &mut TestCaseContext, max_len: usize) -> String {
    let len = noprop::sample_usize_in(ctx, 0..=max_len);
    (0..len)
        .map(|_| char::from(noprop::sample_u8(ctx) & 0x7f))
        .collect()
}

pub fn safe_component(ctx: &mut TestCaseContext) -> String {
    let len = noprop::sample_usize_in(ctx, 1..=12);
    (0..len)
        .map(|_| match noprop::sample_u8(ctx) % 36 {
            value @ 0..=9 => char::from(b'0' + value),
            value => char::from(b'a' + (value - 10)),
        })
        .collect()
}
