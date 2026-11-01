mod lindera;
mod mecab;
mod sudachi;
mod vibrato;

use crate::acc::Acc;

pub use self::lindera::Lindera;
pub use self::mecab::MecabRs;
pub use self::sudachi::{SDict, Sudachi};
pub use self::vibrato::Vibrato;

pub trait Engine {
    fn surface(&mut self, line: &str) -> Acc;
    fn feature(&mut self, line: &str) -> Acc;
    fn dump(&mut self, line: &str, out: &mut String);
}
