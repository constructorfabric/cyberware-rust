use toolkit_macros::temporary;

#[temporary(tracking = "gears-rust#abc", reason = "issue number is not numeric")]
pub struct X;

fn main() {}
