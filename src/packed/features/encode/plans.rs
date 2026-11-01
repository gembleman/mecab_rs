use super::TextOp;

const CODE_NONE: u8 = 0;
const CODE_KEY: u8 = 1;
const CODE_STAR: u8 = 2;
const CODE_EMPTY: u8 = 3;
const CODE_DERIVE: u8 = 4;
const CODE_NEW: u8 = 5;
const CODE_COPY: u8 = 6;

pub(super) fn plan_code(op: Option<TextOp>) -> u8 {
    match op {
        None => CODE_NONE,
        Some(TextOp::Key) => CODE_KEY,
        Some(TextOp::Star) => CODE_STAR,
        Some(TextOp::Empty) => CODE_EMPTY,
        Some(TextOp::Derive) => CODE_DERIVE,
        Some(TextOp::New) => CODE_NEW,
        Some(TextOp::Copy(slot)) => CODE_COPY + slot,
    }
}

pub(super) fn plan_op(code: u8) -> Option<TextOp> {
    match code {
        CODE_NONE => None,
        CODE_KEY => Some(TextOp::Key),
        CODE_STAR => Some(TextOp::Star),
        CODE_EMPTY => Some(TextOp::Empty),
        CODE_DERIVE => Some(TextOp::Derive),
        CODE_NEW => Some(TextOp::New),
        slot => Some(TextOp::Copy(slot - CODE_COPY)),
    }
}

pub(super) fn is_new_code(code: u8) -> bool {
    code == CODE_NEW
}
